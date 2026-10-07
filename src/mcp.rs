//! MCP client support: reads server configs and exposes each server's tools as
//! [`Tool`]s named `mcp__<server>__<tool>`. Servers are either local processes
//! (stdio) or remote streamable-HTTP endpoints, optionally signed in with OAuth.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotificationParam, ClientRequest, ContentBlock,
    ResourceContents, ServerResult,
};
use rmcp::service::{ClientInitializeError, PeerRequestOptions, RunningService};
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    StoredCredentials,
};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{IntoTransport, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{Peer, RoleClient, ServiceExt};
use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::tools::{Tool, ToolCtx, ToolOutput};
use crate::types::ToolSpec;

const CALL_TIMEOUT: Duration = Duration::from_secs(600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// `<home>/mcp.json` — trusted, connected at startup.
    Profile,
    /// `<cwd>/.mcp.json` — connected only on explicit `/mcp connect`.
    Project,
}

#[derive(Clone)]
pub enum Transport {
    Stdio { command: String, args: Vec<String>, env: BTreeMap<String, String>, cwd: Option<PathBuf> },
    /// Streamable HTTP. `headers` are already env-expanded and may hold secrets.
    Http { url: String, headers: BTreeMap<String, String> },
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Transport::Stdio { command, args, cwd, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env", &env.keys().collect::<Vec<_>>())
                .field("cwd", cwd)
                .finish(),
            Transport::Http { url, headers } => {
                f.debug_struct("Http").field("url", url).field("headers", &headers.keys().collect::<Vec<_>>()).finish()
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub name: String,
    pub transport: Transport,
    pub scope: Scope,
}

/// Read `{"mcpServers": {name: {...}}}` from the profile and project files.
/// Stdio entries have `command`/`args`/`env`/`cwd`; HTTP entries have `url`,
/// optional `headers` (values may use `${ENV}`) and `bearer_token_env`.
/// Missing files are not an error; malformed ones are reported in the second
/// tuple element as warnings.
pub fn load_configs(home: &Path, cwd: &Path) -> (Vec<ServerConfig>, Vec<String>) {
    let mut servers = Vec::new();
    let mut warnings = Vec::new();
    for (path, scope) in [(home.join("mcp.json"), Scope::Profile), (cwd.join(".mcp.json"), Scope::Project)] {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warnings.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        if let Err(e) = parse_file(&text, &path, scope, &mut servers, &mut warnings) {
            warnings.push(format!("{}: {e}", path.display()));
        }
    }
    (servers, warnings)
}

fn parse_file(
    text: &str,
    path: &Path,
    scope: Scope,
    servers: &mut Vec<ServerConfig>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let root: Value = serde_json::from_str(text).context("invalid JSON")?;
    let Some(entries) = root.get("mcpServers") else { return Ok(()) };
    let entries = entries.as_object().ok_or_else(|| anyhow!("\"mcpServers\" must be an object"))?;
    let base = path.parent().unwrap_or(Path::new("."));
    for (name, entry) in entries {
        match parse_server(entry, base) {
            Ok(transport) => servers.push(ServerConfig { name: name.clone(), transport, scope }),
            Err(e) => warnings.push(format!("{}: server '{name}': {e:#}", path.display())),
        }
    }
    Ok(())
}

fn parse_server(v: &Value, base: &Path) -> Result<Transport> {
    let obj = v.as_object().ok_or_else(|| anyhow!("must be an object"))?;
    match obj.get("type").and_then(Value::as_str) {
        Some("sse") => bail!("skipped: the legacy SSE transport is not supported (use a streamable HTTP endpoint)"),
        Some("http" | "streamable-http" | "streamable_http" | "streamableHttp") => parse_http(obj),
        None if obj.contains_key("url") => parse_http(obj),
        None | Some("stdio") => parse_stdio(obj, base),
        Some(other) => bail!("skipped: unknown transport type \"{other}\""),
    }
}

fn parse_stdio(obj: &Map<String, Value>, base: &Path) -> Result<Transport> {
    let command = obj
        .get("command")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| anyhow!("missing \"command\""))?;
    let args = match obj.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().map(String::from).ok_or_else(|| anyhow!("\"args\" must be strings")))
            .collect::<Result<_>>()?,
        Some(_) => bail!("\"args\" must be an array"),
    };
    let env = match obj.get("env") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, x)| match x {
                Value::String(s) => Ok((k.clone(), s.clone())),
                Value::Number(_) | Value::Bool(_) => Ok((k.clone(), x.to_string())),
                _ => Err(anyhow!("\"env.{k}\" must be a string")),
            })
            .collect::<Result<_>>()?,
        Some(_) => bail!("\"env\" must be an object"),
    };
    let cwd = match obj.get("cwd") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(base.join(s)),
        Some(_) => bail!("\"cwd\" must be a string"),
    };
    Ok(Transport::Stdio { command: command.to_string(), args, env, cwd })
}

