//! Skill management: `/skills install|remove|info|create|path` and the
//! `install_skill` tool. Skills are installed by copying a directory that
//! holds a `SKILL.md` (from a local path or a shallow `git clone`) into the
//! profile's `skills/` or the project's `.nyq/skills/`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::skills::{self, Skill};
use crate::tools::{slash_path, Tool, ToolCtx, ToolOutput};
use crate::types::ToolSpec;

const CLONE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_NAME: usize = 64;
/// How deep a source is searched for SKILL.md files.
const MAX_DEPTH: usize = 4;
pub const USAGE: &str = "Usage: /skills (menu) | /skills [list|install SOURCE [--project] [--skill NAME]|remove NAME|info NAME|enable NAME|disable NAME|copy NAME [--project] [--force]|create NAME [--project]|path]";

/// Where managed skills live: the profile's or the project's skills directory.
pub fn managed_dir(home: &Path, cwd: &Path, project: bool) -> PathBuf {
    if project {
        cwd.join(".nyq").join("skills")
    } else {
        home.join("skills")
    }
}

/// A skill directory name nyq can create: one path component, no leading dot.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A parsed install source: a local directory or a git URL, plus an optional
/// skill-name filter (`owner/repo@skill`, `--skill NAME`).
#[derive(Debug, PartialEq)]
struct Source {
    location: String,
    filter: Option<String>,
}

/// A skills.sh page (`[https://][www.]skills.sh/owner/repo[/skill][/]`) as
/// `owner/repo[@skill]`; None for anything else.
fn from_skills_sh(raw: &str) -> Option<Result<String>> {
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let path = rest.strip_prefix("skills.sh/")?;
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    Some(match parts[..] {
        [owner, repo] => Ok(format!("{owner}/{repo}")),
        [owner, repo, skill] => Ok(format!("{owner}/{repo}@{skill}")),
        _ => Err(anyhow!(
            "unsupported skills.sh URL `{raw}` (expected skills.sh/OWNER/REPO[/SKILL])"
        )),
    })
}

fn parse_source(raw: &str, filter: Option<&str>) -> Result<Source> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("missing skill source (a git URL, owner/repo, or a local path)");
    }
    let mapped = from_skills_sh(raw).transpose()?;
    let raw = mapped.as_deref().unwrap_or(raw);
    let mut location = raw.to_string();
    let mut inline = None;
    // owner/repo@skill (not for URLs or paths).
    let pathish = raw.starts_with(['.', '/', '~'])
        || raw.contains("://")
        || raw.starts_with("git@")
        || Path::new(raw).is_absolute();
    if !pathish {
        if let Some((repo, skill)) = raw.rsplit_once('@') {
            if repo.contains('/') && !skill.is_empty() {
                location = repo.to_string();
                inline = Some(skill.to_string());
            }
        }
    }
    let filter = match (inline, filter.map(str::trim).filter(|f| !f.is_empty())) {
        (Some(a), Some(b)) if a != b => bail!("conflicting skill names `{a}` and `{b}`"),
        (a, b) => a.or(b.map(String::from)),
    };
    Ok(Source { location, filter })
}

pub(crate) fn expand_tilde(p: &str) -> PathBuf {
    match p.strip_prefix("~/").zip(dirs::home_dir()) {
        Some((rest, home)) => home.join(rest),
        None if p == "~" => dirs::home_dir().unwrap_or_default(),
        None => PathBuf::from(p),
    }
}

/// The URL to clone for a non-local source (`owner/repo` means GitHub).
pub(crate) fn clone_url(location: &str) -> Option<String> {
    if location.contains("://") || location.starts_with("git@") {
        return Some(location.to_string());
    }
    // `./x`, `../x`, `/x` and `~/x` are local paths, even when missing.
    if location.starts_with(['.', '/', '~']) || Path::new(location).is_absolute() {
        return None;
    }
    let parts: Vec<&str> = location.split('/').collect();
    let ok = parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        });
    ok.then(|| format!("https://github.com/{location}.git"))
}

fn repo_name(url: &str) -> String {
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("skill");
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

/// One SKILL.md found in a source.
struct Found {
    /// Directory to copy.
    dir: PathBuf,
    /// Directory name to install under.
    dir_name: String,
    /// The skill's name (frontmatter `name`, else the directory name).
    name: String,
}

/// Check a SKILL.md and return the skill's name.
fn validate_skill_md(path: &Path, dir_name: &str) -> Result<String> {
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("reading {}", slash_path(path)))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", slash_path(path));
    }
    if meta.len() > crate::tools::MAX_ROOTED_TEXT {
        bail!(
            "{} is larger than {} bytes",
            slash_path(path),
            crate::tools::MAX_ROOTED_TEXT
        );
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("{} is not UTF-8 text", slash_path(path)))?;
    let (name, _) =
        skills::front_matter(&text).map_err(|e| anyhow!("{}: {e}", slash_path(path)))?;
    Ok(name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| dir_name.to_string()))
}

