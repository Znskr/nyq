//! Read-only status snapshots and bounded health checks; credentials never enter output.
use crate::{config, mcp, provider::Provider, sandbox, skills, ui};
use anyhow::Result;
use serde_json::{json, Value};
use std::{
    io::{IsTerminal, Write},
    path::Path,
};

pub fn version() {
    ui::info(&format!("nyq {}", env!("CARGO_PKG_VERSION")));
}
fn auth_sources(home: &Path) -> Vec<String> {
    let mut sources = Vec::new();
    if std::env::var("NYQ_API_KEY").is_ok_and(|v| !v.trim().is_empty()) {
        sources.push("NYQ_API_KEY".into());
    }
    if std::fs::read(home.join("model.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v["api_key"].as_str().is_some_and(|k| !k.trim().is_empty()))
    {
        sources.push("model.json api_key".into());
    }
    for service in crate::provider::hosted::Service::ALL {
        if let Some(source) = crate::provider::hosted::key_source(home, service) {
            sources.push(format!("{} key ({source})", service.title()));
        }
    }
    for (label, path) in [
        ("selected Codex auth", config::codex_auth_path(home)),
        ("profile ChatGPT auth", home.join("chatgpt-auth.json")),
        ("Grok subscription", crate::provider::grok::auth_path(home)),
        (
            "Codex CLI auth",
            dirs::home_dir()
                .unwrap_or_default()
                .join(".codex/auth.json"),
        ),
    ] {
        if path.is_file() {
            sources.push(label.into());
        }
    }
    sources
}
pub fn status(
    home: &Path,
    cwd: &Path,
    provider: Option<&dyn Provider>,
    policy: &sandbox::Policy,
    context: u64,
    mcp_state: (&[mcp::ServerConfig], &[(String, usize)]),
    skills_count: usize,
) -> Value {
    let (servers, connected) = mcp_state;
    json!({"version": env!("CARGO_PKG_VERSION"), "provider": provider.map(Provider::name), "model": provider.map(Provider::model),
        "effort": provider.and_then(Provider::effort).unwrap_or("auto"), "profile_dir": home,
        "auth_sources": auth_sources(home), "sandbox": sandbox::describe(policy),
        "mcp": servers.iter().map(|s| json!({"name": s.name, "connected": connected.iter().any(|(name, _)| name == &s.name)})).collect::<Vec<_>>(),
        "skills_count": skills_count, "permissions_file": crate::permissions::user_path(home),
        "project_permissions_file": crate::permissions::project_path(cwd), "context_window": context,
        "max_agent_steps": crate::agent::step_limit().0, "trace": crate::trace::describe(),
        "system_prompt_files": crate::prompt::prompt_files(cwd, home),
        "disabled_tools": crate::settings::current().disabled_tools,
        "disabled_skills": crate::settings::current().disabled_skills,
        "context_limits": crate::limits::report()})
}
pub fn show_status(value: &Value, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        let mut lines = vec![format!(
            "nyq {}",
            value["version"].as_str().unwrap_or_default()
        )];
        for key in [
            "provider",
            "model",
            "effort",
            "profile_dir",
            "auth_sources",
            "sandbox",
            "mcp",
            "skills_count",
            "permissions_file",
            "project_permissions_file",
            "context_window",
            "max_agent_steps",
            "trace",
            "system_prompt_files",
            "disabled_tools",
            "disabled_skills",
        ] {
            let v = &value[key];
            lines.push(format!(
                "  {key}: {}",
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            ));
        }
        // Only limits that differ from their defaults.
        let changed: Vec<String> = value["context_limits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|l| l["source"] != "default")
            .map(|l| format!("{}={} ({})", l["name"].as_str().unwrap_or_default(), l["bytes"].as_u64().map_or("off".into(), |b| b.to_string()), l["source"].as_str().unwrap_or_default()))
            .collect();
        if !changed.is_empty() {
            lines.push(format!("  context_limits: {}", changed.join(", ")));
        }
        ui::list(&lines);
    }
    Ok(())
}
pub fn show_agent_status(
    agent: &crate::agent::Agent,
    manager: &mcp::McpManager,
    servers: &[mcp::ServerConfig],
    skills_count: usize,
) -> Result<()> {
    let value = status(
        &config::home(),
        &agent.cwd,
        Some(agent.provider.as_ref()),
        &agent.sandbox,
        agent.context_window,
        (servers, &manager.connected()),
        skills_count,
    );
    let mut value = value;
    // Live state of each server (connecting, ready, needs sign-in, failed).
    if let Some(list) = value["mcp"].as_array_mut() {
        for entry in list {
            let name = entry["name"].as_str().unwrap_or_default().to_string();
            let state = manager.status(&name).map_or_else(|| "not started".to_string(), |s| s.label());
            entry["state"] = json!(state);
        }
    }
    show_status(&value, false)
}

#[derive(Debug)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
    hint: &'static str,
    /// Informational checks warn (`!`) instead of failing doctor.
    required: bool,
}
impl Check {
    fn line(&self) -> String {
        format!(
            "{} {}: {}{}",
            if self.ok { "✓" } else if self.required { "✗" } else { "!" },
            self.name,
            self.detail,
            if self.ok || self.hint.is_empty() {
                String::new()
            } else {
                format!(" — {}", self.hint)
            }
        )
    }
}
fn check(name: &'static str, ok: bool, detail: impl Into<String>, hint: &'static str) -> Check {
    Check {
        name,
        ok,
        detail: detail.into(),
        hint,
        required: true,
    }
}
fn codex_auth_valid(home: &Path) -> bool {
    let Some(v) = std::fs::read(config::codex_auth_path(home))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return false;
    };
    let token = v["access_token"]
        .as_str()
        .or_else(|| v["tokens"]["access_token"].as_str());
    let expiry = v["expires_at_ms"]
        .as_i64()
        .or_else(|| token.and_then(crate::provider::codex::jwt_exp_ms));
    token.is_some_and(|t| !t.is_empty())
        && expiry
            .is_some_and(|e| config::access_token_fresh(e, chrono::Utc::now().timestamp_millis()))
}
fn writable(home: &Path) -> bool {
    let path = home.join(format!(".doctor-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(home)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&path)?;
        file.write_all(b"nyq")?;
        file.sync_all()
    })();
    let _ = std::fs::remove_file(path);
    result.is_ok()
}
async fn reachable(endpoint: &str) -> bool {
    // Bound the entire HEAD/GET probe, including fallback, to five seconds.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let Ok(client) = crate::http::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
        else {
            return false;
        };
        let response = client.head(endpoint).send().await;
        match response {
            Ok(r) if r.status() != reqwest::StatusCode::METHOD_NOT_ALLOWED => true,
            _ => client.get(endpoint).send().await.is_ok(),
        }
    })
    .await
    .unwrap_or(false)
}
/// Run-time facts doctor reports alongside its checks.
pub struct Facts<'a> {
    pub permission_mode: crate::permissions::Mode,
    pub sessions_dir: &'a Path,
}

