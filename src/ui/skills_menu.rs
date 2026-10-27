//! `/skills` menu: every discovered skill, grouped by where it was found
//! (nyq, Agents, Claude Code, ...), with a preview of the selected one.
//! Typing filters, Tab and Shift+Tab pick a group, Enter runs the skill,
//! Ctrl+E puts `/skill-name ` in the editor for adding a request, → shows
//! the SKILL.md, Ctrl+T turns a skill off or on, Ctrl+O copies one from
//! another tool's folder into nyq's, Ctrl+N installs from a source typed
//! in the menu, Delete removes one nyq manages (after a confirmation), Esc
//! closes.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::menu::{self, Badge, List, Row, View};
use super::theme::{GRAY, GREEN, RED, YELLOW};
use crate::skills::Skill;

pub struct Entry {
    pub skill: Skill,
    /// Which agent's skill root it came from (see [`crate::skills::source`]).
    pub group: &'static str,
    /// In a directory `/skills remove` may delete from.
    pub managed: bool,
    /// Named in the `disabled_skills` setting.
    pub disabled: bool,
    /// Files in the skill's directory besides SKILL.md.
    pub files: usize,
    /// The `/skills info` lines, shown with the SKILL.md when viewing it.
    pub info: Vec<String>,
}

/// nyq's own skills directories, where Ctrl+O copies skills to.
#[derive(Clone, Debug, Default)]
pub struct Targets {
    pub user: PathBuf,
    pub project: PathBuf,
}

/// What the menu is waiting for besides list keys.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    List,
    /// Remove this skill (by id)?
    Remove(String),
    /// Copy this skill where: user or project?
    CopyWhere(String),
    /// The copy target exists: replace it?
    Replace {
        id: String,
        project: bool,
    },
    /// Typing an install source.
    Install,
}

pub struct SkillsMenu {
    entries: Vec<Entry>,
    /// The groups that have skills, in [`crate::skills::SOURCES`] order.
    pub groups: Vec<&'static str>,
    pub list: List,
    mode: Mode,
    /// The install source being typed.
    pub input: String,
    /// A result or problem shown above the hints, and whether it is an error.
    pub notice: Option<(String, bool)>,
    /// A command still running (an install), shown until the menu refreshes.
    pub busy: Option<String>,
    targets: Targets,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
    /// Put this text in the editor (a skill invocation with room for a request).
    Insert(String),
    /// Send this text now (a skill on its own) and close the menu.
    Send(String),
    /// Show this skill (by id) full screen.
    View(String),
    /// Run this command and keep the menu open (it refreshes afterwards).
    Run(String),
}

impl SkillsMenu {
    pub fn new(entries: Vec<Entry>, targets: Targets) -> Self {
        let mut m = Self {
            entries: Vec::new(),
            groups: Vec::new(),
            list: List::default(),
            mode: Mode::List,
            input: String::new(),
            notice: None,
            busy: None,
            targets,
        };
        m.set_entries(entries);
        m
    }

    /// Replace the skills (after a change), keeping the filter, group and
    /// the selected skill (or, when it is gone, the same position).
    pub fn set_entries(&mut self, mut entries: Vec<Entry>) {
        let keep = self.current().map(|e| e.skill.path.clone());
        let group = self.list.group.and_then(|g| self.groups.get(g).copied());
        let order = |g: &str| {
            crate::skills::SOURCES
                .iter()
                .position(|s| *s == g)
                .unwrap_or(usize::MAX)
        };
        // Grouped by source, project before user within a group, then by name.
        entries.sort_by(|a, b| {
            (order(a.group), a.skill.scope != "project", &a.skill.id).cmp(&(
                order(b.group),
                b.skill.scope != "project",
                &b.skill.id,
            ))
        });
        self.groups = entries.iter().map(|e| e.group).collect();
        self.groups.dedup();
        self.entries = entries;
        self.list.group = group.and_then(|g| self.groups.iter().position(|x| *x == g));
        self.busy = None;
        if let Some(path) = keep {
            self.list.selected = self
                .rows()
                .iter()
                .position(|e| e.skill.path == path)
                .unwrap_or(self.list.selected);
        }
    }

    fn matching(&self) -> impl Iterator<Item = &Entry> {
        let words = self.list.words();
        self.entries.iter().filter(move |e| {
            let s = &e.skill;
            let state = if e.disabled { "disabled" } else { "" };
            menu::matches(
                &format!(
                    "{} {} {} {} {} {state}",
                    s.id, s.description, e.group, s.scope, s.location
                ),
                &words,
            )
        })
    }

