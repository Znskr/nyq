//! MCP client support: reads server configs and exposes each server's tools as
//! [`Tool`]s named `mcp__<server>__<tool>`. Servers are local processes
//! (stdio), remote streamable-HTTP endpoints (optionally signed in with
//! OAuth) or legacy HTTP+SSE endpoints.
//!
//! The manager is shared: servers connect in the background, and tool-list
//! changes reach the agent's [`Registry`](crate::tools::Registry) as queued
//! patches applied before the next model request.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotificationParam, ClientRequest, ContentBlock,
    ProgressNotificationParam, ProgressToken, ResourceContents, ServerResult,
};
use rmcp::service::{ClientInitializeError, PeerRequestOptions, RunningService};
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    StoredCredentials, WWWAuthenticateParams,
};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{IntoTransport, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{Peer, RoleClient, ServiceExt};
use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use tokio_util::sync::CancellationToken;

use crate::mcp_client::{Client, Elicitor, TerminalElicitor};
use crate::tools::{LiveTools, Tool, ToolCtx, ToolOutput, ToolPatch};
use crate::types::ToolSpec;

// Resolve batch-file launchers explicitly: Windows process lookup only adds .exe.
#[cfg(any(windows, test))]
fn resolve_launcher(command: &str, paths: &[PathBuf], extensions: &str) -> PathBuf {
    // npm ships an extensionless `npx` shell script next to `npx.cmd`; Windows
    // cannot run the former, so PATHEXT matches win. Relative or empty PATH
    // entries would resolve into the workspace and are skipped.
    for dir in paths.iter().filter(|d| d.is_absolute()) {
        if Path::new(command).extension().is_none() {
            for ext in extensions.split(';').filter(|e| e.starts_with('.')) {
                let candidate = dir.join(format!("{command}{ext}"));
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
        let base = dir.join(command);
        if base.is_file() && Path::new(command).extension().is_some() {
            return base;
        }
    }
    command.into()
}

/// Defaults for the per-server `startup_timeout_ms` and `operation_timeout_ms`.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(600);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// Re-listing after a `list_changed` notification.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// The legacy HTTP+SSE transport (an event stream plus a POST endpoint).
    Sse { url: String, headers: BTreeMap<String, String> },
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
            Transport::Http { url, headers } | Transport::Sse { url, headers } => f
                .debug_struct(if matches!(self, Transport::Sse { .. }) { "Sse" } else { "Http" })
                .field("url", url)
                .field("headers", &headers.keys().collect::<Vec<_>>())
                .finish(),
        }
    }
}

/// OAuth settings for one HTTP server (`"oauth": {...}`).
#[derive(Clone, Debug, Default)]
pub struct OAuthOptions {
    /// A client registered with the authorization server ahead of time;
    /// without it nyq registers itself dynamically.
    pub client_id: Option<String>,
    /// Environment variable holding that client's secret, read at sign-in.
    pub client_secret_env: Option<String>,
    /// Scopes to request (in addition to any the server asks for).
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// `false` keeps the server configured but not started.
    pub enabled: bool,
    /// Startup fails when this server cannot connect.
    pub required: bool,
    /// Limit for starting, initializing and listing tools.
    pub startup_timeout: Duration,
    /// Limit for one tool call (resource and prompt requests use at most 60s).
    pub operation_timeout: Duration,
    /// How often a server that exits is started again (on the next call).
    pub restart_limit: u32,
    pub oauth: OAuthOptions,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            required: false,
            startup_timeout: STARTUP_TIMEOUT,
            operation_timeout: OPERATION_TIMEOUT,
            restart_limit: 1,
            oauth: OAuthOptions::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub name: String,
    pub transport: Transport,
    pub scope: Scope,
    pub options: ServerOptions,
}

impl ServerConfig {
    pub fn new(name: impl Into<String>, transport: Transport, scope: Scope) -> Self {
        Self { name: name.into(), transport, scope, options: ServerOptions::default() }
    }
}

/// Read `{"mcpServers": {name: {...}}}` (or `{"mcp": {...}}`) from the profile
/// and project files. Stdio entries have `command` (a string or an array)/
/// `args`/`env` (or `environment`)/`cwd`; HTTP and SSE entries have `url`,
/// optional `headers`, `header_env` and `bearer_token_env`. `${VAR}` and
/// `${VAR:-default}` are expanded in commands, arguments, env values, URLs and
/// headers — for project servers only once they are trusted. Missing files
/// are not an error; malformed entries are reported in the second tuple
/// element as warnings.
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
        let trusted = match scope {
            Scope::Profile => HashSet::new(),
            Scope::Project => crate::mcp_cli::trusted_project(home, cwd),
        };
        let start = servers.len();
        if let Err(e) = parse_file(&text, &path, scope, &trusted, &mut servers, &mut warnings) {
            warnings.push(format!("{}: {e}", path.display()));
        }
        // Stable order regardless of JSON map ordering: by name within each file.
        servers[start..].sort_by(|a, b| a.name.cmp(&b.name));
    }
    (servers, warnings)
}

/// The server map of a config file: `mcpServers`, or `mcp` as an alias.
pub fn server_entries(root: &Map<String, Value>) -> Result<Option<&Map<String, Value>>> {
    let (key, entries) = match (root.get("mcpServers"), root.get("mcp")) {
        (Some(e), _) => ("mcpServers", e),
        (None, Some(e)) => ("mcp", e),
        (None, None) => return Ok(None),
    };
    entries.as_object().map(Some).ok_or_else(|| anyhow!("\"{key}\" must be an object"))
}

fn parse_file(
    text: &str,
    path: &Path,
    scope: Scope,
    trusted: &HashSet<String>,
    servers: &mut Vec<ServerConfig>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let root: Value = serde_json::from_str(text).context("invalid JSON")?;
    let root = root.as_object().ok_or_else(|| anyhow!("must contain a JSON object"))?;
    for key in root.keys() {
        let folded = key.to_ascii_lowercase().replace(['_', '-'], "");
        if key != "mcpServers" && matches!(folded.as_str(), "servers" | "mcpservers" | "mcpserver" | "mcpserverconfig") {
            warnings.push(format!("{}: \"{key}\" is not read; did you mean \"mcpServers\"?", path.display()));
        }
    }
    let Some(entries) = server_entries(root)? else { return Ok(()) };
    let base = path.parent().unwrap_or(Path::new("."));
    for (name, entry) in entries {
        let env = Env { trusted: scope == Scope::Profile || trusted.contains(name), name };
        match parse_server(entry, base, &env) {
            Ok((transport, options)) => servers.push(ServerConfig { name: name.clone(), transport, scope, options }),
            Err(e) => warnings.push(format!("{}: server '{name}': {e:#}", path.display())),
        }
    }
    Ok(())
}

/// Reads the environment for a trusted entry. An untrusted project entry may
/// neither read environment variables nor send credentials.
struct Env<'a> {
    trusted: bool,
    name: &'a str,
}

impl Env<'_> {
    fn expand(&self, s: &str) -> Result<String> {
        if self.trusted {
            return expand_env(s);
        }
        if s.contains("${") {
            bail!(
                "skipped: it reads environment variables, which project servers may do only once trusted \
                 (nyq mcp trust approve {})",
                self.name
            );
        }
        Ok(s.to_string())
    }

    fn secret(&self, what: &str) -> Result<()> {
        if !self.trusted {
            bail!("skipped: it sets {what}; project servers may send credentials only once trusted (nyq mcp trust approve {})", self.name);
        }
        Ok(())
    }
}

fn parse_server(v: &Value, base: &Path, env: &Env) -> Result<(Transport, ServerOptions)> {
    let obj = v.as_object().ok_or_else(|| anyhow!("must be an object"))?;
    let options = parse_options(obj)?;
    let transport = match obj.get("type").and_then(Value::as_str) {
        Some("sse") => parse_http(obj, env, true)?,
        Some("http" | "streamable-http" | "streamable_http" | "streamableHttp" | "remote") => parse_http(obj, env, false)?,
        None if obj.contains_key("url") => parse_http(obj, env, false)?,
        None | Some("stdio" | "local") => parse_stdio(obj, base, env)?,
        Some(other) => bail!("skipped: unknown transport type \"{other}\" (use stdio, http or sse)"),
    };
    Ok((transport, options))
}

fn parse_options(obj: &Map<String, Value>) -> Result<ServerOptions> {
    let mut o = ServerOptions::default();
    let flag = |k: &str| match obj.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(anyhow!("\"{k}\" must be true or false")),
    };
    let count = |k: &str| match obj.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| anyhow!("\"{k}\" must be a non-negative whole number")),
    };
    o.enabled = flag("enabled")?.unwrap_or(true);
    o.required = flag("required")?.unwrap_or(false);
    for (key, slot) in [("startup_timeout_ms", &mut o.startup_timeout), ("operation_timeout_ms", &mut o.operation_timeout)] {
        if let Some(ms) = count(key)? {
            anyhow::ensure!(ms > 0, "\"{key}\" must be at least 1");
            *slot = Duration::from_millis(ms);
        }
    }
    if let Some(n) = count("restart_limit")? {
        o.restart_limit = u32::try_from(n).unwrap_or(u32::MAX);
    }
    match obj.get("oauth") {
        None | Some(Value::Null | Value::Bool(_)) => {}
        Some(Value::Object(m)) => {
            let text = |k: &str| match m.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
                Some(_) => Err(anyhow!("\"oauth.{k}\" must be a string")),
            };
            o.oauth.client_id = text("client_id")?;
            o.oauth.client_secret_env = text("client_secret_env")?;
            anyhow::ensure!(
                o.oauth.client_secret_env.is_none() || o.oauth.client_id.is_some(),
                "\"oauth.client_secret_env\" needs \"oauth.client_id\""
            );
            o.oauth.scopes = match m.get("scopes") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::String(s)) => s.split_whitespace().map(String::from).collect(),
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|x| x.as_str().map(String::from).ok_or_else(|| anyhow!("\"oauth.scopes\" must be strings")))
                    .collect::<Result<_>>()?,
                Some(_) => bail!("\"oauth.scopes\" must be an array of strings"),
            };
        }
        Some(_) => bail!("\"oauth\" must be an object"),
    }
    Ok(o)
}