/// SKILL.md files in `root` (itself, then subdirectories, skipping hidden
/// ones), matching `filter` by skill or directory name.
fn find_skills(root: &Path, fallback: &str, filter: Option<&str>) -> (Vec<Found>, Vec<String>) {
    let mut found = Vec::new();
    let mut problems = Vec::new();
    let matches = |f: &Found| filter.is_none_or(|w| w == f.name || w == f.dir_name);
    let mut consider = |dir: &Path, dir_name: &str, found: &mut Vec<Found>| {
        let md = dir.join("SKILL.md");
        if std::fs::symlink_metadata(&md).is_err() {
            return;
        }
        if !valid_name(dir_name) {
            problems.push(format!(
                "skipped {}: `{dir_name}` is not a usable skill directory name",
                slash_path(&md)
            ));
            return;
        }
        match validate_skill_md(&md, dir_name) {
            Ok(name) => {
                let f = Found {
                    dir: dir.to_path_buf(),
                    dir_name: dir_name.to_string(),
                    name,
                };
                if matches(&f) && !found.iter().any(|g: &Found| g.dir_name == f.dir_name) {
                    found.push(f);
                }
            }
            Err(e) => problems.push(format!("skipped: {e:#}")),
        }
    };
    consider(root, fallback, &mut found);
    if found.is_empty() {
        let mut stack = vec![(root.to_path_buf(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut subdirs: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter(|e| {
                    !e.file_name().to_string_lossy().starts_with('.')
                        && e.file_name() != "node_modules"
                })
                .map(|e| e.path())
                .collect();
            subdirs.sort();
            for sub in subdirs.into_iter().rev() {
                let name = sub
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let before = found.len();
                consider(&sub, &name, &mut found);
                // A skill's own subdirectories are its assets, not more skills.
                if found.len() == before && !sub.join("SKILL.md").exists() && depth < MAX_DEPTH {
                    stack.push((sub, depth + 1));
                }
            }
        }
    }
    found.sort_by(|a, b| a.dir_name.cmp(&b.dir_name));
    (found, problems)
}

/// Copy a directory tree, skipping `.git` and symlinks.
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", slash_path(to)))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", slash_path(from)))? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".git") {
            continue;
        }
        let ty = entry.file_type()?;
        let dest = to.join(&name);
        if ty.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &dest)
                .with_context(|| format!("copying {}", slash_path(&entry.path())))?;
        }
    }
    Ok(())
}

/// Removes a staging directory when dropped.
pub(crate) struct Staging(pub(crate) PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Install each skill in `dir` into `dest`, replacing same-named installs.
fn install_from_dir(
    dir: &Path,
    fallback: &str,
    filter: Option<&str>,
    dest: &Path,
) -> Result<Vec<String>> {
    let (found, problems) = find_skills(dir, fallback, filter);
    if found.is_empty() {
        let mut msg = match filter {
            Some(f) => format!("skill `{f}` not found in the source"),
            None => "no skills found (no valid SKILL.md files)".to_string(),
        };
        for p in problems.iter().take(5) {
            msg.push_str(&format!("\n  {p}"));
        }
        bail!(msg);
    }
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", slash_path(dest)))?;
    let mut names = Vec::new();
    for f in found {
        let stage = Staging(dest.join(format!(".install-{}", uuid::Uuid::new_v4().simple())));
        copy_tree(&f.dir, &stage.0)?;
        let target = dest.join(&f.dir_name);
        if std::fs::symlink_metadata(&target).is_ok() {
            remove_path(&target).with_context(|| format!("replacing {}", slash_path(&target)))?;
        }
        std::fs::rename(&stage.0, &target)
            .with_context(|| format!("installing {}", slash_path(&target)))?;
        names.push(f.name);
    }
    Ok(names)
}

pub(crate) fn remove_path(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

pub(crate) async fn git_clone(url: &str, into: &Path) -> Result<()> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["clone", "--depth", "1", "--quiet", "--", url])
        .arg(into)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(CLONE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!("git clone timed out after {}s", CLONE_TIMEOUT.as_secs()))?
        .context("running git (is it installed?)")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        bail!(
            "git clone {url} failed{}",
            if err.is_empty() {
                String::new()
            } else {
                format!(": {err}")
            }
        );
    }
    Ok(())
}

/// Install skills from `source` (a local directory, git URL or `owner/repo`)
/// into `dest`. Returns the installed skill names.
pub async fn install(
    source: &str,
    filter: Option<&str>,
    dest: &Path,
    cwd: &Path,
) -> Result<Vec<String>> {
    let src = parse_source(source, filter)?;
    let local = {
        let p = expand_tilde(&src.location);
        if p.is_absolute() {
            p
        } else {
            cwd.join(p)
        }
    };
    if local.is_dir() {
        let local = std::fs::canonicalize(&local).unwrap_or(local);
        let fallback = local
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        return install_from_dir(&local, &fallback, src.filter.as_deref(), dest);
    }
    let Some(url) = clone_url(&src.location) else {
        bail!("`{}` is neither a directory nor a git URL", src.location);
    };
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", slash_path(dest)))?;
    let stage = Staging(dest.join(format!(".clone-{}", uuid::Uuid::new_v4().simple())));
    git_clone(&url, &stage.0).await?;
    install_from_dir(&stage.0, &repo_name(&url), src.filter.as_deref(), dest)
}