fn parse_http(obj: &Map<String, Value>) -> Result<Transport> {
    let url = obj.get("url").and_then(Value::as_str).ok_or_else(|| anyhow!("missing \"url\""))?;
    let parsed = reqwest::Url::parse(url).map_err(|e| anyhow!("invalid \"url\": {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("\"url\" must be http:// or https://");
    }
    let mut headers = BTreeMap::new();
    match obj.get("headers") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            for (k, x) in m {
                let raw = x.as_str().ok_or_else(|| anyhow!("\"headers.{k}\" must be a string"))?;
                let value = expand_env(raw).with_context(|| format!("\"headers.{k}\""))?;
                HeaderName::from_bytes(k.as_bytes()).map_err(|_| anyhow!("invalid header name \"{k}\""))?;
                HeaderValue::from_str(&value).map_err(|_| anyhow!("invalid value for header \"{k}\""))?;
                headers.insert(k.clone(), value);
            }
        }
        Some(_) => bail!("\"headers\" must be an object"),
    }
    match obj.get("bearer_token_env") {
        None | Some(Value::Null) => {}
        Some(Value::String(var)) => {
            let token = std::env::var(var).map_err(|_| anyhow!("environment variable {var} is not set"))?;
            headers.retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
            headers.insert("Authorization".into(), format!("Bearer {}", token.trim()));
        }
        Some(_) => bail!("\"bearer_token_env\" must be a string"),
    }
    Ok(Transport::Http { url: url.to_string(), headers })
}

