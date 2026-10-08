//! `nyq mcp …`: manage MCP servers in `<home>/mcp.json`, their OAuth
//! sign-ins, and trust for a project's `.mcp.json` servers, without the TUI.
//!
//! Trust: `nyq mcp trust approve NAME` records the project server's config
//! hash in `<home>/trusted-mcp.json` (keyed by the project file's path).
//! Trusted project servers connect at startup like profile servers; editing
//! the entry makes it untrusted again.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::mcp::{self, McpManager, Scope, Transport};

#[derive(clap::Subcommand)]
pub enum McpCommand {
    /// List configured servers (profile and project) without connecting.
    List,
    /// Add a server to <home>/mcp.json: `nyq mcp add NAME -- CMD [ARGS…]`
    /// (stdio) or `nyq mcp add NAME --url URL` (streamable HTTP).
    Add {
        name: String,
        /// Streamable HTTP endpoint.
        #[arg(long)]
        url: Option<String>,
        /// HTTP header, `NAME=VALUE` (values may use ${ENV}). Repeatable.
        #[arg(long = "header", value_name = "NAME=VALUE")]
        headers: Vec<String>,
        /// Send `Authorization: Bearer $VAR` (HTTP).
        #[arg(long, value_name = "VAR")]
        bearer_token_env: Option<String>,
        /// Environment variable for a stdio server, `KEY=VALUE`. Repeatable.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Working directory for a stdio server.
        #[arg(long)]
        cwd: Option<String>,
        /// Replace an existing server with the same name.
        #[arg(long)]
        force: bool,
        /// The stdio command and its arguments (after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Remove a server from <home>/mcp.json.
    Remove { name: String },
    /// Sign in to an HTTP server with OAuth (opens your browser).
    Login { name: String },
    /// Forget a server's saved OAuth sign-in.
    Logout { name: String },
    /// Trust the project's .mcp.json servers so they connect at startup.
    Trust {
        #[command(subcommand)]
        action: Option<TrustAction>,
    },
    /// Print the profile MCP config path.
    Path,
}

#[derive(clap::Subcommand, Debug, PartialEq, Eq)]
pub enum TrustAction {
    /// Show each project server and whether it is trusted.
    List,
    /// Trust one project server (its current configuration).
    Approve { name: String },
    /// Stop trusting one project server.
    Reject { name: String },
    /// Trust every server in the project's .mcp.json.
    ApproveAll,
    /// Forget all trust for this project.
    Reset,
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join("mcp.json")
}

fn read_root(path: &Path) -> Result<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))? {
            Value::Object(m) => Ok(m),
            _ => bail!("{} must contain a JSON object", path.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn servers_mut(root: &mut Map<String, Value>) -> Result<&mut Map<String, Value>> {
    root.entry("mcpServers").or_insert_with(|| json!({})).as_object_mut().ok_or_else(|| anyhow!("\"mcpServers\" must be an object"))
}

fn write_root(path: &Path, root: &Map<String, Value>) -> Result<()> {
    let mut text = serde_json::to_string_pretty(root)?;
    text.push('\n');
    crate::config::write_private(path, text.as_bytes())
}

fn check_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "invalid server name `{name}` (use letters, digits, `-` and `_`)"
    );
    Ok(())
}

fn pairs(items: &[String], what: &str) -> Result<Map<String, Value>> {
    items
        .iter()
        .map(|item| {
            let (k, v) = item.split_once('=').filter(|(k, _)| !k.trim().is_empty()).ok_or_else(|| anyhow!("{what} `{item}` must be NAME=VALUE"))?;
            Ok((k.trim().to_string(), json!(v)))
        })
        .collect()
}

/// The mcp.json entry for `nyq mcp add`.
pub fn build_entry(url: Option<&str>, headers: &[String], bearer: Option<&str>, env: &[String], cwd: Option<&str>, command: &[String]) -> Result<Value> {
    let mut entry = Map::new();
    match url {
        Some(url) => {
            anyhow::ensure!(command.is_empty(), "give either --url or a command, not both");
            anyhow::ensure!(env.is_empty() && cwd.is_none(), "--env and --cwd are for stdio servers");
            let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid URL `{url}`"))?;
            anyhow::ensure!(matches!(parsed.scheme(), "http" | "https"), "the URL must be http:// or https://");
            entry.insert("url".into(), json!(url));
            let headers = pairs(headers, "header")?;
            for k in headers.keys() {
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).map_err(|_| anyhow!("invalid header name `{k}`"))?;
            }
            if !headers.is_empty() {
                entry.insert("headers".into(), Value::Object(headers));
            }
            if let Some(var) = bearer {
                entry.insert("bearer_token_env".into(), json!(var));
            }
        }
        None => {
            let (program, args) = command.split_first().ok_or_else(|| anyhow!("give a command (`nyq mcp add NAME -- CMD ARGS…`) or --url URL"))?;
            anyhow::ensure!(headers.is_empty() && bearer.is_none(), "--header and --bearer-token-env are for HTTP servers");
            entry.insert("command".into(), json!(program));
            if !args.is_empty() {
                entry.insert("args".into(), json!(args));
            }
            let env = pairs(env, "--env")?;
            if !env.is_empty() {
                entry.insert("env".into(), Value::Object(env));
            }
            if let Some(dir) = cwd {
                entry.insert("cwd".into(), json!(dir));
            }
        }
    }
    Ok(Value::Object(entry))
}