fn strings(v: &[Value], what: &str) -> Result<Vec<String>> {
    v.iter().map(|x| x.as_str().map(String::from).ok_or_else(|| anyhow!("\"{what}\" must be strings"))).collect()
}

fn parse_stdio(obj: &Map<String, Value>, base: &Path, env: &Env) -> Result<Transport> {
    let (command, mut args) = match obj.get("command") {
        Some(Value::String(c)) if !c.is_empty() => (c.clone(), Vec::new()),
        Some(Value::Array(a)) if !a.is_empty() => {
            let mut all = strings(a, "command")?;
            let program = all.remove(0);
            anyhow::ensure!(!program.is_empty(), "missing \"command\"");
            (program, all)
        }
        _ => bail!("missing \"command\""),
    };
    match obj.get("args") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) => args.extend(strings(a, "args")?),
        Some(_) => bail!("\"args\" must be an array"),
    }
    let (key, vars) = match (obj.get("env"), obj.get("environment")) {
        (Some(v), _) => ("env", v),
        (None, Some(v)) => ("environment", v),
        (None, None) => ("env", &Value::Null),
    };
    let mut vars_out = BTreeMap::new();
    match vars {
        Value::Null => {}
        Value::Object(m) => {
            for (k, x) in m {
                let raw = match x {
                    Value::String(s) => s.clone(),
                    Value::Number(_) | Value::Bool(_) => x.to_string(),
                    _ => bail!("\"{key}.{k}\" must be a string"),
                };
                vars_out.insert(k.clone(), env.expand(&raw).with_context(|| format!("\"{key}.{k}\""))?);
            }
        }
        _ => bail!("\"{key}\" must be an object"),
    }
    let cwd = match obj.get("cwd") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(base.join(env.expand(s).context("\"cwd\"")?)),
        Some(_) => bail!("\"cwd\" must be a string"),
    };
    let command = env.expand(&command).context("\"command\"")?;
    let args = args.iter().map(|a| env.expand(a).context("\"args\"")).collect::<Result<_>>()?;
    Ok(Transport::Stdio { command, args, env: vars_out, cwd })
}

fn parse_http(obj: &Map<String, Value>, env: &Env, sse: bool) -> Result<Transport> {
    let raw = obj.get("url").and_then(Value::as_str).ok_or_else(|| anyhow!("missing \"url\""))?;
    let url = env.expand(raw).context("\"url\"")?;
    let parsed = reqwest::Url::parse(&url).map_err(|e| anyhow!("invalid \"url\": {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("\"url\" must be http:// or https://");
    }
    let mut headers = BTreeMap::new();
    let mut add = |k: &str, value: String| -> Result<()> {
        HeaderName::from_bytes(k.as_bytes()).map_err(|_| anyhow!("invalid header name \"{k}\""))?;
        HeaderValue::from_str(&value).map_err(|_| anyhow!("invalid value for header \"{k}\""))?;
        headers.insert(k.to_string(), value);
        Ok(())
    };
    match obj.get("headers") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            for (k, x) in m {
                let raw = x.as_str().ok_or_else(|| anyhow!("\"headers.{k}\" must be a string"))?;
                if k.eq_ignore_ascii_case("authorization") {
                    env.secret("an Authorization header")?;
                }
                add(k, env.expand(raw).with_context(|| format!("\"headers.{k}\""))?)?;
            }
        }
        Some(_) => bail!("\"headers\" must be an object"),
    }
    match obj.get("header_env") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            env.secret("\"header_env\"")?;
            for (k, x) in m {
                let var = x.as_str().ok_or_else(|| anyhow!("\"header_env.{k}\" must name an environment variable"))?;
                let value = std::env::var(var).map_err(|_| anyhow!("environment variable {var} (for header {k}) is not set"))?;
                add(k, value)?;
            }
        }
        Some(_) => bail!("\"header_env\" must be an object"),
    }
    match obj.get("bearer_token_env") {
        None | Some(Value::Null) => {}
        Some(Value::String(var)) => {
            env.secret("\"bearer_token_env\"")?;
            let token = std::env::var(var).map_err(|_| anyhow!("environment variable {var} is not set"))?;
            headers.retain(|k, _| !k.eq_ignore_ascii_case("authorization"));
            headers.insert("Authorization".into(), format!("Bearer {}", token.trim()));
        }
        Some(_) => bail!("\"bearer_token_env\" must be a string"),
    }
    Ok(if sse { Transport::Sse { url, headers } } else { Transport::Http { url, headers } })
}

/// Replace `${VAR}` with the environment variable's value, or with `default`
/// in `${VAR:-default}` when it is unset or empty; unset without a default is
/// an error.
fn expand_env(s: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let end = rest[start..].find('}').ok_or_else(|| anyhow!("unterminated ${{...}} in \"{s}\""))? + start;
        let inner = &rest[start + 2..end];
        let (var, default) = match inner.split_once(":-") {
            Some((v, d)) => (v, Some(d)),
            None => (inner, None),
        };
        if var.is_empty() || !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("invalid variable name \"{var}\" in \"{s}\" (use ${{NAME}} or ${{NAME:-default}})");
        }
        let value = match (std::env::var(var), default) {
            (Ok(v), Some(d)) if v.is_empty() => d.to_string(),
            (Ok(v), _) => v,
            (Err(_), Some(d)) => d.to_string(),
            (Err(_), None) => bail!("environment variable {var} is not set (set it, or write ${{{var}:-default}} for a fallback)"),
        };
        out.push_str(&rest[..start]);
        out.push_str(&value);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

// ------------------------------------------------------------------ docker

/// A `docker run` server: the container id is written to `cidfile` so the
/// container can be removed when the server is closed.
struct Docker {
    program: PathBuf,
    cidfile: PathBuf,
}

/// `docker run …` gains `--cidfile` (unless it already has one).
fn docker_run(program: &Path, args: &[String]) -> Option<(Vec<String>, Docker)> {
    let stem = program.file_stem()?.to_str()?;
    if !stem.eq_ignore_ascii_case("docker") || args.first().map(String::as_str) != Some("run") {
        return None;
    }
    if args.iter().any(|a| a == "--cidfile" || a.starts_with("--cidfile=")) {
        return None;
    }
    let cidfile = std::env::temp_dir().join(format!("nyq-mcp-{}.cid", uuid::Uuid::new_v4().simple()));
    let mut out = vec!["run".to_string(), "--cidfile".to_string(), cidfile.display().to_string()];
    out.extend(args[1..].iter().cloned());
    Some((out, Docker { program: program.to_path_buf(), cidfile }))
}

impl Docker {
    async fn remove(&self) {
        if let Ok(id) = tokio::fs::read_to_string(&self.cidfile).await {
            let id = id.trim();
            if !id.is_empty() {
                let rm = tokio::process::Command::new(&self.program)
                    .args(["rm", "-f", id])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = tokio::time::timeout(Duration::from_secs(15), rm).await;
            }
        }
        let _ = tokio::fs::remove_file(&self.cidfile).await;
    }
}

// ----------------------------------------------------------------- manager

/// Where a configured server stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Ready,
    NeedsAuth,
    Failed(String),
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::Connecting => "connecting".into(),
            Status::Ready => "ready".into(),
            Status::NeedsAuth => "needs sign-in".into(),
            Status::Failed(why) => format!("failed: {why}"),
        }
    }
}

/// What a `list_changed` notification asks to re-read.
#[derive(Clone, Copy, Debug)]
pub enum Refresh {
    Tools,
    Prompts,
    Resources,
}

struct Server {
    service: RunningService<RoleClient, Client>,
    names: Vec<String>,
    /// The server offers resources (`resources/list`, `resources/read`).
    resources: bool,
    /// Prompts it listed (offered as `/mcp:<server>:<prompt>`).
    prompts: Vec<rmcp::model::Prompt>,
    /// It answers `completion/complete`.
    completions: bool,
    docker: Option<Docker>,
    operation_timeout: Duration,
}

/// Tools built from a listing, and their names.
type Built = (Vec<Box<dyn Tool>>, Vec<String>);

type ChangeHook = Arc<dyn Fn(&McpManager) + Send + Sync>;

struct Shared {
    /// Profile directory; OAuth credentials live in `<home>/mcp-auth/`.
    home: PathBuf,
    servers: Mutex<BTreeMap<String, Server>>,
    status: Mutex<BTreeMap<String, Status>>,
    /// The configuration each server was last connected with (for restarts).
    configs: Mutex<HashMap<String, ServerConfig>>,
    restarts: Mutex<HashMap<String, u32>>,
    restart_lock: tokio::sync::Mutex<()>,
    /// Credential file → `name\nendpoint`, so two identities cannot share one.
    cred_owners: Mutex<HashMap<PathBuf, String>>,
    /// Scopes a server said it needs (403 insufficient_scope), for the next sign-in.
    step_up: Mutex<HashMap<String, String>>,
    live: LiveTools,
    elicitor: Mutex<Arc<dyn Elicitor>>,
    on_change: Mutex<Option<ChangeHook>>,
    /// Running tool calls' progress tokens → their status line.
    progress: Mutex<HashMap<(String, String), mpsc::UnboundedSender<String>>>,
    /// URL elicitations waiting for `notifications/elicitation/complete`.
    elicitations: Mutex<HashMap<String, oneshot::Sender<()>>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Connected MCP servers. Cloning shares the same servers.
#[derive(Clone)]
pub struct McpManager {
    shared: Arc<Shared>,
}

/// A handle that does not keep the manager alive (held by connections).
#[derive(Clone)]
pub struct WeakManager(Weak<Shared>);

impl WeakManager {
    pub fn upgrade(&self) -> Option<McpManager> {
        self.0.upgrade().map(|shared| McpManager { shared })
    }
}

impl McpManager {
    pub fn new(home: PathBuf) -> Self {
        Self {
            shared: Arc::new(Shared {
                home,
                servers: Mutex::default(),
                status: Mutex::default(),
                configs: Mutex::default(),
                restarts: Mutex::default(),
                restart_lock: tokio::sync::Mutex::new(()),
                cred_owners: Mutex::default(),
                step_up: Mutex::default(),
                live: LiveTools::default(),
                elicitor: Mutex::new(Arc::new(TerminalElicitor)),
                on_change: Mutex::default(),
                progress: Mutex::default(),
                elicitations: Mutex::default(),
                tasks: Mutex::default(),
            }),
        }
    }