/// Replace `${VAR}` with the environment variable's value; unset is an error.
fn expand_env(s: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let end = rest[start..].find('}').ok_or_else(|| anyhow!("unterminated ${{...}}"))? + start;
        let var = &rest[start + 2..end];
        out.push_str(&rest[..start]);
        out.push_str(&std::env::var(var).map_err(|_| anyhow!("environment variable {var} is not set"))?);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

struct Server {
    service: RunningService<RoleClient, ()>,
    tools: usize,
}

pub struct McpManager {
    servers: BTreeMap<String, Server>,
    /// Profile directory; OAuth credentials live in `<home>/nyq/mcp-auth/`.
    home: PathBuf,
}

impl McpManager {
    pub fn new(home: PathBuf) -> Self {
        Self { servers: BTreeMap::new(), home }
    }

    fn credentials(&self, name: &str) -> FileCredentialStore {
        let file: String =
            name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        FileCredentialStore::new(self.home.join("nyq").join("mcp-auth").join(format!("{file}.json")))
    }

    /// Start (or reconnect to) the server and return its tools.
    pub async fn connect(&mut self, cfg: &ServerConfig) -> Result<Vec<Box<dyn Tool>>> {
        if self.servers.contains_key(&cfg.name) {
            self.disconnect(&cfg.name).await?;
        }
        let service = match &cfg.transport {
            Transport::Stdio { command, args, env, cwd } => {
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args).envs(env);
                if let Some(dir) = cwd {
                    cmd.current_dir(dir);
                }
                let (transport, _) = TokioChildProcess::builder(cmd)
                    .stderr(Stdio::null())
                    .spawn()
                    .with_context(|| format!("starting MCP server '{}' ({command})", cfg.name))?;
                initialize(transport).await.map_err(|e| init_error(&cfg.name, e, false))?
            }
            Transport::Http { url, headers } => self.connect_http(&cfg.name, url, headers).await?,
        };
        self.register(cfg, service).await
    }

    async fn connect_http(
        &self,
        name: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
    ) -> Result<RunningService<RoleClient, ()>> {
        let config = http_config(url, headers)?;
        let explicit = headers.keys().any(|k| k.eq_ignore_ascii_case("authorization"));
        let store = self.credentials(name);
        let result = if !explicit && store.path.exists() {
            let mut manager = AuthorizationManager::new(url).await?;
            manager.set_credential_store(store);
            let signed_in = tokio::time::timeout(CONNECT_TIMEOUT, manager.initialize_from_store())
                .await
                .map_err(|_| anyhow!("MCP server '{name}': OAuth discovery timed out"))?
                .with_context(|| format!("MCP server '{name}': loading saved sign-in"))?;
            if signed_in {
                let client = AuthClient::new(reqwest::Client::new(), manager);
                initialize(StreamableHttpClientTransport::with_client(client, config)).await
            } else {
                initialize(StreamableHttpClientTransport::with_client(reqwest::Client::new(), config)).await
            }
        } else {
            initialize(StreamableHttpClientTransport::with_client(reqwest::Client::new(), config)).await
        };
        result.map_err(|e| init_error(name, e, explicit))
    }

    async fn register(&mut self, cfg: &ServerConfig, service: RunningService<RoleClient, ()>) -> Result<Vec<Box<dyn Tool>>> {
        let listed = match tokio::time::timeout(CONNECT_TIMEOUT, service.list_all_tools()).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                let _ = service.cancel().await;
                return Err(anyhow!("listing tools of MCP server '{}': {e}", cfg.name));
            }
            Err(_) => {
                let _ = service.cancel().await;
                return Err(anyhow!("MCP server '{}' did not list its tools within 30s", cfg.name));
            }
        };
        let peer = service.peer().clone();
        let tools: Vec<Box<dyn Tool>> = listed
            .into_iter()
            .map(|t| {
                let mut schema = Value::Object(t.input_schema.as_ref().clone());
                if let Value::Object(m) = &mut schema {
                    m.entry("type").or_insert_with(|| "object".into());
                }
                Box::new(McpTool {
                    name: tool_name(&cfg.name, &t.name),
                    description: t.description.as_deref().unwrap_or_default().to_string(),
                    schema,
                    tool: t.name.to_string(),
                    peer: peer.clone(),
                }) as Box<dyn Tool>
            })
            .collect();
        self.servers.insert(cfg.name.clone(), Server { service, tools: tools.len() });
        Ok(tools)
    }

    /// Sign in to an HTTP server with OAuth (discovery, dynamic client
    /// registration as "nyq", PKCE, loopback redirect), save the credentials
    /// and connect. `open` receives the authorization URL to show the user.
    pub async fn login(&mut self, cfg: &ServerConfig, open: impl FnOnce(&str)) -> Result<Vec<Box<dyn Tool>>> {
        let Transport::Http { url, headers } = &cfg.transport else {
            bail!("MCP server '{}' is not an HTTP server; only HTTP servers support sign-in", cfg.name);
        };
        if self.servers.contains_key(&cfg.name) {
            self.disconnect(&cfg.name).await?;
        }
        // Probe without saved credentials to learn the server's auth challenge.
        let probe = initialize(StreamableHttpClientTransport::with_client(reqwest::Client::new(), http_config(url, headers)?));
        let challenge = match probe.await {
            Ok(service) => return self.register(cfg, service).await, // no sign-in needed
            Err(Init::Failed(e)) if is_unauthorized(&e) => e.auth_challenge().map(String::from),
            Err(e) => return Err(init_error(&cfg.name, e, false)),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.context("opening the sign-in callback listener")?;
        let port = listener.local_addr()?.port();
        let redirect = format!("http://127.0.0.1:{port}/callback");
        let mut manager = AuthorizationManager::new(url.as_str()).await?;
        manager.set_credential_store(self.credentials(&cfg.name));
        let metadata = manager
            .resolve_metadata_from_challenge(challenge.as_deref())
            .await
            .context("discovering the server's OAuth configuration")?;
        manager.set_metadata(metadata.metadata);
        let session = AuthorizationSession::new(manager, AuthorizationRequest::new(&redirect).with_client_name("nyq"))
            .await
            .map_err(|(_, e)| anyhow!("starting sign-in: {e}"))?;
        open(session.get_authorization_url());
        let callback = tokio::select! {
            r = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_callback(&listener, port)) => {
                r.map_err(|_| anyhow!("sign-in timed out after 5 minutes"))??
            }
            _ = tokio::signal::ctrl_c() => bail!("sign-in cancelled"),
        };
        drop(listener);
        session.handle_callback_url(&callback).await.context("completing sign-in")?;
        self.connect(cfg).await
    }

    /// Forget saved OAuth credentials (disconnecting first). Returns whether
    /// any were stored.
    pub async fn logout(&mut self, name: &str) -> Result<bool> {
        if self.servers.contains_key(name) {
            self.disconnect(name).await?;
        }
        let store = self.credentials(name);
        let existed = store.path.exists();
        store.clear().await?;
        Ok(existed)
    }

    pub async fn disconnect(&mut self, name: &str) -> Result<()> {
        let server = self.servers.remove(name).ok_or_else(|| anyhow!("MCP server '{name}' is not connected"))?;
        close(server).await;
        Ok(())
    }

    /// (server name, tool count) for connected servers.
    pub fn connected(&self) -> Vec<(String, usize)> {
        self.servers.iter().map(|(n, s)| (n.clone(), s.tools)).collect()
    }

    pub async fn shutdown(&mut self) {
        let all = std::mem::take(&mut self.servers);
        futures::future::join_all(all.into_values().map(close)).await;
    }
}

fn http_config(url: &str, headers: &BTreeMap<String, String>) -> Result<StreamableHttpClientTransportConfig> {
    let custom: HashMap<HeaderName, HeaderValue> = headers
        .iter()
        .map(|(k, v)| {
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| anyhow!("invalid header name \"{k}\""))?;
            let mut value = HeaderValue::from_str(v).map_err(|_| anyhow!("invalid value for header \"{k}\""))?;
            value.set_sensitive(true);
            Ok((name, value))
        })
        .collect::<Result<_>>()?;
    Ok(StreamableHttpClientTransportConfig::with_uri(url).custom_headers(custom))
}

enum Init {
    Timeout,
    Failed(Box<ClientInitializeError>),
}

async fn initialize<T, E, A>(transport: T) -> std::result::Result<RunningService<RoleClient, ()>, Init>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport)).await {
        Ok(r) => r.map_err(|e| Init::Failed(Box::new(e))),
        Err(_) => Err(Init::Timeout),
    }
}