    /// Entries in the chosen group whose name, description, group, scope or
    /// location contain every word of the filter.
    pub fn rows(&self) -> Vec<&Entry> {
        let group = self.list.group.and_then(|g| self.groups.get(g).copied());
        self.matching()
            .filter(|e| group.is_none_or(|g| g == e.group))
            .collect()
    }

    fn current(&self) -> Option<&Entry> {
        let rows = self.rows();
        rows.get(self.list.selected.min(rows.len().saturating_sub(1)))
            .copied()
    }

    /// The entry for `id`.
    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.skill.id == id)
    }

    /// Where Ctrl+O would copy `e`, and whether something is there already.
    fn copy_target(&self, e: &Entry, project: bool) -> (PathBuf, bool) {
        let root = if project {
            &self.targets.project
        } else {
            &self.targets.user
        };
        let dir = root.join(e.skill.dir.file_name().unwrap_or_default());
        let taken = std::fs::symlink_metadata(&dir).is_ok();
        (dir, taken)
    }

    pub fn on_key(&mut self, key: KeyEvent, is_command: impl Fn(&str) -> bool) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && key.code == KeyCode::Char('c') {
            return Action::Close;
        }
        self.notice = None;
        match std::mem::replace(&mut self.mode, Mode::List) {
            Mode::List => {}
            Mode::Remove(id) => {
                return match key.code {
                    KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                        Action::Run(format!("/skills remove {id}"))
                    }
                    _ => Action::None,
                };
            }
            Mode::CopyWhere(id) => {
                let project = match key.code {
                    KeyCode::Enter | KeyCode::Char('u' | 'U') => false,
                    KeyCode::Char('p' | 'P') => true,
                    _ => return Action::None,
                };
                let Some(e) = self.entry(&id) else {
                    return Action::None;
                };
                return match self.copy_target(e, project) {
                    (_, true) => {
                        self.mode = Mode::Replace { id, project };
                        Action::None
                    }
                    _ => Action::Run(copy_command(&id, project, false)),
                };
            }
            Mode::Replace { id, project } => {
                return match key.code {
                    KeyCode::Char('y' | 'Y') => Action::Run(copy_command(&id, project, true)),
                    _ => Action::None,
                };
            }
            Mode::Install => {
                match key.code {
                    KeyCode::Esc => self.input.clear(),
                    KeyCode::Enter if !self.input.trim().is_empty() => {
                        let source = std::mem::take(&mut self.input);
                        self.busy = Some(format!("Installing {}…", source.trim()));
                        return Action::Run(format!("/skills install {}", source.trim()));
                    }
                    KeyCode::Backspace => {
                        self.input.pop();
                        self.mode = Mode::Install;
                    }
                    KeyCode::Char('u') if ctrl => {
                        self.input.clear();
                        self.mode = Mode::Install;
                    }
                    KeyCode::Char(c) if !c.is_control() && !ctrl => {
                        self.input.push(c);
                        self.mode = Mode::Install;
                    }
                    _ => self.mode = Mode::Install,
                }
                return Action::None;
            }
        }
        let count = self.rows().len();
        let current = self.current();
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Enter => match current {
                Some(e) if e.disabled => {
                    self.notice = Some((
                        format!("{} is turned off; Ctrl+T turns it on.", e.skill.id),
                        true,
                    ));
                }
                Some(e) => return Action::Send(invocation(&e.skill.id, &is_command)),
                None => {}
            },
            KeyCode::Char('e') if ctrl => {
                if let Some(e) = current {
                    return Action::Insert(format!("{} ", invocation(&e.skill.id, &is_command)));
                }
            }
            KeyCode::Right => {
                if let Some(e) = current {
                    return Action::View(e.skill.id.clone());
                }
            }
            KeyCode::Char('t') if ctrl => {
                if let Some(e) = current {
                    let verb = if e.disabled { "enable" } else { "disable" };
                    return Action::Run(format!("/skills {verb} {}", e.skill.id));
                }
            }
            KeyCode::Char('o') if ctrl => match current {
                Some(e) if e.group == "nyq" => {
                    self.notice = Some((
                        format!("{} is already in nyq's skills directory.", e.skill.id),
                        false,
                    ));
                }
                Some(e) => self.mode = Mode::CopyWhere(e.skill.id.clone()),
                None => {}
            },
            KeyCode::Delete | KeyCode::Char('x') if key.code == KeyCode::Delete || ctrl => {
                match current {
                    Some(e) if e.managed => self.mode = Mode::Remove(e.skill.id.clone()),
                    Some(e) => {
                        let msg = format!(
                        "{} is in {} ({}); nyq only removes skills it installed. Delete it there, or Ctrl+T turns it off.",
                        e.skill.id, e.skill.location, e.group
                    );
                        self.notice = Some((msg, true));
                    }
                    None => {}
                }
            }
            KeyCode::Char('n') if ctrl => self.mode = Mode::Install,
            _ => {
                self.list.on_key(key, count, self.groups.len());
            }
        }
        Action::None
    }

    /// The menu drawn in `width` columns and about `height` rows.
    pub fn render(&self, width: usize, height: usize) -> Vec<String> {
        let rows = self.rows();
        let all: Vec<&Entry> = self.matching().collect();
        let tabs = self
            .groups
            .iter()
            .map(|g| (g.to_string(), all.iter().filter(|e| e.group == *g).count()))
            .collect();
        let current = self.current();
        let note = match &self.mode {
            Mode::Remove(id) => {
                let path = self.entry(id).map(|e| crate::tools::slash_path(&e.skill.dir)).unwrap_or_default();
                Some((format!("Remove skill {id} ({path})? Enter or y removes it, any other key keeps it."), YELLOW))
            }
            Mode::CopyWhere(id) => Some((
                format!(
                    "Copy {id} into nyq's skills: u {} (yours) or p .nyq/skills (this project)?",
                    super::statusline::tilde(&crate::tools::slash_path(&self.targets.user)),
                ),
                YELLOW,
            )),
            Mode::Replace { id, project } => {
                let path = self.entry(id).map(|e| self.copy_target(e, *project).0).unwrap_or_default();
                let path = super::statusline::tilde(&crate::tools::slash_path(&path));
                Some((format!("{path} already exists. y replaces it with {id}, any other key keeps it."), YELLOW))
            }
            Mode::Install => Some((
                "A git URL, owner/repo (owner/repo@skill picks one), a skills.sh page or a folder; add --project to install into this project.".into(),
                GRAY,
            )),
            Mode::List => match (&self.busy, &self.notice) {
                (Some(b), _) => Some((b.clone(), GRAY)),
                (None, Some((n, true))) => Some((n.clone(), RED)),
                (None, Some((n, false))) => Some((n.clone(), GREEN)),
                _ => None,
            },
        };
        let hints: Vec<(&str, &str)> = match &self.mode {
            Mode::Install => vec![("Enter", "Install"), ("^U", "Clear"), ("Esc", "Back")],
            Mode::Remove(_) => vec![("Enter/y", "Remove"), ("any other key", "Keep")],
            Mode::CopyWhere(_) => vec![
                ("u", "Your skills"),
                ("p", "This project"),
                ("any other key", "Cancel"),
            ],
            Mode::Replace { .. } => vec![("y", "Replace"), ("any other key", "Keep")],
            Mode::List => {
                let mut h = vec![
                    ("Enter", "Run"),
                    ("^E", "Add request"),
                    ("→", "View"),
                    ("Tab", "Source"),
                ];
                if let Some(e) = current {
                    h.push(("^T", if e.disabled { "Turn on" } else { "Turn off" }));
                    if e.group != "nyq" {
                        h.push(("^O", "Copy to nyq"));
                    }
                    if e.managed {
                        h.push(("Del", "Remove"));
                    }
                }
                h.extend([("^N", "Install"), ("Esc", "Close")]);
                h
            }
        };
        let install = self.mode == Mode::Install;
        let view = View {
            title: "Skills",
            tabs,
            group: self.list.group,
            total: all.len(),
            filter: &self.list.filter,
            rows: rows.iter().map(|e| row(e)).collect(),
            selected: self.list.selected,
            empty: if self.entries.is_empty() {
                empty_lines()
            } else {
                vec!["No matching skills.".into()]
            },
            preview: current.map(preview).unwrap_or_default(),
            note,
            input: install.then(|| ("Install from".to_string(), self.input.clone())),
            hints,
        };
        menu::render(&view, width, height)
    }
}