/// Whether `skill` lives directly in one of nyq's managed directories.
fn managed(skill: &Skill, home: &Path, cwd: &Path) -> bool {
    let parent = skill
        .dir
        .parent()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()));
    [false, true].iter().any(|project| {
        let d = managed_dir(home, cwd, *project);
        let d = std::fs::canonicalize(&d).unwrap_or(d);
        parent.as_deref() == Some(d.as_path())
    })
}

/// Remove an installed skill by name (only from a managed directory).
pub fn remove(name: &str, skills: &[Skill], home: &Path, cwd: &Path) -> Result<String> {
    let Some(skill) = skills.iter().find(|s| s.id == name).or_else(|| {
        skills
            .iter()
            .find(|s| s.name == name || s.dir.file_name().is_some_and(|d| d == name))
    }) else {
        bail!("Skill '{name}' not found.");
    };
    if !managed(skill, home, cwd) {
        bail!(
            "Skill '{name}' is not in nyq's skills directories ({} or {}); remove it from {}.",
            slash_path(&managed_dir(home, cwd, false)),
            slash_path(&managed_dir(home, cwd, true)),
            slash_path(&skill.dir)
        );
    }
    remove_path(&skill.dir).with_context(|| format!("removing {}", slash_path(&skill.dir)))?;
    Ok(format!(
        "Removed skill '{}' ({}).",
        skill.name,
        slash_path(&skill.dir)
    ))
}

/// Lines describing one skill for `/skills info`.
pub fn info(name: &str, skills: &[Skill], home: &Path, cwd: &Path) -> Result<Vec<String>> {
    let Some(skill) = skills
        .iter()
        .find(|s| s.id == name || s.name == name)
        .or_else(|| skills.iter().find(|s| s.name.eq_ignore_ascii_case(name)))
    else {
        bail!("Skill '{name}' not found.");
    };
    let mut lines = vec![
        format!("{} ({})", skill.id, skill.scope),
        format!("  location: {}", skill.location),
        format!("  path: {}", slash_path(&skill.path)),
        format!(
            "  managed: {}",
            if managed(skill, home, cwd) {
                "yes (/skills remove works)"
            } else {
                "no"
            }
        ),
    ];
    let desc = skill
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    lines.push(format!(
        "  description: {}",
        if desc.is_empty() { "(none)" } else { &desc }
    ));
    let (files, total) = skills::skill_files(&skill.dir, 20);
    if !files.is_empty() {
        lines.push(format!("  files ({total}):"));
        lines.extend(files.iter().map(|f| format!("    {f}")));
        if total > files.len() {
            lines.push(format!("    … and {} more", total - files.len()));
        }
    }
    Ok(lines)
}

/// Create `<dir>/<name>/SKILL.md` from a template.
pub fn create(name: &str, dest: &Path) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!("Invalid skill name `{name}`: use letters, digits, '-', '_' or '.'.");
    }
    let dir = dest.join(name);
    if dir.exists() {
        bail!("{} already exists.", slash_path(&dir));
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", slash_path(&dir)))?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, format!("---\nname: {name}\ndescription: Describe when this skill should be used\n---\n\n# {name}\n\nInstructions for this skill...\n"))
        .with_context(|| format!("writing {}", slash_path(&path)))?;
    Ok(path)
}

/// What `/skills ARGS` asks for.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// `/skills` alone: the interactive menu.
    Menu,
    List,
    Install {
        source: String,
        project: bool,
        filter: Option<String>,
    },
    Remove(String),
    Info(String),
    Create {
        name: String,
        project: bool,
    },
    /// Copy a skill from another tool's folder into nyq's (`--force` replaces).
    Copy {
        name: String,
        project: bool,
        force: bool,
    },
    /// Turn a skill on or off (`disabled_skills`).
    Enable(String),
    Disable(String),
    Path,
    Usage,
}

/// A pasted `npx|bunx|pnpx skills add owner/repo [--skill x] [-g] [-y]`
/// line (optionally after `install`), as `/skills install` arguments: `-g`
/// installs for the user, otherwise into the project, like that tool does.
fn from_pasted<'a>(args: &[&'a str]) -> Option<Vec<&'a str>> {
    let args = match args.first() {
        Some(&"install" | &"add") => &args[1..],
        _ => args,
    };
    let [runner, tool, "add", rest @ ..] = args else {
        return None;
    };
    let tool = tool.split('@').next().unwrap_or(tool);
    if !matches!(*runner, "npx" | "bunx" | "pnpx") || tool != "skills" {
        return None;
    }
    let mut out = vec!["install"];
    let mut global = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "-g" | "--global" => global = true,
            "-y" | "--yes" => {}
            "-s" | "--skill" => {
                out.push("--skill");
                out.extend(rest.get(i + 1));
                i += 1;
            }
            // Which agents to install for: nyq reads every skills directory.
            "-a" | "--agent" => i += 1,
            a => out.push(a),
        }
        i += 1;
    }
    if !global {
        out.push("--project");
    }
    Some(out)
}