    fn downgrade(&self) -> WeakManager {
        WeakManager(Arc::downgrade(&self.shared))
    }

    /// Let `tools` receive tool changes made in the background (servers that
    /// finish connecting, change their tool lists or restart) and server
    /// instructions.
    pub fn attach(&self, tools: &mut crate::tools::Registry) {
        tools.attach(self.shared.live.clone());
    }

    /// Who answers servers' elicitation requests (default: the terminal).
    pub fn set_elicitor(&self, elicitor: Arc<dyn Elicitor>) {
        *lock(&self.shared.elicitor) = elicitor;
    }

    pub(crate) fn elicitor(&self) -> Arc<dyn Elicitor> {
        lock(&self.shared.elicitor).clone()
    }

    /// Called whenever servers or their prompts change (e.g. to refresh the
    /// editor's prompt commands).
    pub fn on_change(&self, hook: impl Fn(&McpManager) + Send + Sync + 'static) {
        *lock(&self.shared.on_change) = Some(Arc::new(hook));
    }

    fn changed(&self) {
        let hook = lock(&self.shared.on_change).clone();
        if let Some(hook) = hook {
            hook(self);
        }
    }

    fn set_status(&self, name: &str, status: Status) {
        lock(&self.shared.status).insert(name.to_string(), status);
    }

    /// Where `name` stands, if it was ever started.
    pub fn status(&self, name: &str) -> Option<Status> {
        lock(&self.shared.status).get(name).cloned()
    }

    /// Tool names currently registered for `name` (empty when it is not connected).
    pub fn registered_names(&self, name: &str) -> Vec<String> {
        lock(&self.shared.servers).get(name).map(|s| s.names.clone()).unwrap_or_default()
    }

    fn is_connected(&self, name: &str) -> bool {
        lock(&self.shared.servers).contains_key(name)
    }

    fn claim_store(&self, name: &str, endpoint: &str) -> Result<FileCredentialStore> {
        let path = credential_path(&self.shared.home, name, endpoint)?;
        let key = format!("{name}\n{endpoint}");
        let mut owners = lock(&self.shared.cred_owners);
        if let Some(owner) = owners.get(&path) {
            if owner != &key {
                bail!("MCP server '{name}' would share a credential file with another server");
            }
        } else if owners.values().any(|owner| owner == &key) {
            bail!("MCP server '{name}' is already stored under another credential file");
        }
        owners.insert(path.clone(), key);
        Ok(FileCredentialStore::new(path))
    }

    /// Start (or reconnect to) the server and return its tools.
    pub async fn connect(&self, cfg: &ServerConfig) -> Result<Vec<Box<dyn Tool>>> {
        lock(&self.shared.restarts).remove(&cfg.name);
        self.connect_inner(cfg).await
    }

    async fn connect_inner(&self, cfg: &ServerConfig) -> Result<Vec<Box<dyn Tool>>> {
        if self.is_connected(&cfg.name) {
            self.disconnect(&cfg.name).await?;
        }
        lock(&self.shared.configs).insert(cfg.name.clone(), cfg.clone());
        self.set_status(&cfg.name, Status::Connecting);
        let result = match self.open(cfg).await {
            Ok((service, docker)) => self.register(cfg, service, docker).await,
            Err(e) => Err(e),
        };
        if let Err(e) = &result {
            let text = format!("{e:#}");
            let status = if text.contains("needs sign-in") {
                Status::NeedsAuth
            } else {
                Status::Failed(text.lines().next().unwrap_or("").chars().take(200).collect())
            };
            self.set_status(&cfg.name, status);
        }
        result
    }

    /// Connect several servers at once; results in the same order.
    pub async fn connect_all(&self, cfgs: &[ServerConfig]) -> Vec<Result<Vec<Box<dyn Tool>>>> {
        futures::future::join_all(cfgs.iter().map(|c| self.connect(c))).await
    }

    /// Connect servers in the background. Their tools reach the attached
    /// registry before the next model request; `announce` reports each
    /// outcome (failures are always reported).
    pub fn start_background(&self, cfgs: Vec<ServerConfig>, announce: bool) {
        let mut tasks = lock(&self.shared.tasks);
        tasks.retain(|t| !t.is_finished());
        for cfg in cfgs {
            self.set_status(&cfg.name, Status::Connecting);
            let mcp = self.clone();
            tasks.push(tokio::spawn(async move {
                match mcp.connect(&cfg).await {
                    Ok(tools) => {
                        let n = tools.len();
                        mcp.shared.live.push(mcp.install_patch(&cfg.name, Vec::new(), tools));
                        if announce {
                            crate::ui::info(&format!("MCP {}: {n} tools", cfg.name));
                        }
                    }
                    Err(e) => crate::ui::error(&format!("MCP {}: {e:#}", cfg.name)),
                }
            }));
        }
    }

    /// A patch replacing `server`'s tools with `tools` (and the resource and
    /// prompt tools); a no-op once the server is disconnected.
    fn install_patch(&self, server: &str, old: Vec<String>, tools: Vec<Box<dyn Tool>>) -> ToolPatch {
        let (server, mcp) = (server.to_string(), self.clone());
        Box::new(move |reg| {
            reg.remove_names(&old);
            if mcp.is_connected(&server) {
                reg.remove_prefix(&tool_prefix(&server));
                for tool in tools {
                    if let Err(e) = reg.try_insert(tool) {
                        crate::ui::error(&format!("MCP {server}: {e}"));
                    }
                }
            }
            crate::mcp_features::sync_tools(reg, &mcp);
        })
    }

    async fn open(&self, cfg: &ServerConfig) -> Result<(RunningService<RoleClient, Client>, Option<Docker>)> {
        let client = Client { server: cfg.name.clone(), mcp: self.downgrade() };
        let timeout = cfg.options.startup_timeout;
        match &cfg.transport {
            Transport::Stdio { command, args, env, cwd } => {
                #[cfg(windows)]
                let program = {
                    let variable = |name: &str| env.iter().find(|(k, _)| k.eq_ignore_ascii_case(name))
                        .map(|(_, v)| v.clone()).or_else(|| std::env::var(name).ok());
                    let path = variable("PATH").unwrap_or_default();
                    let base = cwd.clone().unwrap_or(std::env::current_dir()?);
                    let paths: Vec<PathBuf> = std::env::split_paths(&path).map(|p| base.join(p)).collect();
                    let extensions = variable("PATHEXT")
                        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
                    resolve_launcher(command, &paths, &extensions)
                };
                #[cfg(not(windows))]
                let program = PathBuf::from(command);
                let (args, docker) = match docker_run(&program, args) {
                    Some((args, docker)) => (args, Some(docker)),
                    None => (args.clone(), None),
                };
                let mut cmd = tokio::process::Command::new(&program);
                cmd.args(&args).envs(env);
                if let Some(dir) = cwd {
                    cmd.current_dir(dir);
                }
                let (transport, _) = TokioChildProcess::builder(cmd)
                    .stderr(Stdio::null())
                    .spawn()
                    .with_context(|| format!("starting MCP server '{}' ({command})", cfg.name))?;
                match initialize(client, transport, timeout).await {
                    Ok(service) => Ok((service, docker)),
                    Err(e) => {
                        if let Some(d) = &docker {
                            d.remove().await;
                        }
                        Err(init_error(&cfg.name, e, false, timeout))
                    }
                }
            }
            Transport::Http { url, headers } => Ok((self.connect_http(cfg, client, url, headers).await?, None)),
            Transport::Sse { url, headers } => {
                let transport = crate::mcp_sse::SseTransport::connect(url, headers, timeout)
                    .await
                    .map_err(|e| anyhow!("MCP server '{}': {e}", cfg.name))?;
                let service = initialize(client, transport, timeout).await.map_err(|e| init_error(&cfg.name, e, false, timeout))?;
                Ok((service, None))
            }
        }
    }

    async fn connect_http(
        &self,
        cfg: &ServerConfig,
        client: Client,
        url: &str,
        headers: &BTreeMap<String, String>,
    ) -> Result<RunningService<RoleClient, Client>> {
        let (name, timeout) = (cfg.name.as_str(), cfg.options.startup_timeout);
        let config = http_config(url, headers)?;
        let explicit = headers.keys().any(|k| k.eq_ignore_ascii_case("authorization"));
        let endpoint = reqwest::Url::parse(url).with_context(|| format!("MCP server '{name}': invalid url"))?.to_string();
        let store = self.claim_store(name, &endpoint)?;
        let result = if !explicit && store.path.exists() {
            let mut manager = AuthorizationManager::new(url).await?;
            manager.set_credential_store(store);
            let signed_in = tokio::time::timeout(timeout, manager.initialize_from_store())
                .await
                .map_err(|_| anyhow!("MCP server '{name}': OAuth discovery timed out"))?
                .with_context(|| format!("MCP server '{name}': loading saved sign-in"))?;
            if signed_in {
                let auth = AuthClient::new(crate::http::client(), manager);
                initialize(client, StreamableHttpClientTransport::with_client(auth, config), timeout).await
            } else {
                initialize(client, StreamableHttpClientTransport::with_client(crate::http::client(), config), timeout).await
            }
        } else {
            initialize(client, StreamableHttpClientTransport::with_client(crate::http::client(), config), timeout).await
        };
        result.map_err(|e| init_error(name, e, explicit, timeout))
    }