/// How the menu sends a skill: `/name` unless a command has that name;
/// `$name` works anywhere.
fn invocation(id: &str, is_command: impl Fn(&str) -> bool) -> String {
    let slash = format!("/{id}");
    if is_command(&slash) {
        format!("${id}")
    } else {
        slash
    }
}

fn copy_command(id: &str, project: bool, force: bool) -> String {
    let mut cmd = format!("/skills copy {id}");
    if project {
        cmd.push_str(" --project");
    }
    if force {
        cmd.push_str(" --force");
    }
    cmd
}

fn row(e: &Entry) -> Row {
    let mut badges = vec![Badge::new(e.skill.scope, GRAY)];
    if e.disabled {
        badges.insert(0, Badge::new("off", RED));
    }
    Row {
        label: e.skill.id.clone(),
        detail: super::render::one_line(&e.skill.description, 300),
        badges,
        dim: e.disabled,
    }
}

fn preview(e: &Entry) -> Vec<String> {
    let s = &e.skill;
    let desc = s
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let files = match e.files {
        0 => "SKILL.md only".to_string(),
        1 => "SKILL.md and 1 more file".to_string(),
        n => format!("SKILL.md and {n} more files"),
    };
    let location = super::statusline::tilde(&s.location);
    let mut place = format!("{} · {} · {location} · {files}", e.group, s.scope);
    if e.disabled {
        place.push_str(" · turned off (disabled_skills)");
    }
    vec![
        if desc.is_empty() {
            "(no description)".into()
        } else {
            desc
        },
        place,
        super::statusline::tilde(&crate::tools::slash_path(&s.path)),
    ]
}