/// Add (or with `force`, replace) a server, keeping everything else in the file.
pub fn add_server(home: &Path, name: &str, entry: Value, force: bool) -> Result<()> {
    check_name(name)?;
    let path = config_path(home);
    let mut root = read_root(&path)?;
    let servers = servers_mut(&mut root)?;
    anyhow::ensure!(force || !servers.contains_key(name), "MCP server `{name}` already exists in {} (use --force to replace it)", path.display());
    servers.insert(name.to_string(), entry);
    write_root(&path, &root)
}

/// Remove a server; false if it wasn't there.
pub fn remove_server(home: &Path, name: &str) -> Result<bool> {
    let path = config_path(home);
    let mut root = read_root(&path)?;
    if servers_mut(&mut root)?.remove(name).is_none() {
        return Ok(false);
    }
    write_root(&path, &root)?;
    Ok(true)
}

// ------------------------------------------------------------------- trust

fn trust_path(home: &Path) -> PathBuf {
    home.join("trusted-mcp.json")
}

/// The project's `.mcp.json` (its trust-store key) and its server entries.
fn project_entries(cwd: &Path) -> Result<(String, Map<String, Value>)> {
    let path = cwd.join(".mcp.json");
    let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone()).to_string_lossy().into_owned();
    let mut root = read_root(&path)?;
    let servers = match root.remove("mcpServers") {
        Some(Value::Object(m)) => m,
        None => Map::new(),
        Some(_) => bail!("{}: \"mcpServers\" must be an object", path.display()),
    };
    Ok((key, servers))
}

fn entry_hash(entry: &Value) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(serde_json::to_vec(entry).unwrap_or_default());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

type TrustStore = BTreeMap<String, BTreeMap<String, String>>;