    async fn register(
        &self,
        cfg: &ServerConfig,
        service: RunningService<RoleClient, Client>,
        docker: Option<Docker>,
    ) -> Result<Vec<Box<dyn Tool>>> {
        let timeout = cfg.options.startup_timeout;
        let listed = match tokio::time::timeout(timeout, service.list_all_tools()).await {
            Ok(Ok(t)) => self.build_tools(&cfg.name, t),
            Ok(Err(e)) => Err(anyhow!("listing tools of MCP server '{}': {e}", cfg.name)),
            Err(_) => Err(anyhow!("MCP server '{}' did not list its tools within {}", cfg.name, secs(timeout))),
        };
        let (tools, names) = match listed {
            Ok(t) => t,
            Err(e) => {
                close(Server {
                    service,
                    names: Vec::new(),
                    resources: false,
                    prompts: Vec::new(),
                    completions: false,
                    docker,
                    operation_timeout: OPERATION_TIMEOUT,
                })
                .await;
                return Err(e);
            }
        };
        let peer = service.peer().clone();
        let info = peer.peer_info();
        let caps = info.as_ref().map(|i| i.capabilities.clone()).unwrap_or_default();
        let prompts = match caps.prompts {
            Some(_) => tokio::time::timeout(timeout, peer.list_all_prompts()).await.ok().and_then(|r| r.ok()).unwrap_or_default(),
            None => Vec::new(),
        };
        self.shared.live.set_note(&cfg.name, info.as_ref().and_then(|i| i.instructions.clone()));
        let server = Server {
            service,
            names,
            resources: caps.resources.is_some(),
            prompts,
            completions: caps.completions.is_some(),
            docker,
            operation_timeout: cfg.options.operation_timeout,
        };
        lock(&self.shared.servers).insert(cfg.name.clone(), server);
        self.set_status(&cfg.name, Status::Ready);
        self.changed();
        Ok(tools)
    }

    fn build_tools(&self, server: &str, listed: Vec<rmcp::model::Tool>) -> Result<Built> {
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        let mut seen: HashMap<String, String> = HashMap::new();
        for t in listed {
            let name = tool_name(server, &t.name);
            if let Some(other) = seen.insert(name.clone(), t.name.to_string()) {
                bail!("MCP server '{server}': tools `{other}` and `{}` both map to `{name}`", t.name);
            }
            let mut schema = Value::Object(t.input_schema.as_ref().clone());
            if let Value::Object(m) = &mut schema {
                m.entry("type").or_insert_with(|| "object".into());
            }
            tools.push(Box::new(McpTool {
                name,
                description: crate::limits::clip(
                    "mcp_description_bytes",
                    t.description.as_deref().unwrap_or_default(),
                ),
                schema,
                tool: t.name.to_string(),
                server: server.to_string(),
                mcp: self.clone(),
            }));
        }
        Ok((tools, seen.into_keys().collect()))
    }

    /// Re-read what a `list_changed` notification says has changed.
    pub(crate) async fn refresh(&self, server: &str, what: Refresh) {
        let Some(peer) = self.peer(server) else { return };
        match what {
            Refresh::Tools => {
                let listed = match tokio::time::timeout(REFRESH_TIMEOUT, peer.list_all_tools()).await {
                    Ok(Ok(t)) => t,
                    _ => return crate::ui::error(&format!("MCP {server}: could not re-list its changed tools")),
                };
                let (tools, names) = match self.build_tools(server, listed) {
                    Ok(t) => t,
                    Err(e) => return crate::ui::error(&format!("{e:#}")),
                };
                let old = {
                    let mut servers = lock(&self.shared.servers);
                    let Some(s) = servers.get_mut(server) else { return };
                    std::mem::replace(&mut s.names, names)
                };
                self.shared.live.push(self.install_patch(server, old, tools));
            }
            Refresh::Prompts => {
                let Ok(Ok(prompts)) = tokio::time::timeout(REFRESH_TIMEOUT, peer.list_all_prompts()).await else { return };
                if let Some(s) = lock(&self.shared.servers).get_mut(server) {
                    s.prompts = prompts;
                }
                self.sync_features();
            }
            Refresh::Resources => self.sync_features(),
        }
        self.changed();
    }

    fn sync_features(&self) {
        let mcp = self.clone();
        self.shared.live.push(Box::new(move |reg| crate::mcp_features::sync_tools(reg, &mcp)));
    }

    /// The server's peer, restarting it first when it has exited (up to its
    /// `restart_limit`).
    async fn ready_peer(&self, server: &str) -> Result<Peer<RoleClient>> {
        match self.peer(server) {
            Some(peer) if !peer.is_transport_closed() => Ok(peer),
            Some(_) => self.restart(server).await,
            None if self.status(server) == Some(Status::Connecting) => {
                bail!("MCP server '{server}' is still connecting; try again in a moment")
            }
            None => bail!("MCP server '{server}' is not connected"),
        }
    }

    async fn restart(&self, server: &str) -> Result<Peer<RoleClient>> {
        let _one = self.shared.restart_lock.lock().await;
        if let Some(peer) = self.peer(server).filter(|p| !p.is_transport_closed()) {
            return Ok(peer); // restarted meanwhile
        }
        let cfg = lock(&self.shared.configs).get(server).cloned().ok_or_else(|| anyhow!("MCP server '{server}' is not connected"))?;
        let tries = *lock(&self.shared.restarts).entry(server.to_string()).and_modify(|n| *n += 1).or_insert(1);
        if tries > cfg.options.restart_limit {
            let names = self.registered_names(server);
            let dead = lock(&self.shared.servers).remove(server);
            if let Some(dead) = dead {
                close(dead).await;
            }
            self.shared.live.push(self.install_patch(server, names, Vec::new()));
            self.set_status(server, Status::Failed("the server exited".into()));
            self.changed();
            bail!("MCP server '{server}' exited (restart limit reached); reconnect it with /mcp connect {server}");
        }
        crate::ui::info(&format!("MCP {server}: the server exited; restarting it"));
        let old = self.registered_names(server);
        let tools = self.connect_inner(&cfg).await?;
        self.shared.live.push(self.install_patch(server, old, tools));
        self.peer(server).ok_or_else(|| anyhow!("MCP server '{server}' is not connected"))
    }

    fn operation_timeout(&self, server: &str) -> Duration {
        lock(&self.shared.servers).get(server).map_or(OPERATION_TIMEOUT, |s| s.operation_timeout)
    }

    /// Timeout for resource and prompt requests: the server's operation
    /// timeout, at most `cap`.
    pub fn request_timeout(&self, server: &str, cap: Duration) -> Duration {
        self.operation_timeout(server).min(cap)
    }

    fn watch_progress(&self, server: &str, token: &ProgressToken) -> ProgressWatch {
        let key = (server.to_string(), serde_json::to_string(&token.0).unwrap_or_default());
        let (tx, rx) = mpsc::unbounded_channel();
        lock(&self.shared.progress).insert(key.clone(), tx);
        ProgressWatch { rx, key, mcp: self.downgrade() }
    }

    pub(crate) fn progress(&self, server: &str, p: &ProgressNotificationParam) {
        let key = (server.to_string(), serde_json::to_string(&p.progress_token.0).unwrap_or_default());
        if let Some(tx) = lock(&self.shared.progress).get(&key) {
            let _ = tx.send(progress_text(p));
        }
    }

    pub(crate) fn expect_elicitation(&self, id: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        lock(&self.shared.elicitations).insert(id.to_string(), tx);
        rx
    }

    /// The server finished (or nyq stopped waiting for) a URL elicitation.
    pub(crate) fn complete_elicitation(&self, id: &str) {
        if let Some(tx) = lock(&self.shared.elicitations).remove(id) {
            let _ = tx.send(());
        }
    }

    fn note_step_up(&self, server: &str, scope: &str) {
        let mut step = lock(&self.shared.step_up);
        let entry = step.entry(server.to_string()).or_default();
        for s in scope.split_whitespace() {
            if !entry.split_whitespace().any(|have| have == s) {
                entry.push(' ');
                entry.push_str(s);
            }
        }
    }

    /// Suggestions for a prompt argument (`completion/complete`), when the
    /// server supports completion; empty otherwise.
    pub async fn complete_prompt_argument(&self, server: &str, prompt: &str, argument: &str, value: &str) -> Vec<String> {
        let peer = match lock(&self.shared.servers).get(server) {
            Some(s) if s.completions => s.service.peer().clone(),
            _ => return Vec::new(),
        };
        let request = peer.complete_prompt_argument(prompt, argument, value, None);
        match tokio::time::timeout(Duration::from_secs(5), request).await {
            Ok(Ok(info)) => info.values.into_iter().take(10).collect(),
            _ => Vec::new(),
        }
    }

    /// Sign in to an HTTP server with OAuth (discovery, dynamic client
    /// registration as "nyq" or the configured client, PKCE, loopback
    /// redirect), save the credentials and connect. `open` receives the
    /// authorization URL to show the user.
    pub async fn login(&self, cfg: &ServerConfig, cancel: Option<&CancellationToken>, open: impl FnOnce(&str)) -> Result<Vec<Box<dyn Tool>>> {
        tokio::select! {
            biased;
            _ = async {
                match cancel {
                    Some(cancel) => cancel.cancelled().await,
                    None => { let _ = tokio::signal::ctrl_c().await; }
                }
            } => bail!("sign-in cancelled"),
            result = self.login_inner(cfg, open) => result,
        }
    }

