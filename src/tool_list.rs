//! `/tools` (with `--debug`): the menu of every tool, or as text where no
//! menu can open, plus `/tools list|info|disable|enable|trust|untrust NAME`.
//! Turning a tool off writes `disabled_tools` in the user's settings.toml;
//! trusting one goes through the `/allowlist` code.

use anyhow::{bail, Result};

use crate::agent::Agent;
use crate::ui::{self, tools_menu, tui::Tui};

pub const USAGE: &str = "Usage: /tools (menu) | /tools [list|info NAME|disable NAME|enable NAME|trust NAME|untrust NAME]";

/// What `/tools ARGS` asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `/tools` alone: the menu, or the list as text.
    Menu,
    List,
    Info(String),
    Disable(String),
    Enable(String),
    /// Always allow the tool (`/allowlist user add tool NAME`).
    Trust(String),
    /// Take that back (`/allowlist remove tool NAME`).
    Untrust(String),
    Usage,
}

pub fn parse(args: &[&str]) -> Command {
    let name = || args[1].to_string();
    match args {
        [] => Command::Menu,
        ["list"] => Command::List,
        ["info" | "show", _] => Command::Info(name()),
        ["disable" | "off", _] => Command::Disable(name()),
        ["enable" | "on", _] => Command::Enable(name()),
        ["trust" | "allow", _] => Command::Trust(name()),
        ["untrust" | "unallow", _] => Command::Untrust(name()),
        _ => Command::Usage,
    }
}

/// The menu rows for every tool the session has, disabled ones included.
pub fn entries(agent: &mut Agent) -> Vec<tools_menu::Entry> {
    // MCP servers and extensions that came up since the last request.
    agent.tools.refresh();
    tools_menu::entries(
        agent.tools.catalog(&agent.cwd),
        &agent.permissions,
        &agent.allowed,
        &agent.cwd,
        agent.permission_mode,
    )
}

/// Turn `name` off (`off`) or back on in the user's `disabled_tools`, and
/// apply it to this session's tools.
fn turn(agent: &mut Agent, name: &str, off: bool) -> Result<String> {
    let home = crate::config::home();
    let (s, notice) = crate::settings::set_listed(&home, crate::settings::List::Tools, name, off)?;
    agent.tools.set_disabled(&s.disabled_tools);
    Ok(match (off, notice) {
        (_, Some(n)) => n,
        (true, None) => format!(
            "Turned off {name}: the model no longer gets it (disabled_tools in settings.toml)."
        ),
        (false, None) => format!("Turned on {name}."),
    })
}

/// Run `/allowlist WORDS` (the same code, rules and files).
fn allowlist(agent: &mut Agent, words: &[&str]) -> Result<Vec<String>> {
    let perms = std::sync::Arc::make_mut(&mut agent.permissions);
    crate::grants::allowlist(perms, &mut agent.allowed, words).map_err(|e| anyhow::anyhow!(e))
}

/// Run `/tools ARGS` against the session; the lines to show. `Ok(None)`
/// means the menu should open.
pub fn run(agent: &mut Agent, args: &[&str], menu: bool) -> Result<Option<Vec<String>>> {
    let known = |agent: &Agent, name: &str| -> Result<()> {
        if agent
            .tools
            .catalog(&agent.cwd)
            .iter()
            .any(|(s, _, _)| s.name == name)
        {
            Ok(())
        } else {
            bail!("no tool named `{name}` (see /tools list)")
        }
    };
    let lines = match parse(args) {
        Command::Menu if menu => return Ok(None),
        Command::Menu | Command::List => tools_menu::plain_lines(&entries(agent)),
        Command::Info(name) => {
            let all = entries(agent);
            let Some(e) = all.iter().find(|e| e.spec.name == name) else {
                bail!("no tool named `{name}` (see /tools list)")
            };
            tools_menu::detail(e).lines().map(String::from).collect()
        }
        Command::Disable(name) => {
            known(agent, &name)?;
            vec![turn(agent, &name, true)?]
        }
        Command::Enable(name) => {
            known(agent, &name)?;
            vec![turn(agent, &name, false)?]
        }
        Command::Trust(name) => {
            known(agent, &name)?;
            allowlist(agent, &["user", "add", "tool", &name])?
        }
        Command::Untrust(name) => {
            known(agent, &name)?;
            allowlist(agent, &["remove", "tool", &name])?
        }
        Command::Usage => vec![USAGE.to_string()],
    };
    Ok(Some(lines))
}

/// `/tools ARGS` in the interactive session (only with `--debug`). With the
/// menu open (it sent the command), the menu shows the result and refreshes.
pub fn command(agent: &mut Agent, tui: &Tui, args: &[&str]) {
    if !crate::config::debug() {
        ui::info("  /tools is available with --debug (or NYQ_DEBUG=1).");
        return;
    }
    let result = run(agent, args, ui::menus_available());
    let menu = tui.screen().tools_open();
    match result {
        Ok(None) => tui.screen().open_tools(entries(agent)),
        Ok(Some(lines)) if menu => {
            tui.screen().refresh_tools(entries(agent));
            tui.screen().menu_notice(&lines.join(" "), false);
        }
        Ok(Some(lines)) if lines.len() == 1 => ui::info(&format!("  {}", lines[0])),
        Ok(Some(lines)) => ui::list(&lines),
        Err(e) if menu => {
            tui.screen().menu_notice(&format!("{e:#}"), true);
        }
        Err(e) => ui::error(&format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subcommands() {
        assert_eq!(parse(&[]), Command::Menu);
        assert_eq!(parse(&["list"]), Command::List);
        assert_eq!(parse(&["info", "bash"]), Command::Info("bash".into()));
        assert_eq!(parse(&["off", "bash"]), Command::Disable("bash".into()));
        assert_eq!(parse(&["enable", "bash"]), Command::Enable("bash".into()));
        assert_eq!(
            parse(&["trust", "mcp__gh__list"]),
            Command::Trust("mcp__gh__list".into())
        );
        assert_eq!(parse(&["untrust", "x"]), Command::Untrust("x".into()));
        assert_eq!(parse(&["disable"]), Command::Usage);
        assert_eq!(parse(&["bogus", "x"]), Command::Usage);
    }

    #[test]
    fn without_a_terminal_lists_text_and_subcommands_work() {
        // Tests have no TUI and no terminal: no menu can open.
        assert!(!ui::menus_available());
        let mut agent = crate::agent::test_agent();
        let perms = std::sync::Arc::make_mut(&mut agent.permissions);
        perms.deny = vec!["web_fetch".into()];
        let lines = run(&mut agent, &[], ui::menus_available())
            .unwrap()
            .expect("text, not a menu");
        assert_eq!(lines[0].split(' ').next(), Some("Built-in"));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("read_file") && l.contains("[auto]")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("web_fetch") && l.contains("[denied]")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("bash") && l.contains("[asks]")),
            "{lines:?}"
        );
        assert!(
            run(&mut agent, &[], true).unwrap().is_none(),
            "with a terminal the menu opens"
        );
        let info = run(&mut agent, &["info", "bash"], false).unwrap().unwrap();
        assert!(
            info[0].starts_with("bash · Built-in")
                && info
                    .iter()
                    .any(|l| l.contains("command (string, required)")),
            "{info:?}"
        );
        assert!(run(&mut agent, &["info", "nope"], false)
            .unwrap_err()
            .to_string()
            .contains("no tool named"));
        assert!(run(&mut agent, &["trust", "nope"], false).is_err());
        assert_eq!(run(&mut agent, &["what"], false).unwrap().unwrap(), [USAGE]);
    }
}
