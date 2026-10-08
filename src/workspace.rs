//! Additional workspace directories, saved per primary workspace in
//! `<home>/workspaces.json`.
//!
//! Saved directories are remembered for the next launch. `--add-dir` adds a
//! directory for this process only. `--no-additional-dirs` ignores the saved
//! list. The active set is what the sandbox may write and what file tools
//! treat as inside the workspace.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Most additional directories one workspace may have active at once.
pub const MAX_ADDITIONAL_DIRECTORIES: usize = 16;

#[derive(Debug)]
pub enum DirError {
    Empty,
    Invalid,
    NotFound(PathBuf),
    NotDirectory(PathBuf),
    Primary,
    Ancestor,
    AlreadyInside,
}

impl std::fmt::Display for DirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirError::Empty => write!(f, "additional directory path is empty"),
            DirError::Invalid => write!(f, "additional directory path is invalid"),
            DirError::NotFound(p) => write!(f, "additional directory not found: {}", p.display()),
            DirError::NotDirectory(p) => write!(f, "not a directory: {}", p.display()),
            DirError::Primary => write!(
                f,
                "cannot add the current workspace as an additional directory"
            ),
            DirError::Ancestor => write!(f, "that directory contains the workspace"),
            DirError::AlreadyInside => write!(f, "that directory is already inside the workspace"),
        }
    }
}

impl std::error::Error for DirError {}

#[derive(Debug)]
pub enum Change {
    Added(PathBuf),
    Already(PathBuf),
}

#[derive(Debug)]
pub enum Action {
    List,
    Add(PathBuf),
    Remove(PathBuf),
    Clear,
}

pub struct View {
    pub active: Vec<PathBuf>,
    pub lines: Vec<String>,
    pub warnings: Vec<String>,
}

pub struct Prepared {
    pub runtime: Runtime,
    pub active: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// What this process knows about additional directories.
pub struct Runtime {
    home: PathBuf,
    primary: PathBuf,
    /// `--no-additional-dirs`: do not activate directories loaded from disk.
    suppress_saved: bool,
    /// Canonical paths from `--add-dir`. `/workspace clear` leaves these in place.
    launch: Vec<PathBuf>,
    /// Directories added with `/workspace add` during this process. They stay
    /// active when saved directories were suppressed, and clear drops them.
    session: Vec<PathBuf>,
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    workspaces: BTreeMap<String, Entry>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct Entry {
    #[serde(default)]
    additional_directories: Vec<String>,
}

impl Runtime {
    pub fn open(home: &Path, primary: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            primary: primary.to_path_buf(),
            suppress_saved: false,
            launch: Vec::new(),
            session: Vec::new(),
        }
    }

    /// Validate `--add-dir` paths and resolve the directories active for this launch.
    pub fn prepare(
        home: &Path,
        primary: &Path,
        cli: &[PathBuf],
        suppress: bool,
    ) -> Result<Prepared> {
        let mut launch = Vec::new();
        for path in cli {
            let canon = check_dir(primary, path)?;
            if !launch.iter().any(|p| p == &canon) {
                launch.push(canon);
            }
        }
        let runtime = Self {
            home: home.to_path_buf(),
            primary: primary.to_path_buf(),
            suppress_saved: suppress,
            launch,
            session: Vec::new(),
        };
        let (active, warnings) = runtime.activate()?;
        Ok(Prepared {
            runtime,
            active,
            warnings,
        })
    }

    pub fn add(&mut self, input: &Path) -> Result<Change> {
        let canon = check_dir(&self.primary, input)?;
        let mut saved = self.read_saved()?;
        if saved.iter().any(|s| Path::new(s) == canon) {
            return Ok(Change::Already(canon));
        }
        if saved.len() >= MAX_ADDITIONAL_DIRECTORIES {
            bail!("at most {MAX_ADDITIONAL_DIRECTORIES} additional directories");
        }
        let previous = saved.clone();
        saved.push(canon.to_string_lossy().into_owned());
        self.write_saved(&saved)?;
        let pushed = self.remember_session(&canon);
        if let Err(e) = self.activate() {
            let _ = self.write_saved(&previous);
            if pushed {
                self.session.pop();
            }
            return Err(e);
        }
        Ok(Change::Added(canon))
    }

