//! `/mcp` in the interactive session: list servers and their tools, add,
//! remove and reload servers without restarting, connect, sign in, trust
//! project servers, and browse resources and prompts.

use anyhow::{bail, Context, Result};
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::mcp::{self, McpManager, Scope, ServerConfig, Transport};
use crate::ui::{self, tui::Tui};
use crate::{config, mcp_cli, mcp_features, tools};

/// Whether `cfg` connects without being asked: profile servers, and project
/// servers the user trusted (unless a profile server has the same name).
fn starts(servers: &[ServerConfig], trusted: &std::collections::HashSet<String>, cfg: &ServerConfig) -> bool {
    let profile = |name: &str| servers.iter().any(|s| s.scope == Scope::Profile && s.name == name);
    cfg.scope == Scope::Profile || (trusted.contains(&cfg.name) && !profile(&cfg.name))
}

pub async fn connect_profile_servers(mcp: &mut McpManager, agent: &mut Agent, servers: &[ServerConfig], quiet: bool) {
    let trusted = mcp_cli::trusted_project(&config::home(), &agent.cwd);
    for cfg in servers.iter().filter(|s| starts(servers, &trusted, s)) {
        connect(mcp, agent, cfg, quiet).await;
    }
    if !quiet {
        let project: Vec<_> = servers.iter().filter(|s| !starts(servers, &trusted, s)).map(|s| s.name.as_str()).collect();
        if !project.is_empty() {
            ui::info(&format!(
                "Project MCP servers in .mcp.json: {} (connect with /mcp connect NAME, or trust with /mcp trust approve NAME)",
                project.join(", ")
            ));
        }
    }
}

fn drop_server_tools(agent: &mut Agent, mcp: &McpManager, name: &str) {
    let names = mcp.registered_names(name);
    agent.tools.remove_names(&names);
    agent.tools.remove_prefix(&mcp::tool_prefix(name));
}

fn install_mcp_tools(agent: &mut Agent, name: &str, tools: Vec<Box<dyn tools::Tool>>) -> Result<()> {
    let mut added = Vec::new();
    for tool in tools {
        let n = tool.spec().name.clone();
        if let Err(e) = agent.tools.try_insert(tool) {
            agent.tools.remove_names(&added);
            agent.tools.remove_prefix(&mcp::tool_prefix(name));
            anyhow::bail!("MCP {name}: {e}");
        }
        added.push(n);
    }
    Ok(())
}

pub async fn connect(mcp: &mut McpManager, agent: &mut Agent, cfg: &ServerConfig, quiet: bool) {
    drop_server_tools(agent, mcp, &cfg.name);
    let result = mcp.connect(cfg).await;
    crate::trace::log("mcp", || match &result {
        Ok(tools) => format!("{} connected · {} tools", cfg.name, tools.len()),
        Err(e) => format!("{} failed: {e:#}", cfg.name),
    });
    match result {
        Ok(tools) => {
            let n = tools.len();
            if let Err(e) = install_mcp_tools(agent, &cfg.name, tools) {
                ui::error(&format!("{e:#}"));
                let _ = mcp.disconnect(&cfg.name).await;
            } else if !quiet {
                ui::info(&format!("MCP {}: {n} tools", cfg.name));
            }
        }
        Err(e) => ui::error(&format!("MCP {}: {e:#}", cfg.name)),
    }
}

/// Everything that decides how a server is started (values included, so a
/// changed secret counts as a change).
fn fingerprint(cfg: &ServerConfig) -> String {
    match &cfg.transport {
        Transport::Stdio { command, args, env, cwd } => format!("{:?} stdio {command:?} {args:?} {env:?} {cwd:?}", cfg.scope),
        Transport::Http { url, headers } => format!("{:?} http {url} {headers:?}", cfg.scope),
    }
}