pub fn parse(args: &[&str]) -> Command {
    let pasted = from_pasted(args);
    let args = pasted.as_deref().unwrap_or(args);
    let project = args.contains(&"--project");
    let force = args.contains(&"--force");
    let mut filter = None;
    let mut rest = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i] {
            "--project" | "--force" => {}
            "--skill" => {
                filter = args.get(i + 1).map(|s| s.to_string());
                i += 1;
            }
            a if a.starts_with("--skill=") => filter = Some(a["--skill=".len()..].to_string()),
            a => rest.push(a),
        }
        i += 1;
    }
    let one = || (rest.len() == 1).then(|| rest[0].to_string());
    match (args.first().copied(), one()) {
        (None, _) if rest.is_empty() => Command::Menu,
        (Some("list"), _) if rest.is_empty() => Command::List,
        (Some("install" | "add"), Some(source)) => Command::Install {
            source,
            project,
            filter,
        },
        (Some("remove" | "rm" | "uninstall"), Some(name)) => Command::Remove(name),
        (Some("info" | "show"), Some(name)) => Command::Info(name),
        (Some("create" | "new"), Some(name)) => Command::Create { name, project },
        (Some("copy" | "cp"), Some(name)) => Command::Copy {
            name,
            project,
            force,
        },
        (Some("enable" | "on"), Some(name)) => Command::Enable(name),
        (Some("disable" | "off"), Some(name)) => Command::Disable(name),
        (Some("path"), _) if rest.is_empty() => Command::Path,
        _ => Command::Usage,
    }
}

/// Rediscover skills and refresh the system prompt and the `skill` tool.
fn reload(agent: &mut crate::agent::Agent, skills: &mut Vec<Skill>, home: &Path) {
    *skills = skills::discover(home, &agent.cwd);
    crate::commands::sync_prompt(agent, skills);
    skills::register(&mut agent.tools, skills.clone(), home, &agent.cwd);
}

/// Find a skill the way `/skills info` does: by id, then name, then name
/// in any case.
fn find<'a>(name: &str, skills: &'a [Skill]) -> Result<&'a Skill> {
    skills
        .iter()
        .find(|s| s.id == name || s.name == name)
        .or_else(|| skills.iter().find(|s| s.name.eq_ignore_ascii_case(name)))
        .ok_or_else(|| anyhow!("Skill '{name}' not found."))
}

/// Copy `skill` (found in another tool's folder) into `dest`, one of nyq's
/// skills directories, under its directory name. Something already there
/// is only replaced with `force`. Returns the new directory.
pub fn copy(skill: &Skill, dest: &Path, force: bool) -> Result<PathBuf> {
    let dir_name = skill
        .dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let target = dest.join(&dir_name);
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(&skill.dir) == canon(&target) {
        bail!("{} is already in {}.", skill.id, slash_path(dest));
    }
    if std::fs::symlink_metadata(&target).is_ok() && !force {
        bail!(
            "{} already exists; /skills copy {} --force replaces it.",
            slash_path(&target),
            skill.id
        );
    }
    install_from_dir(&skill.dir, &dir_name, None, dest)?;
    Ok(target)
}

/// The `/skills` menu rows for every skill found (disabled ones too).
pub fn menu_entries(all: &[Skill], home: &Path, cwd: &Path) -> Vec<crate::ui::skills_menu::Entry> {
    all.iter()
        .map(|skill| crate::ui::skills_menu::Entry {
            group: skills::source(skill),
            managed: managed(skill, home, cwd),
            disabled: skills::is_disabled(skill),
            files: skills::skill_files(&skill.dir, 0).1,
            info: info(&skill.id, all, home, cwd).unwrap_or_default(),
            skill: skill.clone(),
        })
        .collect()
}

/// Turn the skill `name` off (`off`) or back on through `disabled_skills`,
/// and offer the model what is left. Returns what to tell the user.
fn turn(
    agent: &mut crate::agent::Agent,
    skills: &mut Vec<Skill>,
    all: &[Skill],
    name: &str,
    off: bool,
) -> Result<String> {
    let home = crate::config::home();
    let skill = find(name, all)?;
    let (_, notice) =
        crate::settings::set_listed(&home, crate::settings::List::Skills, &skill.id, off)?;
    reload(agent, skills, &home);
    Ok(match (off, notice) {
        (_, Some(n)) => n,
        (true, None) => format!(
            "Turned off {}: the model and $ / completion no longer offer it.",
            skill.id
        ),
        (false, None) => format!("Turned on {}.", skill.id),
    })
}

/// A result line: in the open `/skills` menu, else in the scrollback.
fn say(tui: &crate::ui::tui::Tui, msg: &str) {
    if !tui.screen().menu_notice(msg, false) {
        crate::ui::info(msg);
    }
}