    async fn login_inner(&self, cfg: &ServerConfig, open: impl FnOnce(&str)) -> Result<Vec<Box<dyn Tool>>> {
        let Transport::Http { url, headers } = &cfg.transport else {
            bail!("MCP server '{}' is not a streamable HTTP server; only those support sign-in", cfg.name);
        };
        if self.is_connected(&cfg.name) {
            self.disconnect(&cfg.name).await?;
        }
        // Probe without saved credentials to learn the server's auth challenge.
        let timeout = cfg.options.startup_timeout;
        let client = Client { server: cfg.name.clone(), mcp: self.downgrade() };
        let probe = initialize(client, StreamableHttpClientTransport::with_client(crate::http::client(), http_config(url, headers)?), timeout);
        let challenge = match probe.await {
            Ok(service) => return self.register(cfg, service, None).await, // no sign-in needed
            Err(Init::Failed(e)) if is_unauthorized(&e) => e.auth_challenge().map(String::from),
            Err(e) => return Err(init_error(&cfg.name, e, false, timeout)),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.context("opening the sign-in callback listener")?;
        let port = listener.local_addr()?.port();
        let redirect = format!("http://127.0.0.1:{port}/callback");
        let mut manager = AuthorizationManager::new(url.as_str()).await?;
        let endpoint = canonical_endpoint(&cfg.transport)?;
        let store = self.claim_store(&cfg.name, &endpoint)?;
        let step_up = lock(&self.shared.step_up).get(&cfg.name).cloned();
        let granted = match step_up {
            Some(_) => store.load().await.ok().flatten().map(|c| c.granted_scopes).unwrap_or_default(),
            None => Vec::new(),
        };
        manager.set_credential_store(store);
        let metadata = manager
            .resolve_metadata_from_challenge(challenge.as_deref())
            .await
            .context("discovering the server's OAuth configuration")?;
        manager.set_metadata(metadata.metadata);
        let mut request = AuthorizationRequest::new(&redirect).with_client_name("nyq");
        let scopes = login_scopes(&cfg.options.oauth.scopes, &granted, step_up.as_deref(), challenge.as_deref(), url);
        if !scopes.is_empty() {
            request = request.with_scopes(scopes);
        }
        if let Some(id) = &cfg.options.oauth.client_id {
            request = request.with_preregistered_client(id);
            if let Some(var) = &cfg.options.oauth.client_secret_env {
                let secret = std::env::var(var).map_err(|_| anyhow!("environment variable {var} (the OAuth client secret) is not set"))?;
                request = request.with_client_secret(secret);
            }
        }
        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, e)| anyhow!("starting sign-in: {e}"))?;
        open(session.get_authorization_url());
        let callback = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_callback(&listener, port))
            .await
            .map_err(|_| anyhow!("sign-in timed out after 5 minutes"))??;
        drop(listener);
        session.handle_callback_url(&callback).await.context("completing sign-in")?;
        lock(&self.shared.step_up).remove(&cfg.name);
        self.connect(cfg).await
    }

    /// Forget saved OAuth credentials (disconnecting first), revoking the
    /// tokens when the authorization server offers revocation. Returns
    /// whether any were stored.
    pub async fn logout(&self, name: &str, endpoint: &str) -> Result<bool> {
        if self.is_connected(name) {
            self.disconnect(name).await?;
        }
        let path = credential_path(&self.shared.home, name, endpoint)?;
        lock(&self.shared.cred_owners).remove(&path);
        let store = FileCredentialStore::new(path);
        if let (Ok(Some(saved)), false) = (store.load().await, endpoint.is_empty()) {
            match tokio::time::timeout(Duration::from_secs(15), revoke(endpoint, &saved)).await {
                Ok(Err(e)) => crate::ui::error(&format!("MCP {name}: could not revoke the sign-in: {e:#}")),
                Err(_) => crate::ui::error(&format!("MCP {name}: revoking the sign-in timed out")),
                Ok(Ok(())) => {}
            }
        }
        let existed = store.path.exists();
        store.clear().await?;
        Ok(existed)
    }

    pub async fn disconnect(&self, name: &str) -> Result<()> {
        let server = lock(&self.shared.servers).remove(name).ok_or_else(|| anyhow!("MCP server '{name}' is not connected"))?;
        lock(&self.shared.status).remove(name);
        self.shared.live.set_note(name, None);
        close(server).await;
        self.changed();
        Ok(())
    }

    /// (server name, tool count) for connected servers.
    pub fn connected(&self) -> Vec<(String, usize)> {
        lock(&self.shared.servers).iter().map(|(n, s)| (n.clone(), s.names.len())).collect()
    }

    /// Connected servers that offer resources, with a handle to each.
    pub fn resource_servers(&self) -> Vec<(String, Peer<RoleClient>)> {
        lock(&self.shared.servers)
            .iter()
            .filter(|(_, s)| s.resources)
            .map(|(n, s)| (n.clone(), s.service.peer().clone()))
            .collect()
    }

    /// Every prompt of every connected server, with the server's name.
    pub fn prompts(&self) -> Vec<(String, rmcp::model::Prompt)> {
        lock(&self.shared.servers).iter().flat_map(|(n, s)| s.prompts.iter().map(move |p| (n.clone(), p.clone()))).collect()
    }

    pub fn peer(&self, name: &str) -> Option<Peer<RoleClient>> {
        lock(&self.shared.servers).get(name).map(|s| s.service.peer().clone())
    }

    pub async fn shutdown(&self) {
        for task in std::mem::take(&mut *lock(&self.shared.tasks)) {
            task.abort();
        }
        let all = std::mem::take(&mut *lock(&self.shared.servers));
        lock(&self.shared.status).clear();
        futures::future::join_all(all.into_values().map(close)).await;
    }
}

/// Removes a running call's progress subscription when the call ends.
struct ProgressWatch {
    rx: mpsc::UnboundedReceiver<String>,
    key: (String, String),
    mcp: WeakManager,
}

impl Drop for ProgressWatch {
    fn drop(&mut self) {
        if let Some(mcp) = self.mcp.upgrade() {
            lock(&mcp.shared.progress).remove(&self.key);
        }
    }
}

fn progress_text(p: &ProgressNotificationParam) -> String {
    let amount = match p.total {
        Some(total) if total > 0.0 => format!("{:.0}%", (p.progress / total * 100.0).clamp(0.0, 100.0)),
        _ => format!("{}", p.progress),
    };
    match p.message.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => format!("{amount} · {m}"),
        None => amount,
    }
}

fn secs(d: Duration) -> String {
    match d.as_millis() {
        ms if ms % 1000 == 0 => format!("{}s", ms / 1000),
        ms => format!("{ms}ms"),
    }
}

/// Scopes to ask for at sign-in: the configured ones, plus — when a server
/// asked for more access — those granted before and those it asked for, plus
/// the challenge's. Empty lets the SDK pick.
fn login_scopes(configured: &[String], granted: &[String], step_up: Option<&str>, challenge: Option<&str>, url: &str) -> Vec<String> {
    // Without either, the SDK picks from the challenge and server metadata.
    if configured.is_empty() && step_up.is_none() {
        return Vec::new();
    }
    let challenged = challenge
        .and_then(|c| reqwest::Url::parse(url).ok().map(|u| WWWAuthenticateParams::parse(c, &u)))
        .and_then(|p| p.scope)
        .unwrap_or_default();
    let more = step_up.map(|s| granted.iter().map(String::as_str).chain(s.split_whitespace()).collect::<Vec<_>>());
    let mut out: Vec<String> = Vec::new();
    for s in configured.iter().map(String::as_str).chain(more.unwrap_or_default()).chain(challenged.split_whitespace()) {
        if !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    }
    out
}

/// Revoke saved tokens (RFC 7009) when the authorization server lists a
/// `revocation_endpoint`.
async fn revoke(url: &str, saved: &StoredCredentials) -> Result<()> {
    let Some(token) = &saved.token_response else { return Ok(()) };
    let token = serde_json::to_value(token)?;
    let manager = AuthorizationManager::new(url).await?;
    let metadata = manager.resolve_metadata().await?.metadata;
    let Some(endpoint) = metadata.additional_fields.get("revocation_endpoint").and_then(Value::as_str) else { return Ok(()) };
    let client = crate::http::client();
    for hint in ["refresh_token", "access_token"] {
        let Some(value) = token.get(hint).and_then(Value::as_str) else { continue };
        let mut form = reqwest::Url::parse("http://form/")?;
        form.query_pairs_mut().extend_pairs([("token", value), ("token_type_hint", hint), ("client_id", &saved.client_id)]);
        let status = client
            .post(endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form.query().unwrap_or_default().to_string())
            .send()
            .await?
            .status();
        anyhow::ensure!(status.is_success(), "the revocation endpoint answered HTTP {status}");
    }
    Ok(())
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

async fn initialize<T, E, A>(client: Client, transport: T, timeout: Duration) -> std::result::Result<RunningService<RoleClient, Client>, Init>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(timeout, client.serve(transport)).await {
        Ok(r) => r.map_err(|e| Init::Failed(Box::new(e))),
        Err(_) => Err(Init::Timeout),
    }
}