/// Re-read the MCP configs. Servers that are gone are disconnected, changed
/// ones reconnect, new ones that start on their own connect; the rest keep
/// running untouched.
pub async fn reload(mcp: &mut McpManager, agent: &mut Agent, servers: &mut Vec<ServerConfig>) {
    let (fresh, warnings) = mcp::load_configs(&config::home(), &agent.cwd);
    for w in &warnings {
        ui::error(w);
    }
    let trusted = mcp_cli::trusted_project(&config::home(), &agent.cwd);
    let connected: Vec<String> = mcp.connected().into_iter().map(|(n, _)| n).collect();
    for name in &connected {
        let old = servers.iter().find(|s| &s.name == name).map(fingerprint);
        match fresh.iter().find(|s| &s.name == name) {
            None => {
                drop_server_tools(agent, mcp, name);
                let _ = mcp.disconnect(name).await;
                ui::info(&format!("MCP {name}: removed, disconnected"));
            }
            Some(cfg) if old.as_deref() != Some(fingerprint(cfg).as_str()) => connect(mcp, agent, cfg, false).await,
            Some(_) => {}
        }
    }
    for cfg in fresh.iter().filter(|s| !connected.contains(&s.name) && starts(&fresh, &trusted, s)) {
        connect(mcp, agent, cfg, false).await;
    }
    *servers = fresh;
}

fn find<'a>(servers: &'a [ServerConfig], name: &str) -> Result<&'a ServerConfig> {
    servers.iter().find(|s| s.name == name).with_context(|| format!("no MCP server named `{name}`"))
}

/// `/mcp add [--transport stdio|http] [--force] NAME COMMAND [ARGS…] | URL`.
fn parse_add(args: &[&str]) -> Result<(String, serde_json::Value, bool)> {
    const USAGE: &str = "usage: /mcp add NAME COMMAND [ARGS…] | /mcp add --transport http NAME URL (more options: nyq mcp add --help)";
    let mut http = false;
    let mut force = false;
    let mut i = 0;
    while let Some(flag) = args.get(i).filter(|a| a.starts_with("--")) {
        match *flag {
            "--force" => force = true,
            "--transport" => {
                i += 1;
                http = match args.get(i).copied() {
                    Some("http" | "streamable-http") => true,
                    Some("stdio") => false,
                    _ => bail!("--transport is stdio or http"),
                };
            }
            other => bail!("unknown option {other}; {USAGE}"),
        }
        i += 1;
    }
    let [name, target, rest @ ..] = &args[i..] else { bail!(USAGE) };
    let url = http || target.starts_with("http://") || target.starts_with("https://");
    let entry = if url {
        anyhow::ensure!(rest.is_empty(), "an HTTP server takes only a URL; {USAGE}");
        mcp_cli::build_entry(Some(target), &[], None, &[], None, &[])?
    } else {
        let command: Vec<String> = std::iter::once(*target).chain(rest.iter().copied()).map(String::from).collect();
        mcp_cli::build_entry(None, &[], None, &[], None, &command)?
    };
    Ok((name.to_string(), entry, force))
}

/// Lines for bare `/mcp` and `/mcp list`: each server's state and tools.
fn list_lines(agent: &Agent, mcp: &McpManager, servers: &[ServerConfig]) -> Vec<String> {
    if servers.is_empty() {
        return vec!["No MCP servers configured (/mcp add NAME COMMAND, or edit mcp.json or .mcp.json).".into()];
    }
    let connected = mcp.connected();
    let mut lines = Vec::new();
    for s in servers {
        let scope = if s.scope == Scope::Project { "project" } else { "profile" };
        let state = match connected.iter().find(|(n, _)| n == &s.name) {
            Some((_, n)) => {
                let (action, specific) = agent.permissions.server_default(&mcp::tool_prefix(&s.name));
                let more = if specific { " + tool rules" } else { "" };
                format!("connected · {n} tools · {}{more}", action.as_str())
            }
            None => "not connected".into(),
        };
        lines.push(format!("{:<20} {scope:<8} {state}", s.name));
        let prefix = mcp::tool_prefix(&s.name);
        let tools: Vec<String> = mcp
            .registered_names(&s.name)
            .iter()
            .map(|t| t.strip_prefix(&prefix).unwrap_or(t).to_string())
            .collect();
        if !tools.is_empty() {
            lines.push(format!("  tools: {}", tools.join(", ")));
        }
    }
    lines
}