fn init_error(name: &str, e: Init, explicit_auth: bool) -> anyhow::Error {
    match e {
        Init::Timeout => anyhow!("MCP server '{name}' did not initialize within 30s"),
        Init::Failed(e) if is_unauthorized(&e) && explicit_auth => {
            anyhow!("MCP server '{name}' rejected the configured Authorization header (HTTP 401)")
        }
        Init::Failed(e) if is_unauthorized(&e) => anyhow!("MCP server '{name}' needs sign-in: run /mcp login {name}"),
        Init::Failed(e) => anyhow::Error::new(*e).context(format!("initializing MCP server '{name}'")),
    }
}

/// A 401 from the server, with or without a `WWW-Authenticate` challenge.
fn is_unauthorized(e: &ClientInitializeError) -> bool {
    if e.is_authorization_required() || e.auth_challenge().is_some() {
        return true;
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = source {
        if let Some(r) = err.downcast_ref::<reqwest::Error>() {
            if r.status() == Some(reqwest::StatusCode::UNAUTHORIZED) {
                return true;
            }
        }
        if matches!(err.downcast_ref::<AuthError>(), Some(AuthError::AuthorizationRequired)) {
            return true;
        }
        source = err.source();
    }
    false
}

/// OAuth credentials persisted as JSON, readable only by the user.
#[derive(Clone)]
struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

fn store_err(e: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError(e.to_string())
}

#[async_trait::async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(store_err),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(store_err(e)),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        let path = self.path.clone();
        let bytes = serde_json::to_vec_pretty(&credentials).map_err(store_err)?;
        tokio::task::spawn_blocking(move || write_private(&path, &bytes)).await.map_err(store_err)?.map_err(store_err)
    }

    async fn clear(&self) -> std::result::Result<(), AuthError> {
        match tokio::fs::remove_file(&self.path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(store_err(e)),
            _ => Ok(()),
        }
    }
}

/// Atomically write `bytes` to `path` (file 0600, directory 0700 on unix).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        builder.mode(0o700);
        opts.mode(0o600);
    }
    builder.create(dir)?;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut f = opts.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Open `url` in the user's browser, ignoring failures (the URL is printed too).
pub fn open_browser(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", "start", ""]);
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = cmd.arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

#[derive(Debug, PartialEq)]
enum Callback {
    /// The request target (`/callback?code=..&state=..`).
    Code(String),
    Denied(String),
    Other,
}

/// Classify the request line of an HTTP request hitting the loopback listener.
fn parse_callback(request_line: &str) -> Callback {
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else { return Callback::Other };
    let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else { return Callback::Other };
    if url.path() != "/callback" {
        return Callback::Other;
    }
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    if let Some(err) = q.get("error") {
        let desc = q.get("error_description").map(|d| format!(": {d}")).unwrap_or_default();
        return Callback::Denied(format!("{err}{desc}"));
    }
    if q.contains_key("code") && q.contains_key("state") {
        Callback::Code(target.to_string())
    } else {
        Callback::Denied("the callback is missing code or state".into())
    }
}

/// Serve the loopback redirect until the authorization server sends the
/// browser back; returns the full callback URL.
async fn wait_for_callback(listener: &TcpListener, port: u16) -> Result<String> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(10), async {
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 64 * 1024 {
                let n = sock.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            std::io::Result::Ok(())
        })
        .await;
        if !matches!(read, Ok(Ok(()))) {
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let line = text.lines().next().unwrap_or("");
        let (status, body, result) = match parse_callback(line) {
            Callback::Code(target) => (
                "200 OK",
                "Signed in to the MCP server. You can close this tab and return to nyq.",
                Some(Ok(format!("http://127.0.0.1:{port}{target}"))),
            ),
            Callback::Denied(msg) => ("400 Bad Request", "Sign-in failed. Return to nyq for details.", Some(Err(msg))),
            Callback::Other => ("404 Not Found", "Not found.", None),
        };
        let html = format!("<!doctype html><html><body style=\"font-family:sans-serif\"><p>{body}</p></body></html>");
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.shutdown().await;
        match result {
            Some(Ok(url)) => return Ok(url),
            Some(Err(msg)) => bail!("sign-in failed: {msg}"),
            None => continue,
        }
    }
}

async fn close(mut server: Server) {
    let _ = server.service.close_with_timeout(Duration::from_secs(3)).await;
}

/// `mcp__<server>__<tool>`, restricted to `[A-Za-z0-9_-]` and 64 chars.
fn tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(64)
        .collect()
}