/// `/skills ARGS` in the interactive session. With the menu open (it sent
/// the command), the menu shows the result and refreshes.
pub async fn command(
    agent: &mut crate::agent::Agent,
    tui: &crate::ui::tui::Tui,
    skills: &mut Vec<Skill>,
    args: &[&str],
) -> Result<()> {
    let result = run(agent, tui, skills, args).await;
    if !tui.screen().skills_open() {
        return result;
    }
    let home = crate::config::home();
    let all = skills::discover_report(&home, &agent.cwd).0;
    tui.screen()
        .refresh_skills(menu_entries(&all, &home, &agent.cwd));
    if let Err(e) = result {
        tui.screen().menu_notice(&format!("{e:#}"), true);
    }
    Ok(())
}

async fn run(
    agent: &mut crate::agent::Agent,
    tui: &crate::ui::tui::Tui,
    skills: &mut Vec<Skill>,
    args: &[&str],
) -> Result<()> {
    use crate::ui;
    let home = crate::config::home();
    let cwd = agent.cwd.clone();
    // Every skill found, including turned-off ones (`skills` has only the rest).
    let all = || skills::discover_report(&home, &cwd).0;
    match parse(args) {
        Command::Menu if ui::menus_available() => {
            let targets = ui::skills_menu::Targets {
                user: managed_dir(&home, &cwd, false),
                project: managed_dir(&home, &cwd, true),
            };
            tui.screen()
                .open_skills(menu_entries(&all(), &home, &cwd), targets);
        }
        Command::Menu | Command::List => {
            ui::list(&ui::skills_menu::plain_lines(&menu_entries(
                &all(),
                &home,
                &cwd,
            )));
            let (_, problems) = skills::discover_report(&home, &cwd);
            if !problems.is_empty() {
                ui::error(&format!("{} SKILL.md problem(s):", problems.len()));
                ui::list(
                    &problems
                        .iter()
                        .map(|p| format!("  {p}"))
                        .collect::<Vec<_>>(),
                );
            }
        }
        Command::Usage => ui::info(USAGE),
        Command::Path => ui::list(&[
            format!(
                "user skills: {}",
                slash_path(&managed_dir(&home, &cwd, false))
            ),
            format!(
                "project skills: {}",
                slash_path(&managed_dir(&home, &cwd, true))
            ),
            format!(
                "also read: {} (project) and ~/{} (user)",
                skills::PROJECT_ROOTS
                    .iter()
                    .filter(|r| **r != ".nyq/skills")
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", "),
                skills::USER_ROOTS.join(", ~/")
            ),
        ]),
        Command::Info(name) => ui::list(&info(&name, &all(), &home, &cwd)?),
        Command::Remove(name) => {
            let msg = remove(&name, &all(), &home, &cwd)?;
            reload(agent, skills, &home);
            say(tui, &msg);
        }
        Command::Create { name, project } => {
            let path = create(&name, &managed_dir(&home, &cwd, project))?;
            reload(agent, skills, &home);
            say(
                tui,
                &format!(
                    "Created {}; edit it to describe and instruct the skill.",
                    slash_path(&path)
                ),
            );
        }
        Command::Copy {
            name,
            project,
            force,
        } => {
            let all = all();
            let skill = find(&name, &all)?;
            let dest = managed_dir(&home, &cwd, project);
            let target = copy(skill, &dest, force)?;
            reload(agent, skills, &home);
            say(
                tui,
                &format!("Copied {} to {}.", skill.id, slash_path(&target)),
            );
        }
        Command::Enable(name) => say(tui, &turn(agent, skills, &all(), &name, false)?),
        Command::Disable(name) => say(tui, &turn(agent, skills, &all(), &name, true)?),
        Command::Install {
            source,
            project,
            filter,
        } => {
            let dest = managed_dir(&home, &cwd, project);
            let cancel = tokio_util::sync::CancellationToken::new();
            tui.screen()
                .set_busy(Some("Installing skills"), Some(cancel.clone()));
            let result = tokio::select! {
                r = install(&source, filter.as_deref(), &dest, &cwd) => Some(r),
                _ = cancel.cancelled() => None,
            };
            tui.screen().idle();
            let Some(result) = result else {
                say(tui, "Install cancelled.");
                return Ok(());
            };
            let names = result?;
            reload(agent, skills, &home);
            say(
                tui,
                &format!("Installed {} into {}.", names.join(", "), slash_path(&dest)),
            );
        }
    }
    Ok(())
}

/// The `install_skill` tool: always asks before installing.
pub struct InstallSkill {
    pub home: PathBuf,
}