    pub fn remove(&mut self, input: &Path) -> Result<PathBuf> {
        let canon = check_dir(&self.primary, input).ok();
        let saved = self.read_saved()?;
        let mut next = Vec::new();
        let mut removed_saved = None;
        for stored in saved {
            if self.matches(&stored, input, canon.as_deref()) {
                removed_saved = Some(canon.clone().unwrap_or_else(|| PathBuf::from(&stored)));
            } else {
                next.push(stored);
            }
        }
        let primary = self.primary.clone();
        let hits = |p: &Path| path_hit(p, input, &primary, canon.as_deref());
        let launch_hit = self.launch.iter().any(|p| hits(p));
        let session_hit = self.session.iter().any(|p| hits(p));
        if removed_saved.is_none() && !launch_hit && !session_hit {
            bail!("no additional directory matches {}", input.display());
        }
        if removed_saved.is_some() {
            self.write_saved(&next)?;
        }
        self.launch.retain(|p| !hits(p));
        self.session.retain(|p| !hits(p));
        Ok(removed_saved
            .or(canon)
            .unwrap_or_else(|| input.to_path_buf()))
    }

    pub fn clear(&mut self) -> Result<()> {
        self.session.clear();
        self.write_saved(&[])
    }

    /// When saved directories are suppressed, a directory added in this process
    /// still has to stay active. Returns whether it was newly recorded.
    fn remember_session(&mut self, canon: &Path) -> bool {
        if !self.suppress_saved
            || self.session.iter().any(|p| p == canon)
            || self.launch.iter().any(|p| p == canon)
        {
            return false;
        }
        self.session.push(canon.to_path_buf());
        true
    }

    pub fn view(&self) -> Result<View> {
        let (active, warnings) = self.activate()?;
        let lines = self.format_lines(&active)?;
        Ok(View {
            active,
            lines,
            warnings,
        })
    }

    fn activate(&self) -> Result<(Vec<PathBuf>, Vec<String>)> {
        let mut warnings = Vec::new();
        let mut dirs = Vec::new();
        if !self.suppress_saved {
            for stored in self.read_saved()? {
                if dirs.len() >= MAX_ADDITIONAL_DIRECTORIES {
                    warnings.push(format!(
                        "ignoring additional directories past the limit of {MAX_ADDITIONAL_DIRECTORIES}"
                    ));
                    break;
                }
                match check_dir(&self.primary, Path::new(&stored)) {
                    Ok(path) => push_unique(&mut dirs, path),
                    Err(e) => {
                        warnings.push(format!("ignoring saved additional directory {stored}: {e}"))
                    }
                }
            }
        }
        for path in self.launch.iter().chain(&self.session) {
            if dirs.iter().any(|dir| dir == path) {
                continue;
            }
            if dirs.len() >= MAX_ADDITIONAL_DIRECTORIES {
                bail!("at most {MAX_ADDITIONAL_DIRECTORIES} additional directories (including --add-dir)");
            }
            dirs.push(path.clone());
        }
        Ok((dirs, warnings))
    }

    fn format_lines(&self, active: &[PathBuf]) -> Result<Vec<String>> {
        let saved = self.read_saved()?;
        let saved_ok: Vec<PathBuf> = saved
            .iter()
            .filter_map(|s| check_dir(&self.primary, Path::new(s)).ok())
            .collect();
        let mut lines = vec![format!("Primary: {}", self.primary.display())];
        if active.is_empty() {
            lines.push("No additional directories.".into());
        } else {
            lines.push("Additional:".into());
            for dir in active {
                if saved_ok.iter().any(|saved| saved == dir) {
                    lines.push(format!("  {}", dir.display()));
                } else {
                    lines.push(format!("  {}  (this launch)", dir.display()));
                }
            }
        }
        if self.suppress_saved {
            let ignored: Vec<&String> = saved
                .iter()
                .filter(|stored| match check_dir(&self.primary, Path::new(stored)) {
                    Ok(path) => !active.iter().any(|dir| dir == &path),
                    Err(_) => true,
                })
                .collect();
            if !ignored.is_empty() {
                lines.push("Saved additional directories are ignored this run:".into());
                for stored in ignored {
                    lines.push(format!("  {stored}"));
                }
            }
        }
        Ok(lines)
    }