struct McpTool {
    name: String,
    description: String,
    schema: Value,
    /// The tool's name on the server.
    tool: String,
    peer: Peer<RoleClient>,
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: self.name.clone(), description: self.description.clone(), parameters: self.schema.clone() }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        let s = args.to_string();
        if s.chars().count() > 100 {
            format!("{}…", s.chars().take(99).collect::<String>())
        } else {
            s
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let arguments = match args {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            other => return ToolOutput::err(format!("Arguments must be a JSON object, got {other}")),
        };
        let params = CallToolRequestParams::new(self.tool.clone()).with_arguments(arguments);
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let handle =
            match self.peer.send_request_with_option(request, PeerRequestOptions::with_timeout(CALL_TIMEOUT)).await {
                Ok(h) => h,
                Err(e) => return ToolOutput::err(format!("MCP request failed: {e}")),
            };
        let id = handle.id.clone();
        tokio::select! {
            res = handle.await_response() => match res {
                Ok(ServerResult::CallToolResult(r)) => convert(r),
                Ok(_) => ToolOutput::err("MCP server sent an unexpected response (input requests and tasks are not supported)."),
                Err(e) => ToolOutput::err(format!("MCP tool call failed: {e}")),
            },
            _ = ctx.cancel.cancelled() => {
                let note = CancelledNotificationParam::new(Some(id), Some("cancelled by user".into()));
                let _ = self.peer.notify_cancelled(note).await;
                ToolOutput::err("Cancelled by user.")
            }
        }
    }
}

fn convert(r: CallToolResult) -> ToolOutput {
    let parts: Vec<String> = r
        .content
        .iter()
        .map(|c| match c {
            ContentBlock::Text(t) => t.text.clone(),
            ContentBlock::Image(i) => format!("[{}]", i.mime_type),
            ContentBlock::Audio(a) => format!("[{}]", a.mime_type),
            ContentBlock::Resource(e) => match &e.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => format!("[resource {uri}]\n{text}"),
                ResourceContents::BlobResourceContents { uri, mime_type, .. } => {
                    format!("[resource {uri} ({})]", mime_type.as_deref().unwrap_or("binary"))
                }
                #[allow(unreachable_patterns)]
                _ => "[resource]".into(),
            },
            ContentBlock::ResourceLink(l) => format!("[resource link {}]", l.uri),
            #[allow(unreachable_patterns)]
            _ => "[unsupported content]".into(),
        })
        .collect();
    let has_text = r.content.iter().any(|c| matches!(c, ContentBlock::Text(_)));
    let mut content = parts.join("\n");
    if !has_text {
        if let Some(s) = &r.structured_content {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&s.to_string());
        }
    }
    ToolOutput { content, is_error: r.is_error.unwrap_or(false), display: None, images: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn configs_and_warnings() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::env::set_var("NYQ_MCP_TEST_TOKEN", "tok123");
        std::env::set_var("NYQ_MCP_TEST_TEAM", "core");
        std::fs::write(
            home.path().join("mcp.json"),
            r#"{"mcpServers": {
                "fs": {"command": "npx", "args": ["-y", "server-fs"], "env": {"A": "1"}, "cwd": "sub"},
                "web": {"type": "http", "url": "https://example.com/mcp",
                        "headers": {"X-Team": "team-${NYQ_MCP_TEST_TEAM}!"}, "bearer_token_env": "NYQ_MCP_TEST_TOKEN"},
                "plain": {"url": "http://localhost:1/mcp"},
                "legacy": {"type": "sse", "url": "https://example.com/sse"},
                "unset": {"url": "https://example.com/mcp", "headers": {"X": "${NYQ_MCP_TEST_UNSET_VAR}"}},
                "badurl": {"type": "streamable-http", "url": "ftp://x"},
                "bad": {"args": []}
            }}"#,
        )
        .unwrap();
        std::fs::write(cwd.path().join(".mcp.json"), "{ nope").unwrap();
        let (servers, warnings) = load_configs(home.path(), cwd.path());
        let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["fs", "plain", "web"]);
        let Transport::Stdio { command, args, env, cwd: dir } = &servers[0].transport else { panic!() };
        assert_eq!((command.as_str(), servers[0].scope), ("npx", Scope::Profile));
        assert_eq!(args, &["-y", "server-fs"]);
        assert_eq!(env["A"], "1");
        assert_eq!(dir.as_deref(), Some(home.path().join("sub").as_path()));
        let Transport::Http { url, headers } = &servers[2].transport else { panic!() };
        assert_eq!(url, "https://example.com/mcp");
        assert_eq!(headers["X-Team"], "team-core!");
        assert_eq!(headers["Authorization"], "Bearer tok123");
        assert!(!format!("{:?}", servers[2]).contains("tok123"), "Debug must not leak header values");
        let Transport::Http { headers, .. } = &servers[1].transport else { panic!() };
        assert!(headers.is_empty());

        assert_eq!(warnings.len(), 5, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("'legacy'") && w.contains("SSE")));
        assert!(warnings.iter().any(|w| w.contains("'unset'") && w.contains("NYQ_MCP_TEST_UNSET_VAR")));
        assert!(warnings.iter().any(|w| w.contains("'badurl'") && w.contains("http")));
        assert!(warnings.iter().any(|w| w.contains("'bad'") && w.contains("command")));
        assert!(warnings.iter().any(|w| w.contains(".mcp.json") && w.contains("invalid JSON")));

        std::fs::write(cwd.path().join(".mcp.json"), r#"{"mcpServers": {"p": {"command": "x"}, "h": {"url": "https://h/mcp"}}}"#)
            .unwrap();
        let (servers, _) = load_configs(Path::new("/nonexistent"), cwd.path());
        assert_eq!(servers.len(), 2);
        assert!(servers.iter().all(|s| s.scope == Scope::Project));
    }

    #[test]
    fn env_expansion() {
        std::env::set_var("NYQ_MCP_TEST_X", "1");
        assert_eq!(expand_env("a${NYQ_MCP_TEST_X}b${NYQ_MCP_TEST_X}").unwrap(), "a1b1");
        assert_eq!(expand_env("no vars $X").unwrap(), "no vars $X");
        assert!(expand_env("${NYQ_MCP_TEST_NOPE_NOPE}").is_err());
        assert!(expand_env("${OPEN").is_err());
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(tool_name("my server", "do.it"), "mcp__my_server__do_it");
        assert_eq!(tool_name("s", &"x".repeat(100)).len(), 64);
    }

    const FAKE_SERVER: &str = r#"