#[async_trait::async_trait]
impl Tool for InstallSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "install_skill".into(),
            description: "Install an Agent Skill (a directory with a SKILL.md) from a git URL, a GitHub `owner/repo` \
                (optionally `owner/repo@skill`), a skills.sh URL (skills.sh/owner/repo[/skill]), or a local directory. Every skill found in the source is installed \
                unless `skill` names one. Installs for the user by default; scope \"project\" installs into the \
                project's .nyq/skills. The user must approve. Afterwards load it with the `skill` tool."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "source": {"type": "string", "description": "git URL, owner/repo[@skill], skills.sh URL, or a local directory"},
                    "skill": {"type": "string", "description": "Only install the skill with this name"},
                    "scope": {"type": "string", "enum": ["user", "project"], "description": "user (default) or project"}
                },
                "required": ["source"]
            }),
        }
    }

    fn needs_approval(&self, _: &Value) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        let source = args["source"].as_str().unwrap_or_default();
        match args["skill"].as_str().filter(|s| !s.is_empty()) {
            Some(skill) => format!("{source} ({skill})"),
            None => source.to_string(),
        }
    }

    fn preview(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        let project = args["scope"].as_str() == Some("project");
        Some(format!(
            "install skills from {}\ninto {}",
            args["source"].as_str().unwrap_or_default(),
            slash_path(&managed_dir(&self.home, &ctx.cwd, project))
        ))
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(source) = args["source"].as_str().filter(|s| !s.trim().is_empty()) else {
            return ToolOutput::err("install_skill: \"source\" is required");
        };
        let project = match args["scope"].as_str() {
            None | Some("user") => false,
            Some("project") => true,
            Some(other) => {
                return ToolOutput::err(format!(
                    "install_skill: unknown scope `{other}` (user or project)"
                ))
            }
        };
        let dest = managed_dir(&self.home, &ctx.cwd, project);
        let job = install(source, args["skill"].as_str(), &dest, &ctx.cwd);
        let result = tokio::select! {
            r = job => r,
            _ = ctx.cancel.cancelled() => return ToolOutput::err("install_skill cancelled."),
        };
        match result {
            Ok(names) => {
                let mut out = format!(
                    "Installed {} skill(s) into {}:\n",
                    names.len(),
                    slash_path(&dest)
                );
                for n in &names {
                    out.push_str(&format!(
                        "- {}\n",
                        n.split_whitespace().collect::<Vec<_>>().join(" ")
                    ));
                }
                out.push_str("Load one with the `skill` tool.");
                ToolOutput {
                    changed: true,
                    ..ToolOutput::ok(out)
                }
            }
            Err(e) => ToolOutput::err(format!("install_skill failed: {e:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn md(name: &str) -> String {
        format!("---\nname: {name}\ndescription: {name} helper\n---\n\nUse {name}.\n")
    }

    #[test]
    fn parses_sources_and_commands() {
        assert_eq!(
            parse_source("owner/repo@pdf", None).unwrap(),
            Source {
                location: "owner/repo".into(),
                filter: Some("pdf".into())
            }
        );
        assert_eq!(
            parse_source("git@github.com:o/r.git", None).unwrap().filter,
            None
        );
        assert_eq!(parse_source("./dir@x", None).unwrap().location, "./dir@x");
        // skills.sh pages map to owner/repo[@skill].
        for url in [
            "https://skills.sh/o/r/pdf",
            "skills.sh/o/r/pdf/",
            "http://www.skills.sh/o/r/pdf?x=1",
        ] {
            assert_eq!(
                parse_source(url, None).unwrap(),
                Source {
                    location: "o/r".into(),
                    filter: Some("pdf".into())
                },
                "{url}"
            );
        }
        assert_eq!(
            parse_source("https://www.skills.sh/o/r/", None).unwrap(),
            Source {
                location: "o/r".into(),
                filter: None
            }
        );
        assert!(parse_source("skills.sh/o/r/pdf", Some("other")).is_err());
        assert!(parse_source("https://skills.sh/o", None).is_err());
        assert!(parse_source("skills.sh/a/b/c/d", None).is_err());
        assert!(parse_source("o/r@a", Some("b")).is_err());
        assert!(parse_source("  ", None).is_err());
        assert_eq!(
            clone_url("o/r").as_deref(),
            Some("https://github.com/o/r.git")
        );
        assert_eq!(clone_url("not a repo"), None);
        // Local-looking paths never become GitHub URLs, even when missing.
        for local in ["./nope", "../x/y", "~/skills", "/tmp/x"] {
            assert_eq!(clone_url(local), None, "{local}");
        }
        assert_eq!(
            repo_name("https://github.com/o/agent-skills.git"),
            "agent-skills"
        );
        assert_eq!(repo_name("git@github.com:o/r.git"), "r");

        assert_eq!(parse(&[]), Command::Menu);
        assert_eq!(parse(&["list"]), Command::List);
        assert_eq!(
            parse(&["install", "o/r", "--project", "--skill", "pdf"]),
            Command::Install {
                source: "o/r".into(),
                project: true,
                filter: Some("pdf".into())
            }
        );
        assert_eq!(
            parse(&["add", "./x"]),
            Command::Install {
                source: "./x".into(),
                project: false,
                filter: None
            }
        );
        assert_eq!(parse(&["remove", "pdf"]), Command::Remove("pdf".into()));
        assert_eq!(parse(&["info", "pdf"]), Command::Info("pdf".into()));
        assert_eq!(
            parse(&["create", "x", "--project"]),
            Command::Create {
                name: "x".into(),
                project: true
            }
        );
        assert_eq!(parse(&["install"]), Command::Usage);
        // A pasted install line from a skills page.
        let line = |s: &str| parse(&s.split_whitespace().collect::<Vec<_>>());
        assert_eq!(
            line("install npx skills add owner/repo --skill pdf -g -y"),
            Command::Install {
                source: "owner/repo".into(),
                project: false,
                filter: Some("pdf".into())
            }
        );
        assert_eq!(
            line("bunx skills@latest add owner/repo -a claude-code"),
            Command::Install {
                source: "owner/repo".into(),
                project: true,
                filter: None
            }
        );
        assert_eq!(
            line("add npx skills add o/r -s fmt --yes"),
            Command::Install {
                source: "o/r".into(),
                project: true,
                filter: Some("fmt".into())
            }
        );
        assert_eq!(line("install npx other add o/r"), Command::Usage);
        assert_eq!(parse(&["bogus"]), Command::Usage);
    }

    #[tokio::test]
    async fn installs_local_sources_and_removes_them() {
        let t = tempfile::tempdir().unwrap();
        let (home, cwd) = (t.path().join("home"), t.path().join("proj"));
        fs::create_dir_all(&cwd).unwrap();
        // A pack with two skills, a hidden dir, a nested asset and a .git dir.
        let pack = t.path().join("pack");
        write(&pack.join("skills/pdf/SKILL.md"), &md("pdf"));
        write(&pack.join("skills/pdf/scripts/fill.py"), "print()");
        write(&pack.join("skills/pdf/.git/config"), "secret");
        write(&pack.join("skills/review/SKILL.md"), &md("review"));
        write(&pack.join(".hidden/x/SKILL.md"), &md("hidden"));
        write(&pack.join("broken/SKILL.md"), "---\nname: broken\n");

        let user = managed_dir(&home, &cwd, false);
        let names = install(pack.to_str().unwrap(), Some("pdf"), &user, &cwd)
            .await
            .unwrap();
        assert_eq!(names, ["pdf"]);
        assert!(user.join("pdf/scripts/fill.py").exists());
        assert!(!user.join("pdf/.git").exists());
        assert!(!user.join("review").exists());

        let project = managed_dir(&home, &cwd, true);
        let names = install("../pack", None, &project, &cwd).await.unwrap();
        assert_eq!(names, ["pdf", "review"]);
        assert!(project.join("review/SKILL.md").exists());
        // Only staging-free entries remain.
        let left: Vec<String> = fs::read_dir(&project)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(left.iter().all(|n| !n.starts_with('.')), "{left:?}");

        // A skill directory itself as the source; reinstall replaces it.
        let single = t.path().join("solo");
        write(&single.join("SKILL.md"), &md("solo-skill"));
        assert_eq!(
            install(single.to_str().unwrap(), None, &user, &cwd)
                .await
                .unwrap(),
            ["solo-skill"]
        );
        write(
            &single.join("SKILL.md"),
            "---\nname: solo-skill\ndescription: v2\n---\nbody\n",
        );
        install(single.to_str().unwrap(), None, &user, &cwd)
            .await
            .unwrap();
        assert!(fs::read_to_string(user.join("solo/SKILL.md"))
            .unwrap()
            .contains("v2"));

        let err = install(pack.to_str().unwrap(), Some("nope"), &user, &cwd)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("skill `nope` not found"), "{err}");
        let empty = t.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let err = install(empty.to_str().unwrap(), None, &user, &cwd)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no skills found"), "{err}");
        assert!(install("not a source", None, &user, &cwd).await.is_err());

        // info / remove go through discovery; foreign skills are refused.
        write(&cwd.join(".claude/skills/foreign/SKILL.md"), &md("foreign"));
        let found = skills::discover_with(&home, None, &cwd);
        let lines = info("pdf", &found, &home, &cwd).unwrap();
        assert!(lines[0].starts_with("pdf (project)"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("scripts/fill.py")),
            "{lines:?}"
        );
        let err = remove("foreign", &found, &home, &cwd).unwrap_err();
        assert!(
            err.to_string().contains("not in nyq's skills directories"),
            "{err}"
        );
        assert!(cwd.join(".claude/skills/foreign").exists());
        remove("review", &found, &home, &cwd).unwrap();
        assert!(!project.join("review").exists());
        assert!(remove(
            "review",
            &skills::discover_with(&home, None, &cwd),
            &home,
            &cwd
        )
        .is_err());
        let solo = skills::discover_with(&home, None, &cwd);
        remove("solo-skill", &solo, &home, &cwd).unwrap();
        assert!(!user.join("solo").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn installs_from_a_git_repository() {
        let has_git = std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !has_git {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("my-skills");
        write(&repo.join("deploy/SKILL.md"), &md("deploy"));
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        let url = format!("file://{}", slash_path(&repo));
        let dest = t.path().join("home/skills");
        let names = install(&url, None, &dest, t.path()).await.unwrap();
        assert_eq!(names, ["deploy"]);
        assert!(dest.join("deploy/SKILL.md").exists());
        let left: Vec<_> = fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "the clone is cleaned up: {left:?}");
    }

    #[tokio::test]
    async fn tool_needs_approval_and_installs() {
        let t = tempfile::tempdir().unwrap();
        write(&t.path().join("src/fmt/SKILL.md"), &md("fmt"));
        let tool = InstallSkill {
            home: t.path().join("home"),
        };
        assert!(tool.needs_approval(&json!({"source": "x"})));
        let ctx = ToolCtx {
            cwd: t.path().join("proj"),
            ..Default::default()
        };
        fs::create_dir_all(&ctx.cwd).unwrap();
        let out = tool
            .run(
                json!({"source": t.path().join("src").to_str().unwrap(), "scope": "project"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("- fmt"), "{}", out.content);
        assert!(ctx.cwd.join(".nyq/skills/fmt/SKILL.md").exists());
        assert!(
            tool.run(json!({"source": "x", "scope": "galaxy"}), &ctx)
                .await
                .is_error
        );
        assert!(tool.run(json!({}), &ctx).await.is_error);
    }

    #[test]
    fn create_writes_a_template() {
        let t = tempfile::tempdir().unwrap();
        let path = create("my-skill", t.path()).unwrap();
        assert!(fs::read_to_string(path)
            .unwrap()
            .starts_with("---\nname: my-skill\n"));
        assert!(create("my-skill", t.path()).is_err());
        assert!(create("../evil", t.path()).is_err());
        assert!(create(".hidden", t.path()).is_err());
    }

    #[test]
    fn parses_copy_and_toggles() {
        assert_eq!(
            parse(&["copy", "pdf"]),
            Command::Copy {
                name: "pdf".into(),
                project: false,
                force: false
            }
        );
        assert_eq!(
            parse(&["cp", "pdf", "--project", "--force"]),
            Command::Copy {
                name: "pdf".into(),
                project: true,
                force: true
            }
        );
        assert_eq!(parse(&["disable", "pdf"]), Command::Disable("pdf".into()));
        assert_eq!(
            parse(&["enable", "review@.claude/skills"]),
            Command::Enable("review@.claude/skills".into())
        );
        assert_eq!(parse(&["enable"]), Command::Usage);
    }

    #[test]
    fn copies_another_tools_skill_into_nyq() {
        let t = tempfile::tempdir().unwrap();
        let (cwd, home) = (t.path().join("proj"), t.path().join("home"));
        let src = cwd.join(".claude/skills/pdf");
        write(&src.join("SKILL.md"), &md("pdf"));
        write(&src.join("scripts/fill.py"), "print()\n");
        write(&src.join(".git/HEAD"), "ref\n");
        let skills = skills::discover_with(&home, None, &cwd);
        let dest = managed_dir(&home, &cwd, false);
        let target = copy(&skills[0], &dest, false).unwrap();
        assert_eq!(target, dest.join("pdf"));
        assert!(target.join("scripts/fill.py").is_file() && !target.join(".git").exists());
        assert!(src.join("SKILL.md").is_file(), "the original stays");
        // Something already there is only replaced with force.
        write(&target.join("SKILL.md"), &md("pdf-old"));
        let err = copy(&skills[0], &dest, false).unwrap_err().to_string();
        assert!(
            err.contains("already exists") && err.contains("--force"),
            "{err}"
        );
        assert!(fs::read_to_string(target.join("SKILL.md"))
            .unwrap()
            .contains("pdf-old"));
        copy(&skills[0], &dest, true).unwrap();
        assert!(!fs::read_to_string(target.join("SKILL.md"))
            .unwrap()
            .contains("pdf-old"));
        // A skill already in nyq's directory isn't copied onto itself.
        let again = skills::discover_with(&home, None, &cwd);
        let mine = again.iter().find(|s| s.location.contains("home")).unwrap();
        assert!(copy(mine, &dest, true)
            .unwrap_err()
            .to_string()
            .contains("already in"));
    }

    #[test]
    fn menu_entries_mark_managed_and_count_files() {
        let t = tempfile::tempdir().unwrap();
        let (cwd, home) = (t.path().join("proj"), t.path().join("home"));
        write(&cwd.join(".claude/skills/pdf/SKILL.md"), &md("pdf"));
        write(&cwd.join(".claude/skills/pdf/a.md"), "a");
        write(&cwd.join(".nyq/skills/lint/SKILL.md"), &md("lint"));
        let all = skills::discover_with(&home, None, &cwd);
        let entries = menu_entries(&all, &home, &cwd);
        let e = |id: &str| entries.iter().find(|e| e.skill.id == id).unwrap();
        assert!(e("lint").managed && !e("pdf").managed);
        assert_eq!((e("pdf").files, e("lint").files), (1, 0));
        assert_eq!(e("pdf").group, "Claude Code");
        assert!(!e("pdf").disabled);
        assert!(e("pdf").info[0].starts_with("pdf (project)"));
    }
}