fn read_trust(home: &Path) -> Result<TrustStore> {
    let path = trust_path(home);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustStore::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Names of this project's servers whose current configuration is trusted.
pub fn trusted_project(home: &Path, cwd: &Path) -> HashSet<String> {
    let (Ok((key, entries)), Ok(store)) = (project_entries(cwd), read_trust(home)) else {
        return HashSet::new();
    };
    let Some(trusted) = store.get(&key) else { return HashSet::new() };
    entries.iter().filter(|(n, e)| trusted.get(*n).is_some_and(|h| *h == entry_hash(e))).map(|(n, _)| n.clone()).collect()
}

/// Apply a trust action; returns lines to show.
pub fn trust(home: &Path, cwd: &Path, action: TrustAction) -> Result<Vec<String>> {
    let (key, entries) = project_entries(cwd)?;
    let mut store = read_trust(home)?;
    let mut project = store.remove(&key).unwrap_or_default();
    let find = |name: &str| entries.get(name).ok_or_else(|| anyhow!("no server `{name}` in {}", cwd.join(".mcp.json").display()));
    let msg = match &action {
        TrustAction::List => {
            if entries.is_empty() {
                return Ok(vec![format!("No project MCP servers ({} has none).", cwd.join(".mcp.json").display())]);
            }
            let trusted = trusted_project(home, cwd);
            return Ok(entries
                .keys()
                .map(|n| {
                    let state = match project.get(n) {
                        _ if trusted.contains(n) => "trusted",
                        Some(_) => "changed since trusted",
                        None => "not trusted",
                    };
                    format!("  {n:<20} {state}")
                })
                .collect());
        }
        TrustAction::Approve { name } => {
            project.insert(name.clone(), entry_hash(find(name)?));
            format!("Trusted project MCP server `{name}`; it connects when nyq starts here.")
        }
        TrustAction::Reject { name } => {
            find(name)?;
            project.remove(name);
            format!("Project MCP server `{name}` is no longer trusted.")
        }
        TrustAction::ApproveAll => {
            anyhow::ensure!(!entries.is_empty(), "no project MCP servers to trust");
            project = entries.iter().map(|(n, e)| (n.clone(), entry_hash(e))).collect();
            format!("Trusted {} project MCP server(s): {}.", entries.len(), entries.keys().cloned().collect::<Vec<_>>().join(", "))
        }
        TrustAction::Reset => {
            project.clear();
            "Forgot MCP trust for this project.".to_string()
        }
    };
    if !project.is_empty() {
        store.insert(key, project);
    }
    let mut text = serde_json::to_string_pretty(&store)?;
    text.push('\n');
    crate::config::write_private(&trust_path(home), text.as_bytes())?;
    Ok(vec![msg])
}

/// `/mcp trust [approve NAME|reject NAME|approve-all|reset]`.
pub fn trust_slash(home: &Path, cwd: &Path, args: &[&str]) -> Result<Vec<String>> {
    let action = match args {
        [] | ["list"] => TrustAction::List,
        ["approve", name] => TrustAction::Approve { name: name.to_string() },
        ["reject", name] => TrustAction::Reject { name: name.to_string() },
        ["approve-all"] => TrustAction::ApproveAll,
        ["reset"] => TrustAction::Reset,
        _ => bail!("usage: /mcp trust [list|approve NAME|reject NAME|approve-all|reset]"),
    };
    trust(home, cwd, action)
}

// --------------------------------------------------------------------- CLI

fn describe(t: &Transport) -> String {
    match t {
        Transport::Stdio { command, args, .. } => format!("stdio: {} {}", command, args.join(" ")).trim_end().to_string(),
        Transport::Http { url, .. } => format!("http: {url}"),
    }
}

fn list_lines(home: &Path, cwd: &Path) -> Vec<String> {
    let (servers, warnings) = mcp::load_configs(home, cwd);
    for w in &warnings {
        crate::ui::error(w);
    }
    if servers.is_empty() {
        return vec![format!("No MCP servers configured. Add one with `nyq mcp add NAME -- CMD ARGS…` or `nyq mcp add NAME --url URL` ({}).", config_path(home).display())];
    }
    let trusted = trusted_project(home, cwd);
    servers
        .iter()
        .map(|s| {
            let mut notes = Vec::new();
            if s.scope == Scope::Project {
                notes.push(if trusted.contains(&s.name) { "trusted" } else { "not trusted" });
            }
            let signed_in = mcp::canonical_endpoint(&s.transport)
                .ok()
                .filter(|e| !e.is_empty())
                .and_then(|e| mcp::credential_path(home, &s.name, &e).ok())
                .is_some_and(|p| p.exists());
            if signed_in {
                notes.push("signed in");
            }
            let scope = if s.scope == Scope::Project { "project" } else { "profile" };
            format!("{:<20} {scope:<8} {}{}", s.name, describe(&s.transport), if notes.is_empty() { String::new() } else { format!("  ({})", notes.join(", ")) })
        })
        .collect()
}

fn find(home: &Path, cwd: &Path, name: &str) -> Result<mcp::ServerConfig> {
    let (servers, warnings) = mcp::load_configs(home, cwd);
    servers.into_iter().find(|s| s.name == name).ok_or_else(|| {
        let why = warnings.iter().find(|w| w.contains(&format!("'{name}'"))).map(|w| format!(": {w}")).unwrap_or_default();
        anyhow!("no MCP server named `{name}`{why}")
    })
}

pub async fn run(home: &Path, cwd: &Path, command: McpCommand) -> Result<()> {
    match command {
        McpCommand::List => list_lines(home, cwd).iter().for_each(|l| println!("{l}")),
        McpCommand::Path => println!("{}", config_path(home).display()),
        McpCommand::Add { name, url, headers, bearer_token_env, env, cwd: dir, force, command } => {
            let entry = build_entry(url.as_deref(), &headers, bearer_token_env.as_deref(), &env, dir.as_deref(), &command)?;
            add_server(home, &name, entry, force)?;
            println!("Added MCP server `{name}` to {}.", config_path(home).display());
        }
        McpCommand::Remove { name } => {
            anyhow::ensure!(remove_server(home, &name)?, "no MCP server named `{name}` in {}", config_path(home).display());
            println!("Removed MCP server `{name}`.");
        }
        McpCommand::Login { name } => {
            let cfg = find(home, cwd, &name)?;
            anyhow::ensure!(matches!(cfg.transport, Transport::Http { .. }), "`{name}` is a stdio server; only HTTP servers sign in");
            let mut manager = McpManager::new(home.to_path_buf());
            let result = manager
                .login(&cfg, None, |url| {
                    eprintln!("Sign in to {name} in your browser (waiting up to 5 minutes):\n  {url}");
                    mcp::open_browser(url);
                })
                .await;
            manager.shutdown().await;
            println!("Signed in to {name} · {} tools.", result?.len());
        }
        McpCommand::Logout { name } => {
            let cfg = find(home, cwd, &name)?;
            let endpoint = mcp::canonical_endpoint(&cfg.transport)?;
            let existed = McpManager::new(home.to_path_buf()).logout(&name, &endpoint).await?;
            println!("{}", if existed { format!("Signed out of {name}.") } else { format!("No saved sign-in for {name}.") });
        }
        McpCommand::Trust { action } => trust(home, cwd, action.unwrap_or(TrustAction::List))?.iter().for_each(|l| println!("{l}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn add_remove_round_trip_keeps_other_entries() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(
            config_path(home),
            r#"{"mcpServers":{"keep":{"command":"keep-cmd","args":["-x"],"custom":true}},"other":{"a":1}}"#,
        )
        .unwrap();
        let stdio = build_entry(None, &[], None, &["TOKEN=a=b".into()], Some("sub"), &["npx".into(), "-y".into(), "srv".into()]).unwrap();
        assert_eq!(stdio, json!({"command":"npx","args":["-y","srv"],"env":{"TOKEN":"a=b"},"cwd":"sub"}));
        add_server(home, "fs", stdio, false).unwrap();
        let http = build_entry(Some("https://mcp.example.com/mcp"), &["X-Team=${TEAM}".into()], Some("TOK"), &[], None, &[]).unwrap();
        add_server(home, "web", http, false).unwrap();
        assert!(add_server(home, "fs", json!({"command":"x"}), false).unwrap_err().to_string().contains("already exists"));
        add_server(home, "fs", json!({"command":"y"}), true).unwrap();

        let v = read(&config_path(home));
        assert_eq!(v["other"], json!({"a":1}));
        assert_eq!(v["mcpServers"]["keep"], json!({"command":"keep-cmd","args":["-x"],"custom":true}));
        assert_eq!(v["mcpServers"]["fs"], json!({"command":"y"}));
        assert_eq!(v["mcpServers"]["web"], json!({"url":"https://mcp.example.com/mcp","headers":{"X-Team":"${TEAM}"},"bearer_token_env":"TOK"}));

        assert!(remove_server(home, "fs").unwrap());
        assert!(!remove_server(home, "fs").unwrap());
        let v = read(&config_path(home));
        assert_eq!(v["mcpServers"].as_object().unwrap().keys().collect::<Vec<_>>(), ["keep", "web"]);
        assert_eq!(v["other"], json!({"a":1}));
        // The file still loads as nyq's MCP config.
        let (servers, _) = mcp::load_configs(home, &home.join("nowhere"));
        assert_eq!(servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["keep"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(config_path(home)).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn entry_validation() {
        assert!(build_entry(None, &[], None, &[], None, &[]).is_err());
        assert!(build_entry(Some("ftp://x"), &[], None, &[], None, &[]).is_err());
        assert!(build_entry(Some("https://x"), &[], None, &[], None, &["cmd".into()]).is_err());
        assert!(build_entry(Some("https://x"), &["bad header=1".into()], None, &[], None, &[]).is_err());
        assert!(build_entry(None, &["H=1".into()], None, &[], None, &["cmd".into()]).is_err());
        assert!(build_entry(None, &[], None, &["NOEQUALS".into()], None, &["cmd".into()]).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(add_server(dir.path(), "bad name", json!({"command":"x"}), false).is_err());
        std::fs::write(config_path(dir.path()), "not json").unwrap();
        assert!(add_server(dir.path(), "ok", json!({"command":"x"}), false).is_err());
        assert_eq!(std::fs::read_to_string(config_path(dir.path())).unwrap(), "not json");
    }

    #[test]
    fn project_trust_follows_config_hash() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let (h, p) = (home.path(), project.path());
        std::fs::write(p.join(".mcp.json"), r#"{"mcpServers":{"a":{"command":"a"},"b":{"command":"b"}}}"#).unwrap();
        assert!(trusted_project(h, p).is_empty());
        trust(h, p, TrustAction::Approve { name: "a".into() }).unwrap();
        assert_eq!(trusted_project(h, p), HashSet::from(["a".to_string()]));
        assert!(trust(h, p, TrustAction::Approve { name: "zzz".into() }).is_err());
        trust(h, p, TrustAction::ApproveAll).unwrap();
        assert_eq!(trusted_project(h, p).len(), 2);
        // Editing an entry revokes its trust.
        std::fs::write(p.join(".mcp.json"), r#"{"mcpServers":{"a":{"command":"evil"},"b":{"command":"b"}}}"#).unwrap();
        assert_eq!(trusted_project(h, p), HashSet::from(["b".to_string()]));
        let list = trust(h, p, TrustAction::List).unwrap();
        assert!(list[0].contains("changed since trusted") && list[1].contains("trusted"));
        trust_slash(h, p, &["reject", "b"]).unwrap();
        assert!(trusted_project(h, p).is_empty());
        trust_slash(h, p, &["approve-all"]).unwrap();
        trust_slash(h, p, &["reset"]).unwrap();
        assert!(trusted_project(h, p).is_empty());
        assert!(read(&trust_path(h)).as_object().unwrap().is_empty());
        assert!(trust_slash(h, p, &["bogus"]).is_err());
    }
}