fn init_error(name: &str, e: Init, explicit_auth: bool, timeout: Duration) -> anyhow::Error {
    match e {
        Init::Timeout => anyhow!("MCP server '{name}' did not initialize within {} (raise startup_timeout_ms if it needs longer)", secs(timeout)),
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

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    crate::session::write_private(path, bytes)
}

/// Open `url` in the user's browser, ignoring failures (the URL is printed too).
pub fn open_browser(url: &str) {
    crate::login::open_browser(url);
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
    if let Some(docker) = &server.docker {
        docker.remove().await;
    }
}

/// Sanitized `mcp__<server>__` prefix actually used on tool names.
pub fn tool_prefix(server: &str) -> String {
    sanitize(&format!("mcp__{server}__"))
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// `mcp__<server>__<tool>`, restricted to `[A-Za-z0-9_-]`. Names that would
/// exceed 64 characters keep a stable hash of the full name so a shared
/// prefix cannot collide.
fn tool_name(server: &str, tool: &str) -> String {
    let raw = sanitize(&format!("mcp__{server}__{tool}"));
    if raw.len() <= 64 {
        return raw;
    }
    let hash = format!("{:08x}", fnv64(raw.as_bytes()) & 0xffff_ffff);
    let keep = 64 - hash.len() - 1;
    format!("{}_{hash}", &raw[..keep])
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Canonical identity of a server's credential file. Stdio has no endpoint.
pub fn canonical_endpoint(transport: &Transport) -> Result<String> {
    match transport {
        Transport::Http { url, .. } | Transport::Sse { url, .. } => {
            Ok(reqwest::Url::parse(url).with_context(|| format!("invalid MCP url {url}"))?.to_string())
        }
        Transport::Stdio { .. } => Ok(String::new()),
    }
}

/// Injective filename for `name` + newline + canonical endpoint. `_` is
/// escaped, and names that would exceed 180 characters are rejected.
pub fn credential_path(home: &Path, name: &str, endpoint: &str) -> Result<PathBuf> {
    let file = encode_ident(&format!("{name}\n{endpoint}"))?;
    Ok(home.join("mcp-auth").join(format!("{file}.json")))
}

fn encode_ident(key: &str) -> Result<String> {
    let mut out = String::new();
    for c in key.chars() {
        if c.is_ascii_alphanumeric() || c == '-' {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push('_');
                out.push(char::from(b"0123456789abcdef"[(b >> 4) as usize]));
                out.push(char::from(b"0123456789abcdef"[(b & 0xf) as usize]));
            }
        }
    }
    if out.len() > 180 {
        bail!("MCP credential name is too long to store");
    }
    Ok(out)
}


struct McpTool {
    name: String,
    description: String,
    schema: Value,
    /// The tool's name on the server.
    tool: String,
    server: String,
    mcp: McpManager,
}

/// The scope a server asked for in a 403 `insufficient_scope` answer.
fn insufficient_scope(error: &str) -> Option<String> {
    let rest = &error[error.find("Insufficient scope")?..];
    let scope = rest.split_once(':').map(|(_, s)| s.trim()).unwrap_or("");
    Some(scope.lines().next().unwrap_or("").to_string())
}

impl McpTool {
    async fn failed(&self, peer: &Peer<RoleClient>, error: String) -> ToolOutput {
        let server = &self.server;
        if let Some(scope) = insufficient_scope(&error) {
            self.mcp.note_step_up(server, &scope);
            let what = if scope.is_empty() { String::new() } else { format!(" (scope: {scope})") };
            return ToolOutput::err(format!(
                "MCP server '{server}' needs more access for this call{what}. Ask the user to run /mcp login {server} to grant it."
            ));
        }
        if peer.is_transport_closed() {
            return ToolOutput::err(match self.mcp.ready_peer(server).await {
                Ok(_) => format!(
                    "MCP tool call failed: server '{server}' exited during the call and was restarted. \
                     Call the tool again only if repeating it is safe."
                ),
                Err(e) => format!("MCP tool call failed: {error}. {e:#}"),
            });
        }
        ToolOutput::err(format!("MCP tool call failed: {error}"))
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: self.name.clone(), description: self.description.clone(), parameters: self.schema.clone() }
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn handles_cancel(&self) -> bool {
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
        let problems = crate::mcp_schema::check(&self.schema, &Value::Object(arguments.clone()));
        if !problems.is_empty() {
            return ToolOutput::err(format!(
                "Invalid arguments for {}: {}. Fix them to match the tool's input schema and call it again.",
                self.name,
                problems.join("; ")
            ));
        }
        let peer = match self.mcp.ready_peer(&self.server).await {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(format!("{e:#}")),
        };
        let params = CallToolRequestParams::new(self.tool.clone()).with_arguments(arguments);
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let options = PeerRequestOptions::with_timeout(self.mcp.operation_timeout(&self.server));
        let handle = match peer.send_request_with_option(request, options).await {
            Ok(h) => h,
            Err(e) => return self.failed(&peer, e.to_string()).await,
        };
        let id = handle.id.clone();
        // rmcp sends a progress token with every request; show what arrives for it.
        let mut progress = self.mcp.watch_progress(&self.server, &handle.progress_token);
        let response = handle.await_response();
        tokio::pin!(response);
        loop {
            tokio::select! {
                res = &mut response => return match res {
                    Ok(ServerResult::CallToolResult(r)) => convert(r),
                    Ok(_) => ToolOutput::err("MCP server sent an unexpected response (tasks are not supported)."),
                    Err(e) => self.failed(&peer, e.to_string()).await,
                },
                Some(text) = progress.rx.recv() => crate::ui::tui::tool_progress(&text),
                _ = ctx.cancel.cancelled() => {
                    let note = CancelledNotificationParam::new(Some(id), Some("cancelled by user".into()));
                    let _ = peer.notify_cancelled(note).await;
                    return ToolOutput::err("Cancelled by user.");
                }
            }
        }
    }
}

fn convert(r: CallToolResult) -> ToolOutput {
    let mut images = Vec::new();
    let mut parts = Vec::new();
    for c in &r.content {
        match c {
            ContentBlock::Text(t) => parts.push(t.text.clone()),
            ContentBlock::Image(i) => match image_from_encoded(&i.data, &i.mime_type) {
                Ok(image) => images.push(image),
                Err(reason) => parts.push(format!("[image omitted ({reason})]")),
            },
            ContentBlock::Audio(a) => parts.push(format!("[{}]", a.mime_type)),
            ContentBlock::Resource(e) => match &e.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => parts.push(format!("[resource {uri}]\n{text}")),
                ResourceContents::BlobResourceContents { uri, mime_type, .. } => {
                    parts.push(format!("[resource {uri} ({})]", mime_type.as_deref().unwrap_or("binary")))
                }
                #[allow(unreachable_patterns)]
                _ => parts.push("[resource]".into()),
            },
            ContentBlock::ResourceLink(l) => parts.push(format!("[resource link {}]", l.uri)),
            #[allow(unreachable_patterns)]
            _ => parts.push("[unsupported content]".into()),
        }
    }
    let mut content = parts.join("\n");
    if let Some(s) = &r.structured_content {
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push_str("Structured content:\n");
        content.push_str(&serde_json::to_string_pretty(s).unwrap_or_else(|_| s.to_string()));
    }
    ToolOutput { content, is_error: r.is_error.unwrap_or(false), display: None, images, changed: false, ..ToolOutput::default() }
}

pub(crate) fn image_from_encoded(data: &str, mime: &str) -> Result<crate::types::Image, String> {
    let compact: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, compact.as_bytes())
        .map_err(|_| "invalid base64".to_string())?;
    crate::tools::image::image_from_bytes(&bytes, mime).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn login_can_be_cancelled_without_a_signal() {
        let dir = tempfile::tempdir().unwrap();
        let manager = McpManager::new(dir.path().into());
        let cfg = http_cfg("cancel", "http://127.0.0.1:1/mcp".into(), &[]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = manager.login(&cfg, Some(&cancel), |_| panic!("cancelled login opened browser")).await.err().unwrap();
        assert!(err.to_string().contains("sign-in cancelled"));
    }

    #[test]
    fn resolves_batch_launchers_through_pathext() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = dir.path().join("npx.cmd");
        std::fs::write(&launcher, "").unwrap();
        let paths = [dir.path().to_path_buf()];
        assert_eq!(resolve_launcher("npx", &paths, ".exe;.cmd"), launcher);
        assert_eq!(resolve_launcher("npx.cmd", &paths, ".exe;.cmd"), launcher);
        assert_eq!(resolve_launcher("missing", &paths, ".cmd"), PathBuf::from("missing"));
        // npm's extensionless `npx` script sits next to npx.cmd; the .cmd wins.
        std::fs::write(dir.path().join("npx"), "#!/bin/sh").unwrap();
        assert_eq!(resolve_launcher("npx", &paths, ".exe;.cmd"), launcher);
        // Relative PATH entries are ignored.
        assert_eq!(resolve_launcher("npx", &[PathBuf::from("rel")], ".cmd"), PathBuf::from("npx"));
    }

    #[test]
    fn images_and_structured_content_are_preserved() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png);
        let mut result = CallToolResult::success(vec![
            ContentBlock::text("hello"),
            ContentBlock::image(format!(" {data}\n"), "image/png"),
        ]);
        result.structured_content = Some(serde_json::json!({"n": 1}));
        let out = convert(result);
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        assert!(out.images[0].data.starts_with("iVBOR"));
        assert!(out.content.contains("hello"), "{}", out.content);
        assert!(out.content.contains("Structured content:"), "{}", out.content);
        assert!(out.content.contains("\"n\": 1"), "{}", out.content);

        let bad = convert(CallToolResult::success(vec![ContentBlock::image(&data, "image/jpeg")]));
        assert!(bad.images.is_empty());
        assert!(bad.content.contains("image omitted"), "{}", bad.content);
        let junk = convert(CallToolResult::success(vec![ContentBlock::image("@@@", "image/png")]));
        assert!(junk.images.is_empty() && junk.content.contains("image omitted"));
    }

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
        assert_eq!(names, ["fs", "legacy", "plain", "web"]);
        let Transport::Stdio { command, args, env, cwd: dir } = &servers[0].transport else { panic!() };
        assert_eq!((command.as_str(), servers[0].scope), ("npx", Scope::Profile));
        assert_eq!(args, &["-y", "server-fs"]);
        assert_eq!(env["A"], "1");
        assert_eq!(dir.as_deref(), Some(home.path().join("sub").as_path()));
        let Transport::Sse { url, .. } = &servers[1].transport else { panic!() };
        assert_eq!(url, "https://example.com/sse");
        let Transport::Http { url, headers } = &servers[3].transport else { panic!() };
        assert_eq!(url, "https://example.com/mcp");
        assert_eq!(headers["X-Team"], "team-core!");
        assert_eq!(headers["Authorization"], "Bearer tok123");
        assert!(!format!("{:?}", servers[3]).contains("tok123"), "Debug must not leak header values");
        let Transport::Http { headers, .. } = &servers[2].transport else { panic!() };
        assert!(headers.is_empty());

        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("'unset'") && w.contains("NYQ_MCP_TEST_UNSET_VAR") && w.contains(":-default")));
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
    fn config_aliases_and_options() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("NYQ_MCP_TEST_HDR", "h1");
        std::fs::write(
            home.path().join("mcp.json"),
            r#"{"servers": {}, "mcp": {
                "loc": {"type": "local", "command": ["node", "srv.js"], "args": ["--x"],
                        "environment": {"K": "${NYQ_MCP_TEST_UNSET_ZZ:-dflt}"}, "enabled": false},
                "rem": {"type": "remote", "url": "https://r/mcp", "header_env": {"X-Key": "NYQ_MCP_TEST_HDR"},
                        "required": true, "startup_timeout_ms": 1500, "operation_timeout_ms": 2000, "restart_limit": 3,
                        "oauth": {"client_id": "cid", "client_secret_env": "SECRET", "scopes": "a b"}},
                "badopt": {"command": "x", "startup_timeout_ms": "soon"}
            }}"#,
        )
        .unwrap();
        let (servers, warnings) = load_configs(home.path(), Path::new("/nonexistent"));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("\"servers\" is not read; did you mean \"mcpServers\"")));
        assert!(warnings.iter().any(|w| w.contains("'badopt'") && w.contains("startup_timeout_ms")));
        let Transport::Stdio { command, args, env, .. } = &servers[0].transport else { panic!() };
        assert_eq!((command.as_str(), args.as_slice(), env["K"].as_str()), ("node", &["srv.js".to_string(), "--x".into()][..], "dflt"));
        assert!(!servers[0].options.enabled);
        let o = &servers[1].options;
        let Transport::Http { headers, .. } = &servers[1].transport else { panic!() };
        assert_eq!(headers["X-Key"], "h1");
        assert!(o.required && o.enabled);
        assert_eq!((o.startup_timeout, o.operation_timeout, o.restart_limit), (Duration::from_millis(1500), Duration::from_secs(2), 3));
        assert_eq!((o.oauth.client_id.as_deref(), o.oauth.scopes.as_slice()), (Some("cid"), &["a".to_string(), "b".into()][..]));
        let d = ServerOptions::default();
        assert_eq!((d.startup_timeout, d.operation_timeout, d.restart_limit), (STARTUP_TIMEOUT, OPERATION_TIMEOUT, 1));
    }

    #[test]
    fn untrusted_project_servers_get_no_secrets() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::env::set_var("NYQ_MCP_TEST_PROJ", "leak");
        let project = r#"{"mcpServers": {
            "plain": {"command": "srv", "args": ["--port", "1"]},
            "envy": {"command": "srv", "env": {"T": "${NYQ_MCP_TEST_PROJ}"}},
            "auth": {"url": "https://x/mcp", "headers": {"Authorization": "Bearer abc"}},
            "bearer": {"url": "https://x/mcp", "bearer_token_env": "NYQ_MCP_TEST_PROJ"}
        }}"#;
        std::fs::write(cwd.path().join(".mcp.json"), project).unwrap();
        let (servers, warnings) = load_configs(home.path(), cwd.path());
        assert_eq!(servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["plain"]);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().all(|w| w.contains("nyq mcp trust approve") && !w.contains("leak")));
        // Once trusted, the same entries read the environment.
        crate::mcp_cli::trust(home.path(), cwd.path(), crate::mcp_cli::TrustAction::ApproveAll).unwrap();
        let (servers, warnings) = load_configs(home.path(), cwd.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let envy = servers.iter().find(|s| s.name == "envy").unwrap();
        let Transport::Stdio { env, .. } = &envy.transport else { panic!() };
        assert_eq!(env["T"], "leak");
    }

    #[test]
    fn env_expansion() {
        std::env::set_var("NYQ_MCP_TEST_X", "1");
        std::env::set_var("NYQ_MCP_TEST_EMPTY", "");
        assert_eq!(expand_env("a${NYQ_MCP_TEST_X}b${NYQ_MCP_TEST_X}").unwrap(), "a1b1");
        assert_eq!(expand_env("no vars $X").unwrap(), "no vars $X");
        assert_eq!(expand_env("${NYQ_MCP_TEST_NOPE_NOPE:-fallback}/x").unwrap(), "fallback/x");
        assert_eq!(expand_env("${NYQ_MCP_TEST_EMPTY:-d}${NYQ_MCP_TEST_X:-d}").unwrap(), "d1");
        let err = expand_env("${NYQ_MCP_TEST_NOPE_NOPE}").unwrap_err().to_string();
        assert!(err.contains("NYQ_MCP_TEST_NOPE_NOPE is not set") && err.contains(":-default"), "{err}");
        assert!(expand_env("${OPEN").is_err());
        assert!(expand_env("${bad name}").is_err());
    }

    #[test]
    fn docker_run_gets_a_cidfile() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (out, d) = docker_run(Path::new("/usr/bin/docker"), &args(&["run", "-i", "img"])).unwrap();
        assert_eq!(out[..2], ["run", "--cidfile"]);
        assert_eq!(out[2], d.cidfile.display().to_string());
        assert_eq!(out[3..], ["-i", "img"]);
        assert!(docker_run(Path::new("docker"), &args(&["run", "--cidfile=x", "img"])).is_none());
        assert!(docker_run(Path::new("docker"), &args(&["ps"])).is_none());
        assert!(docker_run(Path::new("npx"), &args(&["run"])).is_none());
    }

    #[test]
    fn scopes_for_sign_in() {
        let url = "https://x/mcp";
        let challenge = r#"Bearer error="insufficient_scope", scope="write""#;
        assert!(login_scopes(&[], &[], None, Some(challenge), url).is_empty(), "the SDK picks by default");
        assert_eq!(login_scopes(&["read".into()], &[], None, Some(challenge), url), ["read", "write"]);
        assert_eq!(login_scopes(&[], &["read".into()], Some("admin write"), None, url), ["read", "admin", "write"]);
        assert_eq!(insufficient_scope("OAuth: Insufficient scope: repo:write").as_deref(), Some("repo:write"));
        assert_eq!(insufficient_scope("Forbidden"), None);
        let p = |progress, total: Option<f64>, message: Option<&str>| {
            let mut p = ProgressNotificationParam::new(ProgressToken(rmcp::model::NumberOrString::Number(1)), progress);
            p.total = total;
            p.message = message.map(String::from);
            progress_text(&p)
        };
        assert_eq!(p(3.0, Some(4.0), Some("copying")), "75% · copying");
        assert_eq!(p(7.0, None, None), "7");
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(tool_name("my server", "do.it"), "mcp__my_server__do_it");
        assert_eq!(tool_name("s", &"x".repeat(100)).len(), 64);
        assert_eq!(tool_name("s", "a.b"), tool_name("s", "a/b"));
        assert_ne!(tool_name("s", &"x".repeat(80)), tool_name("s", &format!("{}y", "x".repeat(79))));
        assert_eq!(tool_prefix("my server"), "mcp__my_server__");
    }

    #[test]
    fn credential_filenames_do_not_collide() {
        let home = Path::new("/tmp/nyq-home");
        let a = credential_path(home, "team/api", "https://example.com/mcp").unwrap();
        let b = credential_path(home, "team_api", "https://example.com/mcp").unwrap();
        assert_ne!(a, b);
        assert!(a.file_name().unwrap().to_string_lossy().contains("_2f"));
        assert!(credential_path(home, &"n".repeat(300), "").is_err());
        let url = reqwest::Url::parse("https://Example.com/mcp").unwrap().to_string();
        assert_eq!(credential_path(home, "idp", &url).unwrap(), credential_path(home, "idp", "https://example.com/mcp").unwrap());
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
            options: Default::default(),
        };
        let mgr = McpManager::new(dir.path().into());
        let tools = mgr.connect(&cfg).await.unwrap();
        assert_eq!(tools.len(), 1);
        let spec = tools[0].spec();
        assert_eq!(spec.name, "mcp__fake__echo");
        assert_eq!(spec.description, "Echo the arguments");
        assert_eq!(spec.parameters["type"], "object");
        assert!(tools[0].needs_approval(&Value::Null));

        let ctx = ToolCtx { cwd: dir.path().into(), cancel: CancellationToken::new(), ..Default::default() };
        let out = tools[0].run(serde_json::json!({"text": "hi"}), &ctx).await;
        assert!(out.content.starts_with("{\"text\": \"hi\"}\n[image omitted"), "{}", out.content);
        assert!(out.images.is_empty());
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
        assert!(out.content.starts_with("{}\n[image omitted"), "{}", out.content);
        assert!(out.images.is_empty());

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
    static REVOKED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
                "revocation_endpoint": format!("{base}/revoke"),
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
            ("POST", "/revoke") if oauth => {
                if body.contains("token=") {
                    REVOKED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                (200, vec![], String::new())
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
        ServerConfig { name: name.into(), transport: Transport::Http { url, headers }, scope: Scope::Profile, options: Default::default() }
    }

    #[tokio::test]
    async fn http_connect_list_call() {
        let home = tempfile::tempdir().unwrap();
        let ctx = ToolCtx { cwd: home.path().into(), cancel: CancellationToken::new(), ..Default::default() };
        let mgr = McpManager::new(home.path().into());

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
            options: Default::default(),
        };
        assert!(mgr.login(&stdio, None, |_| {}).await.is_err());
    }

    #[tokio::test]
    async fn oauth_login_flow() {
        let home = tempfile::tempdir().unwrap();
        let ctx = ToolCtx { cwd: home.path().into(), cancel: CancellationToken::new(), ..Default::default() };
        let mgr = McpManager::new(home.path().into());
        let base = fake_http_server(true, true).await;
        let cfg = http_cfg("idp server", format!("{base}/mcp"), &[]);

        let err = mgr.connect(&cfg).await.err().unwrap();
        assert!(format!("{err:#}").contains("needs sign-in: run /mcp login idp server"), "{err:#}");

        // The "browser" just follows the authorization URL's redirects to the loopback callback.
        let page = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let p = page.clone();
        let tools = mgr
            .login(&cfg, None, move |url| {
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

        let endpoint = canonical_endpoint(&cfg.transport).unwrap();
        let file = credential_path(home.path(), "idp server", &endpoint).unwrap();
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

        assert!(mgr.logout("idp server", &endpoint).await.unwrap());
        assert!(REVOKED.load(std::sync::atomic::Ordering::SeqCst) >= 1, "logout revokes the tokens");
        assert!(mgr.connected().is_empty() && !file.exists());
        assert!(!mgr.logout("idp server", &endpoint).await.unwrap());
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

    // ---- live features: list changes, elicitation, restarts, validation ----------

    const LIVE_SERVER: &str = r#"
import json, sys
extra = False
def send(o):
    print(json.dumps(o), flush=True)
def tools():
    t = [{"name": "echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
         {"name": "change", "inputSchema": {"type": "object"}},
         {"name": "ask", "inputSchema": {"type": "object"}},
         {"name": "crash", "inputSchema": {"type": "object"}}]
    if extra:
        t.append({"name": "added", "inputSchema": {"type": "object"}})
    return t
for line in sys.stdin:
    msg = json.loads(line)
    method, mid, p = msg.get("method"), msg.get("id"), msg.get("params") or {}
    if mid is None or method is None:
        continue
    if method == "initialize":
        caps = p["capabilities"]
        result = {"protocolVersion": p["protocolVersion"],
                  "capabilities": {"tools": {"listChanged": True}, "prompts": {}, "completions": {}},
                  "serverInfo": {"name": "live", "version": "0.1"},
                  "instructions": "Use echo wisely. elicitation=" + json.dumps(caps.get("elicitation"), sort_keys=True)}
    elif method == "tools/list":
        result = {"tools": tools()}
    elif method == "prompts/list":
        result = {"prompts": [{"name": "review", "arguments": [{"name": "lang", "required": True}]}]}
    elif method == "completion/complete":
        result = {"completion": {"values": ["rust", "go"]}}
    elif method == "tools/call":
        name = p["name"]
        if name == "crash":
            sys.exit(1)
        if name == "change":
            extra = True
            send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
            text = "changed"
        elif name == "ask":
            send({"jsonrpc": "2.0", "id": "e1", "method": "elicitation/create", "params": {"mode": "form",
                  "message": "Who?", "requestedSchema": {"type": "object", "properties": {"name": {"type": "string"}}}}})
            while True:
                reply = json.loads(sys.stdin.readline())
                if reply.get("id") == "e1":
                    break
            text = json.dumps(reply["result"], sort_keys=True)
        else:
            text = json.dumps(p.get("arguments", {}), sort_keys=True)
        result = {"content": [{"type": "text", "text": text}]}
    else:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "nope"}})
        continue
    send({"jsonrpc": "2.0", "id": mid, "result": result})
"#;

    struct Answer;

    #[async_trait::async_trait]
    impl Elicitor for Answer {
        async fn form(&self, server: &str, message: &str, schema: &Value) -> crate::mcp_client::Elicited {
            assert_eq!((server, message), ("live", "Who?"));
            assert_eq!(schema["properties"]["name"]["type"], "string");
            crate::mcp_client::Elicited::Accept(serde_json::json!({"name": "Ada"}))
        }
        async fn url(&self, _: &str, _: &str, _: &str, _: &str) -> crate::mcp_client::Elicited {
            crate::mcp_client::Elicited::Decline
        }
    }

    fn python() -> String {
        if cfg!(windows) { "python" } else { "python3" }.into()
    }

    async fn until(mut done: impl FnMut() -> bool) {
        for _ in 0..250 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not reached");
    }

    #[tokio::test]
    async fn live_server_features() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("live.py");
        std::fs::write(&script, LIVE_SERVER).unwrap();
        let cfg = ServerConfig::new(
            "live",
            Transport::Stdio { command: python(), args: vec![script.display().to_string()], env: BTreeMap::new(), cwd: None },
            Scope::Profile,
        );
        let mgr = McpManager::new(dir.path().into());
        mgr.set_elicitor(std::sync::Arc::new(Answer));
        let mut reg = crate::tools::Registry::default();
        mgr.attach(&mut reg);
        mgr.start_background(vec![cfg], false);
        until(|| mgr.status("live") == Some(Status::Ready)).await;
        reg.refresh();
        let ctx = ToolCtx { cwd: dir.path().into(), cancel: CancellationToken::new(), ..Default::default() };

        // Instructions reach the registry; nyq announces form and URL elicitation.
        let notes = reg.mcp_notes();
        assert!(notes["live"].starts_with("Use echo wisely."), "{notes:?}");
        assert!(notes["live"].contains(r#""form": {"schemaValidation": true}, "url": {}"#), "{notes:?}");
        // Prompt tools come with prompts; completion suggests missing arguments.
        assert!(reg.get("mcp_list_prompts").is_some() && reg.get("mcp_get_prompt").is_some());
        let err = crate::mcp_features::prompt_turn(&mgr, "/mcp:live:review").await.unwrap_err().to_string();
        assert!(err.contains("values for lang include: rust, go"), "{err}");

        // Arguments are checked against the input schema before the call.
        let echo = reg.get("mcp__live__echo").expect("echo is installed");
        let out = echo.run(serde_json::json!({"text": 5}), &ctx).await;
        assert!(out.is_error && out.content.contains("`text` must be string (got integer)"), "{}", out.content);
        let out = echo.run(serde_json::json!({}), &ctx).await;
        assert!(out.is_error && out.content.contains("`text` is required"), "{}", out.content);
        assert_eq!(echo.run(serde_json::json!({"text": "hi"}), &ctx).await.content, r#"{"text": "hi"}"#);

        // Form elicitation goes to the elicitor and its answer back to the server.
        let out = reg.get("mcp__live__ask").unwrap().run(serde_json::json!({}), &ctx).await;
        assert_eq!(out.content, r#"{"action": "accept", "content": {"name": "Ada"}}"#);

        // tools/list_changed re-lists and patches the registry.
        assert!(reg.get("mcp__live__added").is_none());
        reg.get("mcp__live__change").unwrap().run(serde_json::json!({}), &ctx).await;
        until(|| mgr.registered_names("live").iter().any(|n| n == "mcp__live__added")).await;
        reg.refresh();
        assert!(reg.get("mcp__live__added").is_some());

        // A crash restarts the server once; the next crash is final.
        let out = reg.get("mcp__live__crash").unwrap().run(serde_json::json!({}), &ctx).await;
        assert!(out.is_error && out.content.contains("was restarted"), "{}", out.content);
        reg.refresh();
        let out = reg.get("mcp__live__echo").unwrap().run(serde_json::json!({"text": "again"}), &ctx).await;
        assert_eq!(out.content, r#"{"text": "again"}"#);
        let out = reg.get("mcp__live__crash").unwrap().run(serde_json::json!({}), &ctx).await;
        assert!(out.is_error && out.content.contains("restart limit"), "{}", out.content);
        assert!(matches!(mgr.status("live"), Some(Status::Failed(_))));
        reg.refresh();
        assert!(reg.get("mcp__live__echo").is_none(), "a dead server's tools are removed");
        mgr.shutdown().await;
    }

    #[tokio::test]
    async fn progress_reaches_the_running_call() {
        let mgr = McpManager::new(PathBuf::from("/nonexistent"));
        let token = ProgressToken(rmcp::model::NumberOrString::Number(7));
        let mut watch = mgr.watch_progress("s", &token);
        let mut p = ProgressNotificationParam::new(token.clone(), 1.0);
        p.total = Some(2.0);
        mgr.progress("s", &p);
        mgr.progress("other", &p);
        assert_eq!(watch.rx.recv().await.as_deref(), Some("50%"));
        assert!(watch.rx.try_recv().is_err());
        drop(watch);
        assert!(lock(&mgr.shared.progress).is_empty());
    }

    #[tokio::test]
    async fn failed_servers_report_their_status() {
        let mgr = McpManager::new(PathBuf::from("/nonexistent"));
        let mut cfg = ServerConfig::new(
            "gone",
            Transport::Stdio { command: "/nonexistent/nyq-no-such-server".into(), args: vec![], env: BTreeMap::new(), cwd: None },
            Scope::Profile,
        );
        cfg.options.startup_timeout = Duration::from_millis(500);
        let results = mgr.connect_all(&[cfg]).await;
        assert!(results[0].is_err());
        assert!(matches!(mgr.status("gone"), Some(Status::Failed(_))));
        assert!(Status::NeedsAuth.label().contains("sign-in"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn docker_containers_are_removed_on_close() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("server.py");
        std::fs::write(&server, FAKE_SERVER).unwrap();
        let docker = dir.path().join("docker");
        let script = format!(
            "#!/usr/bin/env python3\nimport os, sys\nargs = sys.argv[1:]\nif args[0] == 'run':\n    \
             open(args[args.index('--cidfile') + 1], 'w').write('cid-42')\n    os.execvp('python3', ['python3', {:?}])\n\
             open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'rm.log'), 'a').write(' '.join(args))\n",
            server.display().to_string()
        );
        std::fs::write(&docker, script).unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let args = vec!["run".into(), "-i".into(), "img".into()];
        let cfg = ServerConfig::new(
            "boxed",
            Transport::Stdio { command: docker.display().to_string(), args, env: BTreeMap::new(), cwd: None },
            Scope::Profile,
        );
        let mgr = McpManager::new(dir.path().into());
        assert_eq!(mgr.connect(&cfg).await.unwrap().len(), 1);
        mgr.disconnect("boxed").await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("rm.log")).unwrap(), "rm -f cid-42");
    }

    /// A legacy SSE server: `GET /sse` streams events, `POST /messages` takes requests.
    async fn fake_sse_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let stream: std::sync::Arc<tokio::sync::Mutex<Option<tokio::net::TcpStream>>> = Default::default();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let stream = stream.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    if head.starts_with("GET /sse") {
                        let open = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: endpoint\ndata: /messages?session=1\n\n";
                        let _ = sock.write_all(open).await;
                        *stream.lock().await = Some(sock);
                        return;
                    }
                    if !head.starts_with("POST /messages") {
                        let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
                        return;
                    }
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                        .unwrap_or(0);
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let _ = sock.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n").await;
                    let msg: Value = serde_json::from_slice(&buf[head_end..]).unwrap_or_default();
                    let Some(id) = msg.get("id") else { return };
                    let result = match msg["method"].as_str().unwrap_or("") {
                        "initialize" => serde_json::json!({"protocolVersion": msg["params"]["protocolVersion"],
                            "capabilities": {"tools": {}}, "serverInfo": {"name": "sse", "version": "0.1"}}),
                        "tools/list" => serde_json::json!({"tools": [{"name": "ping", "inputSchema": {"type": "object"}}]}),
                        _ => serde_json::json!({"content": [{"type": "text", "text": "pong"}]}),
                    };
                    let reply = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                    if let Some(s) = stream.lock().await.as_mut() {
                        let _ = s.write_all(format!("event: message\ndata: {reply}\n\n").as_bytes()).await;
                    }
                });
            }
        });
        base
    }

    #[tokio::test]
    async fn legacy_sse_transport() {
        let base = fake_sse_server().await;
        let cfg = ServerConfig::new("old", Transport::Sse { url: format!("{base}/sse"), headers: BTreeMap::new() }, Scope::Profile);
        let dir = tempfile::tempdir().unwrap();
        let mgr = McpManager::new(dir.path().into());
        let tools = mgr.connect(&cfg).await.unwrap();
        assert_eq!(tools[0].spec().name, "mcp__old__ping");
        let ctx = ToolCtx { cwd: dir.path().into(), cancel: CancellationToken::new(), ..Default::default() };
        assert_eq!(tools[0].run(serde_json::json!({}), &ctx).await.content, "pong");
        mgr.shutdown().await;
        let bad = ServerConfig::new("bad", Transport::Sse { url: format!("{base}/nothing"), headers: BTreeMap::new() }, Scope::Profile);
        assert!(mgr.connect(&bad).await.is_err());
    }
}