    fn matches(&self, stored: &str, input: &Path, canon: Option<&Path>) -> bool {
        path_hit(Path::new(stored), input, &self.primary, canon)
            || canon.is_some_and(|canon| {
                check_dir(&self.primary, Path::new(stored)).ok().as_deref() == Some(canon)
            })
    }

    fn read_saved(&self) -> Result<Vec<String>> {
        let store = self.load_store()?;
        Ok(store
            .workspaces
            .get(&workspace_key(&self.primary))
            .map(|entry| entry.additional_directories.clone())
            .unwrap_or_default())
    }

    fn write_saved(&self, dirs: &[String]) -> Result<()> {
        let mut store = self.load_store()?;
        let key = workspace_key(&self.primary);
        if dirs.is_empty() {
            store.workspaces.remove(&key);
        } else {
            store.workspaces.insert(
                key,
                Entry {
                    additional_directories: dirs.to_vec(),
                },
            );
        }
        self.save_store(&store)
    }

    fn path(&self) -> PathBuf {
        self.home.join("workspaces.json")
    }

    fn load_store(&self) -> Result<Store> {
        let path = self.path();
        match fs::read(&path) {
            Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => Ok(Store::default()),
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is malformed", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn save_store(&self, store: &Store) -> Result<()> {
        let path = self.path();
        if store.workspaces.is_empty() {
            if path.exists() {
                fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            }
            return Ok(());
        }
        let mut text = serde_json::to_string_pretty(store)?;
        text.push('\n');
        crate::session::write_private(&path, text.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// `nyq workspace …` (stdout for the listing, stderr for warnings).
pub fn run_cli(home: &Path, primary: &Path, request: CliRequest<'_>) -> Result<()> {
    let mut runtime = Runtime::open(home, primary);
    match request {
        CliRequest::List => {}
        CliRequest::Add(path) => match runtime.add(path)? {
            Change::Added(path) => println!("Added {}", path.display()),
            Change::Already(path) => {
                println!("Already an additional directory: {}", path.display())
            }
        },
        CliRequest::Remove(path) => {
            let path = runtime.remove(path)?;
            println!("Removed {}", path.display());
        }
        CliRequest::Clear => {
            runtime.clear()?;
            println!("Cleared saved additional directories.");
        }
    }
    let view = runtime.view()?;
    for warning in view.warnings {
        eprintln!("warning: {warning}");
    }
    for line in view.lines {
        println!("{line}");
    }
    Ok(())
}

pub enum CliRequest<'a> {
    List,
    Add(&'a Path),
    Remove(&'a Path),
    Clear,
}

pub fn parse_action(rest: &str) -> Result<Action> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(Action::List);
    }
    let (cmd, path) = match rest.split_once(char::is_whitespace) {
        Some((cmd, path)) => (cmd, path.trim()),
        None => (rest, ""),
    };
    match cmd {
        "list" if path.is_empty() => Ok(Action::List),
        "list" => bail!("Use: /workspace list"),
        "add" if path.is_empty() => bail!("Use: /workspace add <path>"),
        "add" => Ok(Action::Add(PathBuf::from(path))),
        "remove" if path.is_empty() => bail!("Use: /workspace remove <path>"),
        "remove" => Ok(Action::Remove(PathBuf::from(path))),
        "clear" if path.is_empty() => Ok(Action::Clear),
        "clear" => bail!("Use: /workspace clear"),
        other => bail!("unknown /workspace argument `{other}` (try list, add, remove, or clear)"),
    }
}

/// Canonical directory `input` may be added beside `primary`.
pub fn check_dir(primary: &Path, input: &Path) -> Result<PathBuf, DirError> {
    if input.as_os_str().is_empty() {
        return Err(DirError::Empty);
    }
    let expanded = expand_tilde(input);
    if expanded.as_os_str().is_empty() {
        return Err(DirError::Empty);
    }
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        primary.join(expanded)
    };
    let canon = match fs::canonicalize(&absolute) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(DirError::NotFound(absolute))
        }
        Err(_) => return Err(DirError::Invalid),
    };
    if !canon.is_dir() {
        return Err(DirError::NotDirectory(canon));
    }
    let primary_canon = fs::canonicalize(primary)
        .unwrap_or_else(|_| std::path::absolute(primary).unwrap_or_else(|_| primary.to_path_buf()));
    if canon == primary_canon {
        return Err(DirError::Primary);
    }
    if primary_canon.starts_with(&canon) {
        return Err(DirError::Ancestor);
    }
    if canon.starts_with(&primary_canon) {
        return Err(DirError::AlreadyInside);
    }
    Ok(canon)
}

fn expand_tilde(input: &Path) -> PathBuf {
    let text = input.to_string_lossy();
    let rest = if text == "~" {
        Some("")
    } else {
        text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\"))
    };
    match rest {
        Some(rest) => dirs::home_dir()
            .map(|home| {
                if rest.is_empty() {
                    home
                } else {
                    home.join(rest)
                }
            })
            .unwrap_or_else(|| input.to_path_buf()),
        None => input.to_path_buf(),
    }
}

fn workspace_key(primary: &Path) -> String {
    fs::canonicalize(primary)
        .unwrap_or_else(|_| std::path::absolute(primary).unwrap_or_else(|_| primary.to_path_buf()))
        .to_string_lossy()
        .into_owned()
}

fn push_unique(dirs: &mut Vec<PathBuf>, path: PathBuf) {
    if !dirs.iter().any(|dir| dir == &path) {
        dirs.push(path);
    }
}

fn path_hit(stored: &Path, input: &Path, primary: &Path, canon: Option<&Path>) -> bool {
    if let Some(canon) = canon {
        if stored == canon {
            return true;
        }
    }
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        primary.join(input)
    };
    stored == input || stored == absolute
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        (home, primary, extra)
    }

    #[test]
    fn path_checks_reject_primary_children_ancestors_files_and_missing_paths() {
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let file = primary.path().join("notes.txt");
        fs::write(&file, "x").unwrap();
        let child = primary.path().join("src");
        fs::create_dir(&child).unwrap();

        assert!(matches!(
            check_dir(primary.path(), primary.path()),
            Err(DirError::Primary)
        ));
        assert!(matches!(
            check_dir(primary.path(), Path::new(".")),
            Err(DirError::Primary)
        ));
        assert!(matches!(
            check_dir(primary.path(), &child),
            Err(DirError::AlreadyInside)
        ));
        assert!(matches!(
            check_dir(primary.path(), primary.path().parent().unwrap()),
            Err(DirError::Ancestor)
        ));
        assert!(matches!(
            check_dir(primary.path(), &file),
            Err(DirError::NotDirectory(_))
        ));
        assert!(matches!(
            check_dir(primary.path(), &primary.path().join("missing")),
            Err(DirError::NotFound(_))
        ));
        assert!(matches!(
            check_dir(primary.path(), Path::new("")),
            Err(DirError::Empty)
        ));
        assert_eq!(
            check_dir(primary.path(), extra.path()).unwrap(),
            fs::canonicalize(extra.path()).unwrap()
        );

        #[cfg(unix)]
        {
            let link = primary.path().join("self");
            std::os::unix::fs::symlink(primary.path(), &link).unwrap();
            assert!(matches!(
                check_dir(primary.path(), &link),
                Err(DirError::Primary)
            ));
        }
    }

    #[test]
    fn saved_directories_roundtrip_and_count_as_inside_the_workspace() {
        let (home, primary, extra) = pair();
        fs::write(extra.path().join("note.txt"), "hello").unwrap();
        let mut runtime = Runtime::open(home.path(), primary.path());
        let added = match runtime.add(extra.path()).unwrap() {
            Change::Added(path) => path,
            Change::Already(_) => panic!("new directory"),
        };
        assert!(matches!(
            runtime.add(extra.path()).unwrap(),
            Change::Already(_)
        ));

        let again = Runtime::open(home.path(), primary.path());
        let (active, warnings) = again.activate().unwrap();
        assert!(warnings.is_empty());
        assert_eq!(active, vec![added.clone()]);

        let note = added.join("note.txt");
        assert!(crate::agent::inside_any(
            primary.path(),
            &active,
            note.to_str().unwrap()
        ));
        assert!(!crate::agent::inside(
            primary.path(),
            note.to_str().unwrap()
        ));

        let text = fs::read_to_string(home.path().join("workspaces.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let key = workspace_key(primary.path());
        assert_eq!(
            value["workspaces"][&key]["additional_directories"][0],
            added.to_string_lossy().as_ref()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(home.path().join("workspaces.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("secret"), "nope").unwrap();
            let link = added.join("link");
            std::os::unix::fs::symlink(outside.path().join("secret"), &link).unwrap();
            assert!(!crate::agent::inside_any(
                primary.path(),
                &active,
                link.to_str().unwrap()
            ));
        }

        let other = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let mut other_rt = Runtime::open(home.path(), other.path());
        other_rt.add(other_dir.path()).unwrap();
        runtime.remove(extra.path()).unwrap();
        let store: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.path().join("workspaces.json")).unwrap())
                .unwrap();
        assert!(store["workspaces"].get(&key).is_none());
        assert!(store["workspaces"]
            .get(workspace_key(other.path()))
            .is_some());
    }

    #[test]
    fn suppress_ignores_saved_directories_but_keeps_launch_flags() {
        let (home, primary, extra) = pair();
        let launch = tempfile::tempdir().unwrap();
        Runtime::open(home.path(), primary.path())
            .add(extra.path())
            .unwrap();

        let hidden = Runtime::prepare(home.path(), primary.path(), &[], true).unwrap();
        assert!(hidden.active.is_empty());
        assert!(hidden
            .runtime
            .view()
            .unwrap()
            .lines
            .iter()
            .any(|line| line.contains("ignored this run")));

        let shown = Runtime::prepare(
            home.path(),
            primary.path(),
            &[launch.path().to_path_buf()],
            true,
        )
        .unwrap();
        assert_eq!(shown.active.len(), 1);
        assert_eq!(shown.active[0], fs::canonicalize(launch.path()).unwrap());
        assert!(!shown
            .active
            .iter()
            .any(|path| path == &fs::canonicalize(extra.path()).unwrap()));
    }

    #[test]
    fn the_seventeenth_directory_is_rejected() {
        let home = tempfile::tempdir().unwrap();
        let primary = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut runtime = Runtime::open(home.path(), primary.path());
        for i in 0..MAX_ADDITIONAL_DIRECTORIES {
            let path = root.path().join(format!("d{i}"));
            fs::create_dir(&path).unwrap();
            runtime.add(&path).unwrap();
        }
        let overflow = root.path().join("overflow");
        fs::create_dir(&overflow).unwrap();
        assert!(runtime
            .add(&overflow)
            .unwrap_err()
            .to_string()
            .contains("at most"));
        assert_eq!(
            runtime.activate().unwrap().0.len(),
            MAX_ADDITIONAL_DIRECTORIES
        );
    }

    #[test]
    fn a_sibling_path_is_inside_only_when_that_directory_was_added() {
        let root = tempfile::tempdir().unwrap();
        let primary = root.path().join("primary");
        let extra = root.path().join("extra");
        fs::create_dir(&primary).unwrap();
        fs::create_dir(&extra).unwrap();
        fs::write(extra.join("note.txt"), "hello").unwrap();
        let extra = fs::canonicalize(&extra).unwrap();
        assert!(!crate::agent::inside_any(
            &primary,
            std::slice::from_ref(&extra),
            "../missing/note.txt"
        ));
        assert!(crate::agent::inside_any(
            &primary,
            &[extra],
            "../extra/note.txt"
        ));
        assert!(!crate::agent::inside(&primary, "../extra/note.txt"));
    }

    #[test]
    fn a_deleted_saved_directory_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        let primary = tempfile::tempdir().unwrap();
        let gone = tempfile::tempdir().unwrap();
        let mut runtime = Runtime::open(home.path(), primary.path());
        runtime.add(gone.path()).unwrap();
        drop(gone);
        let (active, warnings) = runtime.activate().unwrap();
        assert!(active.is_empty(), "{active:?}");
        assert!(
            warnings.iter().any(|w| w.contains("ignoring saved")),
            "{warnings:?}"
        );
    }

    #[test]
    fn clear_removes_saved_directories_and_leaves_other_workspaces() {
        let (home, primary, extra) = pair();
        let mut runtime = Runtime::open(home.path(), primary.path());
        runtime.add(extra.path()).unwrap();
        runtime.clear().unwrap();
        assert!(runtime.activate().unwrap().0.is_empty());
        assert!(!home.path().join("workspaces.json").exists());
        assert!(runtime.remove(extra.path()).is_err());
    }
}