/// Whether `program --version` runs within five seconds.
async fn runs(program: &str) -> bool {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(program)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .is_some_and(|o| o.status.success())
}

fn info(name: &'static str, ok: bool, detail: impl Into<String>, hint: &'static str) -> Check {
    Check {
        required: false,
        ..check(name, ok, detail, hint)
    }
}

pub async fn doctor(
    home: &Path,
    cwd: &Path,
    provider: Option<&dyn Provider>,
    policy: &sandbox::Policy,
    facts: &Facts<'_>,
    json: bool,
) -> Result<()> {
    let configured = provider.is_some();
    let auth = match provider.map(Provider::name) {
        Some("codex") => codex_auth_valid(home),
        Some("grok") => crate::provider::grok::has_session(home),
        Some("ollama") => true,
        Some(name) if crate::provider::hosted::Service::parse(name).is_some() => {
            crate::provider::hosted::Service::parse(name)
                .is_some_and(|s| crate::provider::hosted::api_key(home, s).is_some())
        }
        Some(_) => config::openai_config(home)
            .ok()
            .is_some_and(|(url, key, _)| {
                key.is_some_and(|k| {
                    !k.trim().is_empty()
                        && crate::provider::codex::jwt_exp_ms(&k).is_none_or(|e| {
                            config::access_token_fresh(e, chrono::Utc::now().timestamp_millis())
                        })
                }) || url
                    .and_then(|u| reqwest::Url::parse(&u).ok())
                    .is_some_and(|u| {
                        matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
                    })
            }),
        None => false,
    };
    let mut checks = vec![
        check(
            "model configuration",
            configured,
            if configured {
                "provider and model selected"
            } else {
                "no usable model configuration"
            },
            "set NYQ_BASE_URL and NYQ_MODEL, or run nyq login",
        ),
        check(
            "auth",
            auth,
            if auth {
                "credentials valid, or local endpoint needs no key"
            } else {
                "credentials missing, invalid or expired"
            },
            "run nyq login, or set NYQ_API_KEY; refresh expired tokens",
        ),
    ];
    let endpoint = provider.and_then(Provider::endpoint);
    let endpoint_ok = match endpoint {
        Some(url) => reachable(&url).await,
        None => false,
    };
    checks.push(check(
        "model endpoint",
        endpoint_ok,
        if endpoint_ok {
            "HTTP endpoint reachable"
        } else {
            "endpoint unavailable"
        },
        "check endpoint configuration and network access",
    ));
    let available = sandbox::check(policy).is_ok();
    checks.push(check(
        "sandbox",
        available,
        sandbox::describe(policy),
        "install or enable the platform sandbox, or select --sandbox off",
    ));
    checks.push(check(
        "profile",
        writable(home),
        "profile directory write probe",
        "check profile directory permissions and available disk space",
    ));
    let (_, errors) = mcp::load_configs(home, cwd);
    checks.push(check(
        "MCP config",
        errors.is_empty(),
        if errors.is_empty() {
            "configs parse"
        } else {
            "invalid config or missing environment variable"
        },
        "check mcp.json and .mcp.json syntax and referenced environment variables",
    ));
    let (_, settings_warning) = crate::settings::load(home);
    checks.push(info(
        "settings",
        settings_warning.is_none(),
        settings_warning.unwrap_or_else(|| "settings.toml parses (or is absent)".into()),
        "fix settings.toml; nyq uses the defaults until then",
    ));
    let disabled = crate::settings::current().disabled_tools;
    let unknown = crate::tools::unknown_tool_names(&disabled);
    checks.push(info(
        "disabled tools",
        unknown.is_empty(),
        match (disabled.is_empty(), unknown.is_empty()) {
            (true, _) => "none".to_string(),
            (false, true) => disabled.join(", "),
            (false, false) => format!("unknown: {}", unknown.join(", ")),
        },
        "check the names in disabled_tools against /tools",
    ));
    let color_warnings = crate::ui::theme::color_warnings(&crate::settings::current().theme_colors);
    checks.push(info(
        "theme colors",
        color_warnings.is_empty(),
        if color_warnings.is_empty() {
            "theme_colors valid (or unset)".into()
        } else {
            color_warnings.join("; ")
        },
        "fix theme_colors; the entries listed keep their built-in colors",
    ));
    let (_, key_warnings) = crate::ui::keys::load(home);
    checks.push(info(
        "keybindings",
        key_warnings.is_empty(),
        if key_warnings.is_empty() { "keybindings.toml parses (or is absent)".into() } else { key_warnings.join("; ") },
        "fix keybindings.toml; nyq skips the entries it warned about",
    ));
    checks.push(sessions_check(facts.sessions_dir));
    let (steps, steps_source) = crate::agent::step_limit();
    checks.push(info(
        "agent",
        true,
        format!(
            "permission mode {} · step limit {steps} ({steps_source})",
            facts.permission_mode.as_str()
        ),
        "",
    ));
    let git = runs("git").await;
    checks.push(check(
        "git",
        git,
        if git {
            "git available"
        } else {
            "git unavailable"
        },
        "install git and add it to PATH",
    ));
    let repo = git
        && tokio::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .is_ok_and(|o| o.status.success());
    checks.push(info(
        "git repository",
        repo,
        if repo {
            "the working directory is in a git repository"
        } else {
            "not a git repository"
        },
        "nyq pr and nyq issue need a git repository",
    ));
    let gh = runs("gh").await;
    checks.push(info(
        "gh",
        gh,
        if gh { "GitHub CLI available" } else { "GitHub CLI unavailable" },
        "install gh to publish with nyq pr and nyq issue",
    ));
    let terminal = std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && std::env::var("TERM").map_or(true, |v| v != "dumb");
    checks.push(Check { required: false, ..check(
        "terminal",
        terminal,
        if terminal {
            "interactive terminal available"
        } else {
            "interactive terminal unavailable"
        },
        "run in an interactive terminal with TERM set appropriately; print mode remains available",
    ) });
    let passed = checks.iter().all(|c| c.ok || !c.required);
    if json {
        let list: Vec<Value> = checks
            .iter()
            .map(|c| json!({"name": c.name, "ok": c.ok, "required": c.required, "detail": c.detail, "hint": c.hint}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&json!({"ok": passed, "checks": list}))?);
        if !passed {
            // The JSON already says what failed.
            std::process::exit(1);
        }
        return Ok(());
    }
    ui::list(&checks.iter().map(Check::line).collect::<Vec<_>>());
    anyhow::ensure!(passed, "doctor found failing checks");
    Ok(())
}
/// Saved sessions: how many, and which need repair (with the command for
/// each) or are large enough to load slowly.
fn sessions_check(dir: &Path) -> Check {
    let (count, problems) = crate::session::health(dir);
    info(
        "sessions",
        problems.is_empty(),
        if problems.is_empty() {
            format!("{count} saved, all readable")
        } else {
            format!("{count} saved, {} need attention: {}", problems.len(), problems.join("; "))
        },
        "repair with the command shown for each session",
    )
}
pub async fn models(home: &Path, json: bool) -> Result<()> {
    let (choices, warnings) = crate::models::list(home).await;
    if json {
        let list: Vec<Value> = choices
            .iter()
            .map(|c| {
                json!({"provider": c.provider, "model": c.model, "context_window": c.context_window,
                    "efforts": c.efforts, "price": c.price})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json!({"models": list, "warnings": warnings}))?);
        return Ok(());
    }
    let mut groups = std::collections::BTreeMap::<String, Vec<String>>::new();
    for c in choices {
        groups.entry(c.provider).or_default().push(c.model);
    }
    let mut lines = Vec::new();
    for (provider, mut models) in groups {
        models.sort();
        models.dedup();
        lines.push(format!("{provider}:"));
        lines.extend(models.into_iter().map(|m| format!("  {m}")));
    }
    if lines.is_empty() {
        lines.push("No models found. Configure a provider or run nyq login.".into());
    }
    ui::list(&lines);
    for w in warnings {
        ui::error(&w);
    }
    Ok(())
}
pub fn cli_status(
    home: &Path,
    cwd: &Path,
    provider: Option<&dyn Provider>,
    policy: &sandbox::Policy,
    as_json: bool,
) -> Result<()> {
    let (servers, errors) = mcp::load_configs(home, cwd);
    let mut value = status(
        home,
        cwd,
        provider,
        policy,
        provider.map_or(0, |p| config::context_window_for(p.name(), None)),
        (&servers, &[]),
        skills::discover(home, cwd).len(),
    );
    value["mcp_config_valid"] = json!(errors.is_empty());
    value["model_config_valid"] = json!(provider.is_some());
    show_status(&value, as_json)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn check_formatting() {
        assert_eq!(
            check("git", true, "available", "install git").line(),
            "✓ git: available"
        );
        assert_eq!(
            check("auth", false, "expired", "run nyq login").line(),
            "✗ auth: expired — run nyq login"
        );
        assert_eq!(check("test", false, "failed", "").line(), "✗ test: failed");
    }
    #[test]
    fn session_check_reports_unreadable_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let c = sessions_check(&dir);
        assert!(c.ok && c.detail == "0 saved, all readable", "{c:?}");
        let mut s = crate::session::Session::create(&dir, tmp.path(), "m").unwrap();
        s.append(&crate::types::Item::user("hi")).unwrap();
        s.flush().unwrap();
        std::fs::write(dir.join("deadbeef-broken.jsonl"), "not json\n").unwrap();
        let c = sessions_check(&dir);
        assert!(!c.ok && !c.required, "{c:?}");
        assert!(c.detail.starts_with("2 saved, 1 need attention: deadbeef-broken:"), "{c:?}");
        assert!(c.line().contains("nyq session recover"));
    }
    #[tokio::test]
    async fn endpoint_probe_accepts_http_errors_as_reachable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1024];
            assert!(socket.read(&mut buf).await.unwrap() > 0);
            socket
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        assert!(reachable(&format!("http://{addr}/models")).await);
        server.await.unwrap();
    }
}