/// What the menu says when no skill was found anywhere.
fn empty_lines() -> Vec<String> {
    use crate::skills::{PROJECT_ROOTS, USER_ROOTS};
    vec![
        "No skills yet.".into(),
        format!("nyq looks in {} (this project)", PROJECT_ROOTS.join(", ")),
        format!("and ~/.nyq/skills, ~/{} (yours).", USER_ROOTS.join(", ~/")),
        "Ctrl+N installs one from a git URL, owner/repo, a skills.sh page or a folder; /skills create NAME starts a new one.".into(),
    ]
}

/// The skills as text, grouped like the menu, for `/skills list` and where
/// no menu can open (piped, `-p`, editors).
pub fn plain_lines(entries: &[Entry]) -> Vec<String> {
    if entries.is_empty() {
        return empty_lines()[..3]
            .iter()
            .cloned()
            .chain(["Install one with /skills install SOURCE [--project].".into()])
            .collect();
    }
    let width = entries
        .iter()
        .map(|e| e.skill.id.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = Vec::new();
    for group in crate::skills::SOURCES {
        let rows: Vec<&Entry> = entries.iter().filter(|e| e.group == *group).collect();
        if rows.is_empty() {
            continue;
        }
        out.push(format!("{group} ({})", rows.len()));
        for e in rows {
            let state = if e.disabled { "  [off]" } else { "" };
            let desc = super::render::one_line(&e.skill.description, 100);
            out.push(
                format!(
                    "  {:width$}  {:7}  {desc}{state}",
                    e.skill.id, e.skill.scope
                )
                .trim_end()
                .to_string(),
            );
        }
    }
    out.push(
        "Use: /skills info|enable|disable|remove NAME · /skills copy NAME [--project] · /skills install SOURCE [--project]".into(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::render::strip_ansi;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn typed(m: &mut SkillsMenu, text: &str) {
        for c in text.chars() {
            m.on_key(key(KeyCode::Char(c)), |_| false);
        }
    }

    fn menu_in(t: &std::path::Path) -> SkillsMenu {
        let (cwd, home) = (t.join("proj"), t.join("home"));
        for (root, name, desc) in [
            (
                cwd.join(".claude/skills"),
                "review",
                "Review a pull request",
            ),
            (cwd.join(".nyq/skills"), "pdf", "Fill PDF forms"),
            (
                cwd.join(".agents/skills"),
                "model",
                "A skill named like a command",
            ),
            (home.join("skills"), "review", "user review"),
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(
                root.join(name).join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {desc}\n---\nbody\n"),
            )
            .unwrap();
        }
        let skills = crate::skills::discover_with(&home, None, &cwd);
        let entries = skills
            .into_iter()
            .map(|skill| Entry {
                group: crate::skills::source(&skill),
                managed: skill.location.contains("nyq"),
                disabled: skill.id == "model",
                files: 0,
                info: Vec::new(),
                skill,
            })
            .collect();
        SkillsMenu::new(
            entries,
            Targets {
                user: home.join("skills"),
                project: cwd.join(".nyq/skills"),
            },
        )
    }

    fn ids(m: &SkillsMenu) -> Vec<String> {
        m.rows().iter().map(|e| e.skill.id.clone()).collect()
    }

    #[test]
    fn groups_filters_runs_and_inserts() {
        let t = tempfile::tempdir().unwrap();
        let mut m = menu_in(t.path());
        assert_eq!(m.groups, ["nyq", "Agents", "Claude Code"]);
        assert_eq!(ids(&m)[0], "pdf");
        assert_eq!(
            ids(&m)[1].split('@').next(),
            Some("review"),
            "project before user within nyq"
        );
        m.on_key(key(KeyCode::Tab), |_| false);
        assert_eq!(m.list.group, Some(0));
        m.on_key(key(KeyCode::Tab), |_| false);
        assert_eq!(ids(&m), ["model"]);
        // A turned-off skill doesn't run; Ctrl+T turns it back on.
        assert_eq!(m.on_key(key(KeyCode::Enter), |_| false), Action::None);
        assert!(m.notice.as_ref().unwrap().0.contains("turned off"));
        assert_eq!(
            m.on_key(ctrl('t'), |_| false),
            Action::Run("/skills enable model".into())
        );
        // A skill named like a command is invoked with `$`.
        assert_eq!(
            m.on_key(ctrl('e'), |c| c == "/model"),
            Action::Insert("$model ".into())
        );
        m.on_key(key(KeyCode::BackTab), |_| false);
        m.on_key(key(KeyCode::BackTab), |_| false);
        assert_eq!(m.list.group, None);
        typed(&mut m, "pdf form");
        assert_eq!(ids(&m), ["pdf"]);
        assert_eq!(
            m.on_key(key(KeyCode::Enter), |_| false),
            Action::Send("/pdf".into())
        );
        assert_eq!(
            m.on_key(ctrl('e'), |_| false),
            Action::Insert("/pdf ".into())
        );
        assert_eq!(
            m.on_key(ctrl('t'), |_| false),
            Action::Run("/skills disable pdf".into())
        );
        assert_eq!(
            m.on_key(key(KeyCode::Right), |_| false),
            Action::View("pdf".into())
        );
        assert_eq!(m.on_key(key(KeyCode::Esc), |_| false), Action::Close);
    }

    #[test]
    fn tabs_count_filtered_rows_and_render_at_80_columns() {
        let t = tempfile::tempdir().unwrap();
        let mut m = menu_in(t.path());
        typed(&mut m, "review");
        let out = m.render(80, 40);
        assert!(out
            .iter()
            .all(|r| super::super::render::visible_width(r) <= 80));
        let text = strip_ansi(&out.join("\n"));
        assert!(
            text.contains("[All 2]")
                && text.contains("nyq 1")
                && text.contains("Agents 0")
                && text.contains("Claude Code 1"),
            "{text}"
        );
        // The preview shows the selected skill's description, source and path.
        assert!(
            text.contains("user review") && text.contains("nyq · user · "),
            "{text}"
        );
        assert!(!text.contains("^O Copy"), "{text}");
        m.on_key(key(KeyCode::Down), |_| false);
        let text = strip_ansi(&m.render(80, 40).join("\n"));
        assert!(
            text.contains("Review a pull request")
                && text.contains("Claude Code · project · .claude/skills"),
            "{text}"
        );
        assert!(text.contains(".claude/skills/review/SKILL.md"), "{text}");
        assert!(
            text.contains("^O Copy to nyq") && !text.contains("Del Remove"),
            "{text}"
        );
        // Matched characters are highlighted.
        assert!(
            out.iter()
                .any(|r| r.contains("\x1b[1;4;") && strip_ansi(r).contains("review")),
            "{out:?}"
        );
    }

    #[test]
    fn removes_only_managed_skills_after_confirming() {
        let t = tempfile::tempdir().unwrap();
        let mut m = menu_in(t.path());
        typed(&mut m, "pull");
        assert_eq!(ids(&m), ["review"]);
        assert_eq!(m.on_key(key(KeyCode::Delete), |_| false), Action::None);
        assert!(m
            .notice
            .as_ref()
            .unwrap()
            .0
            .contains("only removes skills it installed"));
        m.list.filter.clear();
        m.list.selected = 0;
        assert_eq!(m.on_key(ctrl('x'), |_| false), Action::None);
        assert_eq!(m.mode, Mode::Remove("pdf".into()));
        // Anything but Enter or y cancels.
        assert_eq!(m.on_key(key(KeyCode::Char('n')), |_| false), Action::None);
        assert!(m.mode == Mode::List && m.list.filter.is_empty());
        m.on_key(key(KeyCode::Delete), |_| false);
        assert!(strip_ansi(&m.render(80, 40).join("\n")).contains("Remove skill pdf"));
        assert_eq!(
            m.on_key(key(KeyCode::Char('y')), |_| false),
            Action::Run("/skills remove pdf".into())
        );
    }

    #[test]
    fn copies_into_nyq_and_asks_before_replacing() {
        let t = tempfile::tempdir().unwrap();
        let mut m = menu_in(t.path());
        typed(&mut m, "pdf");
        m.on_key(ctrl('o'), |_| false);
        assert!(m.notice.as_ref().unwrap().0.contains("already in nyq"));
        m.list.filter = "pull".into();
        m.on_key(ctrl('o'), |_| false);
        assert!(strip_ansi(&m.render(80, 40).join("\n")).contains("Copy review into nyq's skills"));
        assert_eq!(
            m.on_key(key(KeyCode::Char('p')), |_| false),
            Action::Run("/skills copy review --project".into())
        );
        // Your skills already have a `review`: confirm first.
        m.on_key(ctrl('o'), |_| false);
        assert_eq!(m.on_key(key(KeyCode::Char('u')), |_| false), Action::None);
        assert!(
            strip_ansi(&m.render(80, 40).join("\n")).contains("home/skills/review already exists")
        );
        assert_eq!(
            m.on_key(key(KeyCode::Enter), |_| false),
            Action::None,
            "only y replaces"
        );
        assert_eq!(m.mode, Mode::List);
        // So does a project copy once there is one.
        std::fs::create_dir_all(t.path().join("proj/.nyq/skills/review")).unwrap();
        m.on_key(ctrl('o'), |_| false);
        m.on_key(key(KeyCode::Char('p')), |_| false);
        assert_eq!(
            m.on_key(key(KeyCode::Char('y')), |_| false),
            Action::Run("/skills copy review --project --force".into())
        );
    }

    #[test]
    fn installs_from_the_input_line() {
        let t = tempfile::tempdir().unwrap();
        let mut m = menu_in(t.path());
        m.on_key(ctrl('n'), |_| false);
        typed(&mut m, "owner/repo x");
        m.on_key(key(KeyCode::Backspace), |_| false);
        m.on_key(key(KeyCode::Backspace), |_| false);
        assert!(strip_ansi(&m.render(80, 40).join("\n")).contains("Install from owner/repo"));
        assert!(
            m.list.filter.is_empty(),
            "typing goes to the input, not the filter"
        );
        assert_eq!(
            m.on_key(key(KeyCode::Enter), |_| false),
            Action::Run("/skills install owner/repo".into())
        );
        assert!(strip_ansi(&m.render(80, 40).join("\n")).contains("Installing owner/repo…"));
        // Refreshing clears the progress line and keeps the selection.
        m.list.filter = "pull".into();
        let entries = std::mem::take(&mut m.entries);
        m.set_entries(entries);
        assert!(m.busy.is_none());
        assert_eq!(ids(&m), ["review"]);
        // Esc leaves the input without closing the menu.
        m.on_key(ctrl('n'), |_| false);
        assert_eq!(m.on_key(key(KeyCode::Esc), |_| false), Action::None);
        assert_eq!(m.on_key(key(KeyCode::Esc), |_| false), Action::Close);
    }

    #[test]
    fn empty_state_and_plain_listing() {
        let m = SkillsMenu::new(Vec::new(), Targets::default());
        let text = strip_ansi(&m.render(80, 40).join("\n"));
        assert!(
            text.contains("No skills yet")
                && text.contains(".claude/skills")
                && text.contains("Ctrl+N installs"),
            "{text}"
        );
        assert!(plain_lines(&[])
            .iter()
            .any(|l| l.contains("/skills install SOURCE")));
        let t = tempfile::tempdir().unwrap();
        let m = menu_in(t.path());
        let lines = plain_lines(&m.entries);
        assert_eq!(lines[0], "nyq (2)");
        assert!(lines.iter().any(|l| l.starts_with("Agents (1)")));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("model") && l.ends_with("[off]")),
            "{lines:?}"
        );
        assert!(lines.last().unwrap().contains("/skills copy NAME"));
    }
}