import json, sys, time
for line in sys.stdin:
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    if mid is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "0.1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "Echo the arguments",
                             "inputSchema": {"properties": {"text": {"type": "string"}}}}]}
    elif method == "tools/call":
        args = msg["params"].get("arguments", {})
        time.sleep(args.get("sleep", 0))
        result = {"content": [{"type": "text", "text": json.dumps(args, sort_keys=True)},
                              {"type": "image", "data": "", "mimeType": "image/png"}],
                  "isError": args.get("fail", False)}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "nope"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}), flush=True)
"#;

    #[tokio::test]
    async fn connect_list_call() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("server.py");
        std::fs::write(&script, FAKE_SERVER).unwrap();
        let cfg = ServerConfig {
            name: "fake".into(),
            transport: Transport::Stdio {
                command: if cfg!(windows) { "python" } else { "python3" }.into(),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
                cwd: None,
            },
            scope: Scope::Profile,
        };
        let mut mgr = McpManager::new(dir.path().into());
        let tools = mgr.connect(&cfg).await.unwrap();
        assert_eq!(tools.len(), 1);
        let spec = tools[0].spec();
        assert_eq!(spec.name, "mcp__fake__echo");
        assert_eq!(spec.description, "Echo the arguments");
        assert_eq!(spec.parameters["type"], "object");
        assert!(tools[0].needs_approval(&Value::Null));

        let ctx = ToolCtx { cwd: dir.path().into(), cancel: CancellationToken::new(), ..Default::default() };
        let out = tools[0].run(serde_json::json!({"text": "hi"}), &ctx).await;
        assert_eq!(out.content, "{\"text\": \"hi\"}\n[image/png]");
        assert!(!out.is_error);
        let out = tools[0].run(serde_json::json!({"fail": true}), &ctx).await;
        assert!(out.is_error);

        let cancel = CancellationToken::new();
        let slow = ToolCtx { cwd: dir.path().into(), cancel: cancel.clone(), ..Default::default() };
        let started = std::time::Instant::now();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let out = tools[0].run(serde_json::json!({"sleep": 5}), &slow).await;
        assert!(out.is_error && out.content.contains("Cancelled"));
        assert!(started.elapsed() < Duration::from_secs(2));

        // Reconnecting replaces the old client.
        let tools = mgr.connect(&cfg).await.unwrap();
        assert_eq!(mgr.connected(), vec![("fake".to_string(), 1)]);
        let out = tools[0].run(serde_json::json!({}), &ctx).await;
        assert_eq!(out.content, "{}\n[image/png]");

        mgr.disconnect("fake").await.unwrap();
        assert!(mgr.connected().is_empty());
        assert!(mgr.disconnect("fake").await.is_err());
        let out = tools[0].run(serde_json::json!({}), &ctx).await;
        assert!(out.is_error);
        mgr.shutdown().await;
    }

    // ---- streamable HTTP + OAuth -------------------------------------------------

    /// A tiny hand-rolled streamable-HTTP MCP server, plus (when `oauth`) the
    /// protected-resource / authorization-server endpoints of a fake IdP.
    /// Accepts the bearer tokens `static-token` and `good-token`.
    async fn fake_http_server(protected: bool, oauth: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let b = base.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { return };
                let base = b.clone();
                tokio::spawn(async move {
                    let _ = serve_conn(sock, &base, protected, oauth).await;
                });
            }
        });
        base
    }

    async fn serve_conn(mut sock: tokio::net::TcpStream, base: &str, protected: bool, oauth: bool) -> std::io::Result<()> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let n = sock.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.lines();
        let mut first = lines.next().unwrap_or("").split_whitespace();
        let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
        let headers: HashMap<String, String> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        while buf.len() < head_end + len {
            let n = sock.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
        let url = reqwest::Url::parse(&format!("{base}{target}")).unwrap();
        let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
        let json = |v: Value| (200, vec![("Content-Type".to_string(), "application/json".to_string())], v.to_string());
        let (status, extra, out): (u16, Vec<(String, String)>, String) = match (method.as_str(), url.path()) {
            ("POST", "/mcp") => {
                let auth = headers.get("authorization").map(String::as_str);
                if protected && !matches!(auth, Some("Bearer static-token" | "Bearer good-token")) {
                    let challenge = if oauth {
                        format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\"")
                    } else {
                        "Bearer".to_string()
                    };
                    (401, vec![("WWW-Authenticate".into(), challenge)], String::new())
                } else {
                    let msg: Value = serde_json::from_str(&body).unwrap_or_default();
                    match msg.get("id") {
                        None => (202, vec![], String::new()),
                        Some(id) => {
                            let result = match msg["method"].as_str().unwrap_or("") {
                                "initialize" => serde_json::json!({
                                    "protocolVersion": msg["params"]["protocolVersion"],
                                    "capabilities": {"tools": {}},
                                    "serverInfo": {"name": "fake-http", "version": "0.1"}}),
                                "tools/list" => serde_json::json!({"tools": [{"name": "echo", "description": "Echo",
                                    "inputSchema": {"type": "object"}}]}),
                                "tools/call" => serde_json::json!({"content": [{"type": "text", "text": serde_json::json!({
                                    "args": msg["params"]["arguments"], "x_foo": headers.get("x-foo")}).to_string()}]}),
                                _ => Value::Null,
                            };
                            json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}))
                        }
                    }
                }
            }
            ("GET", p) if p.starts_with("/.well-known/oauth-protected-resource") && oauth => {
                json(serde_json::json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]}))
            }
            ("GET", "/.well-known/oauth-authorization-server") if oauth => json(serde_json::json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"],
            })),
            ("POST", "/register") if oauth => {
                let req: Value = serde_json::from_str(&body).unwrap_or_default();
                assert_eq!(req["client_name"], "nyq");
                let mut r = json(serde_json::json!({"client_id": "nyq-client", "redirect_uris": req["redirect_uris"]}));
                r.0 = 201;
                r
            }
            ("GET", "/authorize") if oauth => {
                assert_eq!(query.get("code_challenge_method").map(String::as_str), Some("S256"));
                assert_eq!(query.get("client_id").map(String::as_str), Some("nyq-client"));
                let location = format!("{}?code=the-code&state={}", query["redirect_uri"], query["state"]);
                (302, vec![("Location".into(), location)], String::new())
            }
            ("POST", "/token") if oauth => {
                let form: HashMap<String, String> = reqwest::Url::parse(&format!("http://x/?{body}"))
                    .unwrap()
                    .query_pairs()
                    .into_owned()
                    .collect();
                if form.get("code").map(String::as_str) == Some("the-code") && form.contains_key("code_verifier") {
                    json(serde_json::json!({"access_token": "good-token", "token_type": "Bearer",
                        "expires_in": 3600, "refresh_token": "refresh-1"}))
                } else {
                    (400, vec![("Content-Type".into(), "application/json".into())], r#"{"error":"invalid_grant"}"#.into())
                }
            }
            ("GET" | "DELETE", "/mcp") => (405, vec![], String::new()),
            _ => (404, vec![], String::new()),
        };
        let mut resp = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", out.len());
        for (k, v) in extra {
            resp.push_str(&format!("{k}: {v}\r\n"));
        }
        resp.push_str("\r\n");
        resp.push_str(&out);
        sock.write_all(resp.as_bytes()).await?;
        sock.shutdown().await
    }

    fn http_cfg(name: &str, url: String, headers: &[(&str, &str)]) -> ServerConfig {
        let headers = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        ServerConfig { name: name.into(), transport: Transport::Http { url, headers }, scope: Scope::Profile }
    }

    #[tokio::test]
    async fn http_connect_list_call() {
        let home = tempfile::tempdir().unwrap();
        let ctx = ToolCtx { cwd: home.path().into(), cancel: CancellationToken::new(), ..Default::default() };
        let mut mgr = McpManager::new(home.path().into());

        let base = fake_http_server(false, false).await;
        let cfg = http_cfg("open", format!("{base}/mcp"), &[("X-Foo", "bar")]);
        let tools = mgr.connect(&cfg).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].spec().name, "mcp__open__echo");
        let out = tools[0].run(serde_json::json!({"a": 1}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v, serde_json::json!({"args": {"a": 1}, "x_foo": "bar"}));

        // Protected without credentials: a 401 asks for sign-in.
        let base = fake_http_server(true, false).await;
        let err = mgr.connect(&http_cfg("locked", format!("{base}/mcp"), &[])).await.err().unwrap();
        assert!(format!("{err:#}").contains("needs sign-in: run /mcp login locked"), "{err:#}");
        // A wrong static bearer is reported as such.
        let bad = http_cfg("locked", format!("{base}/mcp"), &[("Authorization", "Bearer nope")]);
        let err = mgr.connect(&bad).await.err().unwrap();
        assert!(format!("{err:#}").contains("rejected the configured Authorization"), "{err:#}");
        // A correct static bearer works.
        let good = http_cfg("locked", format!("{base}/mcp"), &[("Authorization", "Bearer static-token")]);
        let tools = mgr.connect(&good).await.unwrap();
        assert!(!tools[0].run(serde_json::json!({}), &ctx).await.is_error);
        assert_eq!(mgr.connected().len(), 2);
        mgr.shutdown().await;

        // Login is only for HTTP servers.
        let stdio = ServerConfig {
            name: "s".into(),
            transport: Transport::Stdio { command: "true".into(), args: vec![], env: BTreeMap::new(), cwd: None },
            scope: Scope::Profile,
        };
        assert!(mgr.login(&stdio, |_| {}).await.is_err());
    }

    #[tokio::test]
    async fn oauth_login_flow() {
        let home = tempfile::tempdir().unwrap();
        let ctx = ToolCtx { cwd: home.path().into(), cancel: CancellationToken::new(), ..Default::default() };
        let mut mgr = McpManager::new(home.path().into());
        let base = fake_http_server(true, true).await;
        let cfg = http_cfg("idp server", format!("{base}/mcp"), &[]);

        let err = mgr.connect(&cfg).await.err().unwrap();
        assert!(format!("{err:#}").contains("needs sign-in: run /mcp login idp server"), "{err:#}");

        // The "browser" just follows the authorization URL's redirects to the loopback callback.
        let page = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let p = page.clone();
        let tools = mgr
            .login(&cfg, move |url| {
                assert!(url.starts_with(&format!("{base}/authorize?")), "{url}");
                let url = url.to_string();
                tokio::spawn(async move {
                    let body = reqwest::get(url).await.unwrap().text().await.unwrap();
                    *p.lock().await = body;
                });
            })
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert!(!tools[0].run(serde_json::json!({}), &ctx).await.is_error);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(page.lock().await.contains("You can close this tab"));

        let file = home.path().join("nyq/mcp-auth/idp_server.json");
        let saved: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(saved["client_id"], "nyq-client");
        assert_eq!(saved["token_response"]["access_token"], "good-token");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(file.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }

        // Saved credentials are used on the next connect.
        mgr.shutdown().await;
        let tools = mgr.connect(&cfg).await.unwrap();
        assert!(!tools[0].run(serde_json::json!({}), &ctx).await.is_error);

        assert!(mgr.logout("idp server").await.unwrap());
        assert!(mgr.connected().is_empty() && !file.exists());
        assert!(!mgr.logout("idp server").await.unwrap());
        let err = mgr.connect(&cfg).await.err().unwrap();
        assert!(format!("{err:#}").contains("needs sign-in"), "{err:#}");
    }

    #[tokio::test]
    async fn credential_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(dir.path().join("a/b/s.json"));
        assert!(store.load().await.unwrap().is_none());
        store.save(StoredCredentials::new("cid".into(), None, vec!["x".into()], Some(7))).await.unwrap();
        let back = store.load().await.unwrap().unwrap();
        assert_eq!((back.client_id.as_str(), back.granted_scopes.as_slice(), back.token_received_at), ("cid", &["x".to_string()][..], Some(7)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(dir.path().join("a/b")).unwrap().permissions().mode() & 0o777, 0o700);
        }
        store.save(StoredCredentials::new("cid2".into(), None, vec![], None)).await.unwrap();
        assert_eq!(store.load().await.unwrap().unwrap().client_id, "cid2");
        store.clear().await.unwrap();
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        std::fs::write(&store.path, "garbage").unwrap();
        assert!(matches!(store.load().await, Err(AuthError::CredentialStoreError(_))));
    }

    #[test]
    fn callback_parsing() {
        assert_eq!(
            parse_callback("GET /callback?code=abc&state=xyz HTTP/1.1"),
            Callback::Code("/callback?code=abc&state=xyz".into())
        );
        assert_eq!(
            parse_callback("GET /callback?error=access_denied&error_description=no%20way HTTP/1.1"),
            Callback::Denied("access_denied: no way".into())
        );
        assert!(matches!(parse_callback("GET /callback?code=abc HTTP/1.1"), Callback::Denied(_)));
        assert_eq!(parse_callback("GET /favicon.ico HTTP/1.1"), Callback::Other);
        assert_eq!(parse_callback("POST /callback?code=a&state=b HTTP/1.1"), Callback::Other);
        assert_eq!(parse_callback(""), Callback::Other);
    }

    #[tokio::test]
    async fn callback_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = tokio::spawn(async move {
            let miss = reqwest::get(format!("http://127.0.0.1:{port}/favicon.ico")).await.unwrap();
            assert_eq!(miss.status(), 404);
            reqwest::get(format!("http://127.0.0.1:{port}/callback?code=c&state=s")).await.unwrap().text().await.unwrap()
        });
        let url = wait_for_callback(&listener, port).await.unwrap();
        assert_eq!(url, format!("http://127.0.0.1:{port}/callback?code=c&state=s"));
        assert!(client.await.unwrap().contains("close this tab"));

        let client = tokio::spawn(async move { reqwest::get(format!("http://127.0.0.1:{port}/callback?error=denied")).await });
        let err = wait_for_callback(&listener, port).await.unwrap_err();
        assert!(err.to_string().contains("denied"));
        assert_eq!(client.await.unwrap().unwrap().status(), 400);
    }
}