/// `/mcp …`.
pub async fn command(
    agent: &mut Agent,
    mcp: &mut McpManager,
    servers: &mut Vec<ServerConfig>,
    tui: &Tui,
    arg: &[&str],
) -> Result<()> {
    let home = config::home();
    match arg {
        [] | ["list"] => ui::list(&list_lines(agent, mcp, servers)),
        ["trust", rest @ ..] => ui::list(&mcp_cli::trust_slash(&home, &agent.cwd, rest)?),
        ["path"] => ui::info(&format!("  {}", mcp_cli::config_path(&home).display())),
        ["reload"] => {
            reload(mcp, agent, servers).await;
            ui::info(&format!("Reloaded MCP config: {} server(s), {} connected.", servers.len(), mcp.connected().len()));
        }
        ["add", rest @ ..] => {
            let (name, entry, force) = parse_add(rest)?;
            mcp_cli::add_server(&home, &name, entry, force)?;
            ui::info(&format!("Added {name} to {}.", mcp_cli::config_path(&home).display()));
            reload(mcp, agent, servers).await;
        }
        ["remove" | "rm", name] => {
            if !mcp_cli::remove_server(&home, name)? {
                bail!("no MCP server named `{name}` in {}", mcp_cli::config_path(&home).display());
            }
            ui::info(&format!("Removed {name} from {}.", mcp_cli::config_path(&home).display()));
            reload(mcp, agent, servers).await;
        }
        ["resource" | "resources", rest @ ..] => ui::list(&mcp_features::resource_command(mcp, rest).await?),
        ["prompt" | "prompts", rest @ ..] => ui::list(&mcp_features::prompt_command(mcp, rest).await?),
        ["connect", name] => {
            let cfg = find(servers, name)?.clone();
            connect(mcp, agent, &cfg, false).await;
        }
        ["login", name] => {
            let cfg = find(servers, name)?.clone();
            drop_server_tools(agent, mcp, name);
            let cancel = CancellationToken::new();
            tui.screen().set_busy(Some("Signing in"), Some(cancel.clone()));
            let result = mcp
                .login(&cfg, Some(&cancel), |url| {
                    ui::info(&format!("Sign in to {name} in your browser (waiting up to 5 minutes):\n  {url}"));
                    mcp::open_browser(url);
                })
                .await;
            tui.screen().idle();
            let tools = result?;
            let n = tools.len();
            install_mcp_tools(agent, name, tools)?;
            ui::info(&format!("MCP {name}: signed in · {n} tools"));
        }
        ["logout", name] => {
            let endpoint = mcp::canonical_endpoint(&find(servers, name)?.transport)?;
            drop_server_tools(agent, mcp, name);
            let existed = mcp.logout(name, &endpoint).await?;
            ui::info(&if existed { format!("Signed out of {name}.") } else { format!("No saved sign-in for {name}.") });
        }
        ["disconnect", name] => {
            drop_server_tools(agent, mcp, name);
            mcp.disconnect(name).await?;
            ui::info(&format!("Disconnected {name}."));
        }
        _ => bail!(
            "usage: /mcp [list|reload|path] · add NAME COMMAND [ARGS] · add --transport http NAME URL · remove NAME · \
             connect|disconnect|login|logout NAME · trust … · resource … · prompt …"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_arguments_make_stdio_and_http_entries() {
        let (name, entry, force) = parse_add(&["fs", "npx", "-y", "server"]).unwrap();
        assert_eq!((name.as_str(), force), ("fs", false));
        assert_eq!(entry, serde_json::json!({"command": "npx", "args": ["-y", "server"]}));
        let (_, entry, force) = parse_add(&["--force", "--transport", "http", "wiki", "https://x.dev/mcp"]).unwrap();
        assert!(force);
        assert_eq!(entry, serde_json::json!({"url": "https://x.dev/mcp"}));
        let (_, entry, _) = parse_add(&["wiki", "http://localhost:3000/mcp"]).unwrap();
        assert_eq!(entry["url"], "http://localhost:3000/mcp");
        assert!(parse_add(&["only-name"]).is_err());
        assert!(parse_add(&["--transport", "http", "w", "https://x", "extra"]).is_err());
        assert!(parse_add(&["--bogus", "a", "b"]).is_err());
    }

    #[test]
    fn fingerprints_see_secret_changes() {
        let cfg = |token: &str| ServerConfig {
            name: "w".into(),
            transport: Transport::Http {
                url: "https://x".into(),
                headers: [("Authorization".to_string(), token.to_string())].into(),
            },
            scope: Scope::Profile,
        };
        assert_eq!(fingerprint(&cfg("a")), fingerprint(&cfg("a")));
        assert_ne!(fingerprint(&cfg("a")), fingerprint(&cfg("b")));
    }
}
