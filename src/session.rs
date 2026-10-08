//! Saved conversations: one JSONL file per session under `<home>/sessions`.
//!
//! Line 1 is a header (`{"type":"session",...}`). A later
//! `{"type":"title","title":...}` line is metadata from `/rename` (the latest
//! one wins in listings). Every other line is one [`Item`]. Files are
//! append-only, so a crash can at worst truncate the last line, which loading
//! tolerates.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::Item;

const INTERRUPTED: &str = "Interrupted before this tool ran.";
const TITLE_MAX: usize = 60;
/// Longest `/rename` title, in bytes. Listings may still shorten it.
pub const MAX_TITLE_BYTES: usize = 240;

#[derive(Clone, Debug)]
pub struct Meta {
    pub id: String,
    pub created: DateTime<Utc>,
    pub cwd: PathBuf,
    pub model: String,
    /// `/rename` title, or the first user message when the session was never renamed.
    pub title: String,
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename = "session")]
struct Header {
    #[serde(default = "current_version")]
    version: u32,
    id: String,
    created: DateTime<Utc>,
    cwd: PathBuf,
    model: String,
}

fn current_version() -> u32 {
    1
}

/// Resolve files independently of header readability (needed for salvage).
fn source_path(dir: &Path, which: &str, cwd: &Path) -> Result<PathBuf> {
    if which == "last" {
        return Ok(find(dir, which, cwd)?.path);
    }
    ensure_identifier(which)?;
    let mut matches = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            if let Some(id) = path.file_stem().and_then(|s| s.to_str()) {
                if id == which {
                    return Ok(path);
                }
                if id.starts_with(which) {
                    matches.push(path);
                }
            }
        }
    }
    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => bail!("No session matches '{which}'"),
        _ => bail!(
            "'{which}' is ambiguous; it matches {} sessions",
            matches.len()
        ),
    }
}
fn ensure_identifier(which: &str) -> Result<()> {
    if which.is_empty()
        || !which
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid session id");
    }
    Ok(())
}

/// Salvage readable records, leaving all source bytes unchanged.
pub fn recover(dir: &Path, which: &str, cwd: &Path) -> Result<(String, usize, usize)> {
    let path = source_path(dir, which, cwd)?;
    let data = fs::read(&path)?;
    let header = data
        .split(|b| *b == b'\n')
        .next()
        .and_then(|line| serde_json::from_slice::<Header>(line).ok());
    let mut recovered = Session::create(
        dir,
        header.as_ref().map_or(cwd, |h| h.cwd.as_path()),
        header.as_ref().map_or("unknown", |h| h.model.as_str()),
    )?;
    let mut payload = recovered.pending_header.take().expect("new header");
    payload.push('\n');
    let (mut kept, mut skipped) = (0, 0);
    for (i, line) in data.split(|b| *b == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) || (i == 0 && header.is_some()) {
            continue;
        }
        if let Ok(item) = serde_json::from_slice::<Item>(line) {
            payload.push_str(&serde_json::to_string(&item)?);
            kept += 1;
        } else if is_title_line(line) {
            let value: serde_json::Value = serde_json::from_slice(line)?;
            if value["title"].as_str().is_none() {
                skipped += 1;
                continue;
            }
            payload.push_str(&serde_json::to_string(&value)?);
        } else {
            skipped += 1;
            continue;
        }
        payload.push('\n');
    }
    if kept == 0 {
        bail!("No readable items found; source unchanged");
    }
    write_private(&recovered.path, payload.as_bytes())?;
    Ok((recovered.id.clone(), kept, skipped))
}

/// Upgrade the header atomically; refuse malformed bodies rather than losing data.
pub fn migrate(dir: &Path, which: &str, cwd: &Path) -> Result<bool> {
    let path = source_path(dir, which, cwd)?;
    let data = fs::read(&path)?;
    let first_end = data
        .iter()
        .position(|b| *b == b'\n')
        .map_or(data.len(), |n| n + 1);
    let first = &data[..first_end];
    let mut value = serde_json::from_slice::<serde_json::Value>(first).ok();
    let has_header = value.as_ref().is_some_and(|v| v["type"] == "session");
    let body = if has_header {
        &data[first_end..]
    } else {
        &data[..]
    };
    for (i, line) in body.split(|b| *b == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) || is_title_line(line) {
            continue;
        }
        serde_json::from_slice::<Item>(line)
            .with_context(|| format!("line {} is corrupt; use nyq session recover", i + 1))?;
    }
    let header = if has_header {
        let v = value.as_mut().expect("header value");
        let _: Header = serde_json::from_value(v.clone())
            .context("invalid session header; use nyq session recover")?;
        if let Some(version) = v.get("version") {
            let version = version.as_u64().context("invalid session version")?;
            if version > u64::from(current_version()) {
                bail!("unsupported session version {version}");
            }
            if version == u64::from(current_version()) {
                return Ok(false);
            }
        }
        v["version"] = serde_json::json!(current_version());
        serde_json::to_string(v)?
    } else {
        serde_json::to_string(&Header {
            version: current_version(),
            id: path
                .file_stem()
                .and_then(|s| s.to_str())
                .context("invalid filename")?
                .into(),
            created: Utc::now(),
            cwd: cwd.into(),
            model: "unknown".into(),
        })?
    };
    let mut bytes = header.into_bytes();
    bytes.push(b'\n');
    bytes.extend_from_slice(body);
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    write_private(&path, &bytes)?;
    Ok(true)
}

pub struct Session {
    pub id: String,
    pub path: PathBuf,
    /// Header line still to be written (new sessions, before the first append).
    pending_header: Option<String>,
    /// Items accepted by `append` whose write has not landed yet.
    queued: Vec<String>,
    file: Option<File>,
    /// Held while the session is open (see `lock_session`).
    lock: Option<File>,
}

impl Session {
    /// New session. The file is only created on the first append so that
    /// empty sessions leave nothing behind.
    pub fn create(dir: &Path, cwd: &Path, model: &str) -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let header = Header {
            version: current_version(),
            id: id.clone(),
            created: Utc::now(),
            cwd: cwd.to_path_buf(),
            model: model.to_string(),
        };
        Ok(Self {
            path: dir.join(format!("{id}.jsonl")),
            id,
            pending_header: Some(serde_json::to_string(&header)?),
            queued: Vec::new(),
            file: None,
            lock: None,
        })
    }

    pub fn append(&mut self, item: &Item) -> Result<()> {
        let line = serde_json::to_string(item)
            .with_context(|| format!("encoding a session item for {}", self.id))?;
        self.queued.push(line);
        self.flush_queue()
    }

    /// Write anything still queued from an earlier failed append.
    pub fn flush(&mut self) -> Result<()> {
        self.flush_queue()
    }

    /// Whether the session has been written to disk (and can be resumed).
    pub fn is_saved(&self) -> bool {
        self.pending_header.is_none() && self.path.exists()
    }

    /// Write the header (once) and every queued item. A failed write is rolled
    /// back and the items stay queued for the next attempt.
    fn flush_queue(&mut self) -> Result<()> {
        if self.queued.is_empty() {
            return Ok(());
        }
        self.ensure_file()?;
        let file = self.file.as_mut().expect("file opened above");
        let start = file
            .seek(SeekFrom::End(0))
            .with_context(|| format!("saving session {}", self.id))?;
        let mut payload = String::new();
        if let Some(header) = &self.pending_header {
            payload.push_str(header);
            payload.push('\n');
        }
        for line in &self.queued {
            payload.push_str(line);
            payload.push('\n');
        }
        if let Err(e) = file
            .write_all(payload.as_bytes())
            .and_then(|_| file.flush())
        {
            let _ = rollback(file, start);
            return Err(e).with_context(|| format!("saving session {}", self.id));
        }
        self.pending_header = None;
        self.queued.clear();
        Ok(())
    }

    fn ensure_file(&mut self) -> Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                        .with_context(|| format!("securing {}", dir.display()))?;
                }
            }
        }
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("securing {}", self.path.display()))?;
        }
        if self.lock.is_none() {
            self.lock = Some(lock_session(&self.path)?);
        }
        self.file = Some(file);
        Ok(())
    }

    /// Append a title metadata line. The latest one is what listings show.
    pub fn set_title(&mut self, title: &str) -> Result<()> {
        let line = serde_json::to_string(&serde_json::json!({ "type": "title", "title": title }))
            .with_context(|| format!("encoding a session title for {}", self.id))?;
        self.queued.push(line);
        self.flush_queue()
    }

    /// Replace the file with its header, keeping the session id and settings.
    /// The new header is written by rename, then the file is reopened for append,
    /// so a crash cannot leave a zero-length session.
    pub fn clear_transcript(&mut self) -> Result<()> {
        self.queued.clear();
        let header = self.header_line()?;
        if self.file.is_none() && !self.path.exists() {
            self.pending_header = Some(header);
            return Ok(());
        }
        let mut bytes = header.into_bytes();
        bytes.push(b'\n');
        // Drop the old handle so the replacement can be opened (the session
        // keeps its lock, which lives on a separate file).
        self.file.take();
        write_private(&self.path, &bytes).with_context(|| format!("clearing session {}", self.id))?;
        self.ensure_file()?;
        self.pending_header = None;
        Ok(())
    }

    fn header_line(&self) -> Result<String> {
        if let Some(header) = &self.pending_header {
            return Ok(header.clone());
        }
        let file =
            File::open(&self.path).with_context(|| format!("reading {}", self.path.display()))?;
        let mut line = String::new();
        BufReader::new(file)
            .read_line(&mut line)
            .with_context(|| format!("reading {}", self.path.display()))?;
        if line.ends_with('\n') {
            line.pop();
        }
        if line.ends_with('\r') {
            line.pop();
        }
        if line.is_empty() {
            bail!("session {} is missing its header", self.id);
        }
        Ok(line)
    }
}

/// Exclusive lock for the life of the open session file. A second process
/// that already holds it is refused instead of interleaving appends or
/// trimming the file out from under a live turn.
///
/// Releasing the previous handle and locking the new one can overlap for a
/// moment (the old descriptor is closed on another thread, or `close` has not
/// dropped the lock yet), so a busy lock is retried briefly. A session that
/// is actually still open stays locked for much longer than this.
fn lock_session(path: &Path) -> Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", path.display()))?;
    }
    // Lock a sidecar file, not the transcript: Windows locks are mandatory
    // and would stop every other reader (listing, resume, copy) of the log.
    let lock_path = lock_path(path);
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(&lock_path).with_context(|| format!("opening {}", lock_path.display()))?;
    let mut tries = 0u32;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                tries += 1;
                if tries == 20 {
                    bail!("{} is already open in another nyq process", path.display());
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
}

/// `<id>.jsonl` → `<id>.jsonl.lock`.
fn lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(lock) = self.lock.as_ref() {
            let _ = lock.unlock();
        }
    }
}

fn rollback(file: &mut File, start: u64) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(start))?;
    file.set_len(start)
}

/// Atomically replace `path` (file 0600, parent directory 0700 on Unix).
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        builder.mode(0o700);
        opts.mode(0o600);
    }
    builder.create(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut f = opts.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    crate::tools::safeio::rename_over(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// All sessions, newest first.
pub fn list(dir: &Path) -> Result<Vec<Meta>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut all: Vec<Meta> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| read_meta(&p))
        .collect();
    all.sort_by_key(|m| std::cmp::Reverse(m.created));
    Ok(all)
}

/// Header plus title (first user message); `None` if the file is unreadable.
fn read_meta(path: &Path) -> Option<Meta> {
    let mut lines = BufReader::new(File::open(path).ok()?).lines();
    let header: Header = serde_json::from_str(&lines.next()?.ok()?).ok()?;
    let mut custom = None;
    let mut derived = None;
    for line in lines.map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("title") => {
                if let Some(title) = value
                    .get("title")
                    .and_then(|t| t.as_str())
                    .filter(|t| !t.is_empty())
                {
                    custom = Some(title.to_string());
                }
            }
            Some("user") if derived.is_none() => {
                if let Some(text) = value.get("text").and_then(|t| t.as_str()) {
                    derived = Some(title_of(text));
                }
            }
            _ => {}
        }
    }
    let title = custom.or(derived).unwrap_or_else(|| "(untitled)".into());
    Some(Meta {
        id: header.id,
        created: header.created,
        cwd: header.cwd,
        model: header.model,
        title,
        path: path.into(),
    })
}

/// A user-supplied `/rename` title: trimmed, printable, at most [`MAX_TITLE_BYTES`].
pub fn validate_title(raw: &str) -> Result<String> {
    let title = raw.trim();
    if title.is_empty() {
        bail!("Use: /rename <title>");
    }
    if title.len() > MAX_TITLE_BYTES {
        bail!("title is too long");
    }
    if title.chars().any(char::is_control) {
        bail!("title must be printable text");
    }
    Ok(title.to_string())
}

/// Turn a model's reply into a 1–6 word title.
pub fn generated_title(raw: &str) -> Result<String> {
    let line = raw
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let line = line
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’'));
    let mut words: Vec<&str> = line.split_whitespace().collect();
    if words.len() > 6 {
        words.truncate(6);
    }
    let mut title = words.join(" ");
    while title.ends_with(['.', '!', '?', ',', ':', ';']) {
        title.pop();
        title = title.trim_end().to_string();
    }
    let title: String = title.chars().filter(|c| !c.is_control()).collect();
    validate_title(&title).map_err(|_| anyhow::anyhow!("the model did not return a usable title"))
}

fn is_title_line(line: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(|kind| kind.as_str())
                .map(|kind| kind == "title")
        })
        .unwrap_or(false)
}

fn title_of(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() > TITLE_MAX {
        let cut: String = line.chars().take(TITLE_MAX - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        line.to_string()
    }
}

/// Resolve `last`, an id, or a unique id prefix to one session in `dir`.
pub fn find(dir: &Path, which: &str, cwd: &Path) -> Result<Meta> {
    let all = list(dir)?;
    if all.is_empty() {
        bail!("No saved sessions in {}", dir.display());
    }
    if which == "last" {
        return all.into_iter().find(|m| same_cwd(&m.cwd, cwd)).ok_or_else(|| {
            anyhow::anyhow!("No saved conversation in this directory (use -r to pick one)")
        });
    }
    if let Some(meta) = all.iter().find(|m| m.id == which) {
        return Ok(meta.clone());
    }
    let matches: Vec<Meta> = all
        .into_iter()
        .filter(|m| m.id.starts_with(which))
        .collect();
    match matches.as_slice() {
        [] => bail!("No session matches '{which}' (see `nyq sessions` or /sessions)."),
        [meta] => Ok(meta.clone()),
        many => bail!(
            "'{which}' is ambiguous; it matches {} sessions: {}",
            many.len(),
            many.iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Print a short transcript summary for `nyq session show`.
pub fn print_summary(dir: &Path, which: &str, cwd: &Path) -> Result<()> {
    let meta = find(dir, which, cwd)?;
    let items = read_stored_items(&meta.path)?;
    println!("{}", format_summary(&meta, &items));
    Ok(())
}

/// Delete one saved session. Does not open it for append.
pub fn delete(dir: &Path, which: &str, cwd: &Path) -> Result<Meta> {
    let meta = find(dir, which, cwd)?;
    fs::remove_file(&meta.path).with_context(|| format!("deleting {}", meta.path.display()))?;
    let _ = fs::remove_file(lock_path(&meta.path));
    // Its named subagents go with it.
    let _ = fs::remove_dir_all(crate::subagents::store_dir(&meta.path));
    if let Some(home) = dir.parent() {
        crate::tools::artifacts::remove_session(home, &meta.id);
    }
    Ok(meta)
}

/// Whether two working directories are the same place. Canonical paths match
/// through symlinks; Windows and macOS also ignore ASCII case.
fn same_cwd(saved: &Path, current: &Path) -> bool {
    if saved == current {
        return true;
    }
    let left = fs::canonicalize(saved).unwrap_or_else(|_| saved.to_path_buf());
    let right = fs::canonicalize(current).unwrap_or_else(|_| current.to_path_buf());
    if left == right {
        return true;
    }
    case_fold_eq(&left, &right)
}

#[cfg(any(windows, target_os = "macos"))]
fn case_fold_eq(left: &Path, right: &Path) -> bool {
    left.to_string_lossy().eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn case_fold_eq(_: &Path, _: &Path) -> bool {
    false
}

pub fn format_summary(meta: &Meta, items: &[Item]) -> String {
    let mut users = 0usize;
    let mut assistants = 0usize;
    let mut tools = 0usize;
    let mut recent = Vec::new();
    for item in items {
        match item {
            Item::User { text, .. } => {
                users += 1;
                recent.push(format!("user: {}", preview_line(text)));
            }
            Item::Assistant { text } if !text.trim().is_empty() => {
                assistants += 1;
                recent.push(format!("assistant: {}", preview_line(text)));
            }
            Item::ToolCall { name, .. } => {
                tools += 1;
                recent.push(format!("tool: {name}"));
            }
            Item::Compaction { summary } => {
                recent.push(format!("summary: {}", preview_line(summary)))
            }
            _ => {}
        }
    }
    let shown = if recent.len() > 8 {
        &recent[recent.len() - 8..]
    } else {
        recent.as_slice()
    };
    let mut out = format!(
        "{}\ntitle: {}\ncreated: {}\ndirectory: {}\nmodel: {}\n{} messages · {} user · {} assistant · {} tool calls\n",
        meta.id,
        meta.title,
        meta.created.to_rfc3339(),
        meta.cwd.display(),
        meta.model,
        items.len(),
        users,
        assistants,
        tools,
    );
    if shown.is_empty() {
        out.push_str("\n(no messages yet)\n");
    } else {
        out.push('\n');
        for line in shown {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn preview_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    const MAX: usize = 120;
    if line.chars().count() > MAX {
        let cut: String = line.chars().take(MAX - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        line.to_string()
    }
}

fn read_stored_items(path: &Path) -> Result<Vec<Item>> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(read_items(&data, path)?.0)
}

/// Load a session by id, unique id prefix, or `last` (newest in the
/// given cwd). Returns the session reopened for appending plus its items.
pub fn load(dir: &Path, which: &str, cwd: &Path) -> Result<(Session, Meta, Vec<Item>)> {
    let meta = find(dir, which, cwd)?;

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&meta.path)
        .with_context(|| format!("opening {}", meta.path.display()))?;
    let lock = lock_session(&meta.path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .with_context(|| format!("reading {}", meta.path.display()))?;
    let (mut items, good) = read_items(&data, &meta.path)?;
    if good < data.len() || !data[..good].ends_with(b"\n") {
        // Drop a half-written final record (including a torn UTF-8 byte) and
        // make sure appends start on a fresh line. The lock is already held.
        file.set_len(good as u64)
            .with_context(|| format!("repairing {}", meta.path.display()))?;
        if good == 0 || !data[..good].ends_with(b"\n") {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
            file.flush()?;
        }
    }
    let mut session = Session {
        id: meta.id.clone(),
        path: meta.path.clone(),
        pending_header: None,
        queued: Vec::new(),
        file: Some(file),
        lock: Some(lock),
    };
    for item in repairs(&items) {
        session.append(&item)?;
        items.push(item);
    }
    // Everything before the latest compaction is summarized by it.
    if let Some(i) = items
        .iter()
        .rposition(|it| matches!(it, Item::Compaction { .. }))
    {
        items.drain(..i);
    }
    Ok((session, meta, items))
}

/// Items after the header plus the byte length of the well-formed prefix.
/// An unparsable final record (crash mid-write, including torn UTF-8) is
/// ignored; an unparsable record elsewhere is an error.
fn read_items(data: &[u8], path: &Path) -> Result<(Vec<Item>, usize)> {
    let mut items = Vec::new();
    let mut good = 0;
    let mut offset = 0;
    let mut n = 0usize;
    while offset < data.len() {
        let end = data[offset..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| offset + i + 1)
            .unwrap_or(data.len());
        let raw = &data[offset..end];
        let mut line = raw.strip_suffix(b"\n").unwrap_or(raw);
        line = line.strip_suffix(b"\r").unwrap_or(line);
        let blank = line.iter().all(u8::is_ascii_whitespace);
        if n > 0 && !blank && !is_title_line(line) {
            match serde_json::from_slice::<Item>(line) {
                Ok(item) => items.push(item),
                Err(_) if end == data.len() => break,
                Err(e) => bail!("{}: line {} is corrupt: {e}", path.display(), n + 1),
            }
        }
        good = end;
        offset = end;
        n += 1;
    }
    Ok((items, good))
}

/// Synthetic results for trailing tool calls that never got one, so the next
/// request to the model is well-formed.
pub(crate) fn repairs(items: &[Item]) -> Vec<Item> {
    let tail_start = items
        .iter()
        .rposition(|i| !matches!(i, Item::ToolCall { .. } | Item::ToolResult { .. }))
        .map_or(0, |p| p + 1);
    let tail = &items[tail_start..];
    let answered: HashSet<&str> = tail
        .iter()
        .filter_map(|i| match i {
            Item::ToolResult { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    tail.iter()
        .filter_map(|i| match i {
            Item::ToolCall { id, .. } if !answered.contains(id.as_str()) => {
                Some(Item::ToolResult {
                    id: id.clone(),
                    output: INTERRUPTED.into(),
                    is_error: true,
                })
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(t: &str) -> Item {
        Item::user(t)
    }
    fn call(id: &str) -> Item {
        Item::ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn create_is_lazy_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let mut s = Session::create(&dir, Path::new("/w"), "m1").unwrap();
        assert!(!dir.exists());
        assert!(list(&dir).unwrap().is_empty());
        s.append(&user("hello there\nsecond line")).unwrap();
        s.append(&Item::Assistant { text: "hi".into() }).unwrap();
        let first = fs::read_to_string(&s.path).unwrap();
        let header: serde_json::Value =
            serde_json::from_str(first.lines().next().unwrap()).unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(header["id"], s.id.as_str());
        assert_eq!(header["model"], "m1");

        let metas = list(&dir).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].title, "hello there");
        let id = s.id.clone();
        drop(s);
        let (_, meta, items) = load(&dir, &id, Path::new("/w")).unwrap();
        assert_eq!(meta.cwd, Path::new("/w"));
        assert_eq!(
            items,
            vec![
                user("hello there\nsecond line"),
                Item::Assistant { text: "hi".into() }
            ]
        );
    }

    #[test]
    fn title_truncates() {
        let t = title_of(&"x".repeat(100));
        assert_eq!(t.chars().count(), TITLE_MAX);
        assert!(t.ends_with('…'));
        assert_eq!(title_of("  \n short "), "short");
    }

    #[test]
    fn last_prefix_and_ambiguity() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut ids = Vec::new();
        for (cwd, text) in [("/a", "one"), ("/b", "two"), ("/c", "three")] {
            let mut s = Session::create(dir, Path::new(cwd), "m").unwrap();
            s.append(&user(text)).unwrap();
            ids.push(s.id.clone());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Newest first.
        let titles: Vec<_> = list(dir).unwrap().into_iter().map(|m| m.title).collect();
        assert_eq!(titles, ["three", "two", "one"]);
        assert_eq!(load(dir, "last", Path::new("/a")).unwrap().1.title, "one");
        assert!(load(dir, "last", Path::new("/zzz"))
            .err()
            .unwrap()
            .to_string()
            .contains("No saved conversation in this directory"));
        assert_eq!(
            load(dir, &ids[1][..8], Path::new("/")).unwrap().1.title,
            "two"
        );
        assert!(load(dir, "", Path::new("/"))
            .err()
            .unwrap()
            .to_string()
            .contains("ambiguous"));
        assert!(load(dir, "nope", Path::new("/"))
            .err()
            .unwrap()
            .to_string()
            .contains("No session"));
        // Garbage files are skipped silently.
        fs::write(dir.join("junk.jsonl"), "not json\n").unwrap();
        assert_eq!(list(dir).unwrap().len(), 3);
    }

    #[test]
    fn truncated_tail_and_repair() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut s = Session::create(dir, Path::new("/w"), "m").unwrap();
        s.append(&user("run it")).unwrap();
        s.append(&call("c1")).unwrap();
        s.append(&call("c2")).unwrap();
        s.append(&Item::ToolResult {
            id: "c1".into(),
            output: "ok".into(),
            is_error: false,
        })
        .unwrap();
        drop(s);
        let path = dir.join(
            fs::read_dir(dir)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name(),
        );
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"type":"tool_res"#).unwrap();
        drop(f);

        let (mut s, _, items) = load(dir, "last", Path::new("/w")).unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!(
            items[4],
            Item::ToolResult {
                id: "c2".into(),
                output: INTERRUPTED.into(),
                is_error: true
            }
        );
        s.append(&Item::Assistant {
            text: "done".into(),
        })
        .unwrap();

        // The file is consistent: reloading needs no further repair.
        drop(s);
        let (_, _, again) = load(dir, "last", Path::new("/w")).unwrap();
        assert_eq!(again.len(), 6);
        assert_eq!(again[4], items[4]);
        assert!(repairs(&again).is_empty());
    }

    #[test]
    fn complete_last_line_without_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.jsonl");
        let header = r#"{"type":"session","id":"abc","created":"2026-01-01T00:00:00Z","cwd":"/w","model":"m"}"#;
        fs::write(
            &path,
            format!("{header}\n{}", serde_json::to_string(&user("hi")).unwrap()),
        )
        .unwrap();
        let (mut s, meta, items) = load(tmp.path(), "abc", Path::new("/w")).unwrap();
        assert_eq!(meta.title, "hi");
        assert_eq!(items, vec![user("hi")]);
        s.append(&user("again")).unwrap();
        drop(s);
        assert_eq!(load(tmp.path(), "abc", Path::new("/w")).unwrap().2.len(), 2);
    }

    #[test]
    fn torn_utf8_tail_keeps_the_previous_item() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut s = Session::create(dir, Path::new("/w"), "m").unwrap();
        s.append(&user("kept")).unwrap();
        drop(s);
        let path = dir.join(
            fs::read_dir(dir)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name(),
        );
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[0xc3])
            .unwrap();
        let (_, _, items) = load(dir, "last", Path::new("/w")).unwrap();
        assert_eq!(items, vec![user("kept")]);
    }

    #[test]
    fn failed_append_keeps_the_header_and_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("sessions");
        fs::write(&blocker, "x").unwrap();
        let mut s = Session::create(&blocker, Path::new("/w"), "m").unwrap();
        assert!(s.append(&user("one")).is_err());
        fs::remove_file(&blocker).unwrap();
        s.append(&user("two")).unwrap();
        drop(s);
        let (_, _, items) = load(&blocker, "last", Path::new("/w")).unwrap();
        assert_eq!(items, vec![user("one"), user("two")]);
    }

    #[test]
    fn partial_record_is_rolled_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        f.write_all(b"header\n").unwrap();
        let start = f.stream_position().unwrap();
        f.write_all(b"PARTIAL").unwrap();
        rollback(&mut f, start).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"header\n");
    }

    #[cfg(unix)]
    #[test]
    fn session_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let mut s = Session::create(&dir, Path::new("/w"), "m").unwrap();
        s.append(&user("hi")).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&s.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(s);
        let path = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&path, perms).unwrap();
        let (mut s, _, _) = load(&dir, "last", Path::new("/w")).unwrap();
        s.append(&user("again")).unwrap();
        assert_eq!(
            fs::metadata(&s.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_title_line_overrides_the_first_message_and_is_not_an_item() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut s = Session::create(dir, Path::new("/w"), "m").unwrap();
        s.append(&user("hello there")).unwrap();
        s.set_title("Ship the parser").unwrap();
        s.append(&Item::Assistant {
            text: "done".into(),
        })
        .unwrap();
        s.set_title("say \"hi\"").unwrap();
        let raw = fs::read_to_string(&s.path).unwrap();
        assert!(raw.contains("\"type\":\"title\""));
        assert_eq!(list(dir).unwrap()[0].title, "say \"hi\"");
        drop(s);
        let (mut s, meta, items) = load(dir, "last", Path::new("/w")).unwrap();
        assert_eq!(meta.title, "say \"hi\"");
        assert_eq!(
            items,
            vec![
                user("hello there"),
                Item::Assistant {
                    text: "done".into()
                }
            ]
        );
        let summary = format_summary(&meta, &items);
        assert!(summary.contains("title: say \"hi\""));
        assert!(summary.contains("user: hello there"));
        assert!(summary.contains("model: m"));

        let id = s.id.clone();
        s.clear_transcript().unwrap();
        let raw = fs::read(&s.path).unwrap();
        assert!(!raw.is_empty());
        assert!(raw.starts_with(b"{"), "a crash window must not leave an empty file");
        let metas = list(dir).unwrap();
        assert_eq!(metas[0].id, id);
        assert_eq!(metas[0].title, "(untitled)");
        drop(s);
        assert!(load(dir, &id, Path::new("/w")).unwrap().2.is_empty());
        delete(dir, &id[..8], Path::new("/w")).unwrap();
        assert!(list(dir).unwrap().is_empty());
    }

    #[test]
    fn recovery_skips_broken_lines_and_preserves_source() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("broken.jsonl");
        let mut data = b"broken header\n".to_vec();
        data.extend_from_slice(serde_json::to_string(&user("one")).unwrap().as_bytes());
        data.extend_from_slice(b"\n{bad}\n");
        data.extend_from_slice(serde_json::to_string(&user("two")).unwrap().as_bytes());
        data.extend_from_slice(b"\n{truncated");
        fs::write(&path, &data).unwrap();
        let (id, kept, skipped) = recover(tmp.path(), "broken", Path::new("/w")).unwrap();
        assert_eq!((kept, skipped), (2, 3));
        assert_eq!(fs::read(path).unwrap(), data);
        assert_eq!(
            load(tmp.path(), &id, Path::new("/w")).unwrap().2,
            vec![user("one"), user("two")]
        );
        assert!(recover(tmp.path(), "../broken", Path::new("/w")).is_err());
    }

    #[test]
    fn migration_refuses_corruption_without_changing_source() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("corrupt.jsonl");
        let data = b"{broken}\n";
        fs::write(&path, data).unwrap();
        assert!(migrate(tmp.path(), "corrupt", Path::new("/w")).is_err());
        assert_eq!(fs::read(path).unwrap(), data);
    }

    #[test]
    fn migration_is_idempotent_and_keeps_first_item() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("legacy.jsonl");
        fs::write(&path, serde_json::to_vec(&user("first")).unwrap()).unwrap();
        assert!(migrate(tmp.path(), "legacy", Path::new("/w")).unwrap());
        let once = fs::read(&path).unwrap();
        assert!(!migrate(tmp.path(), "legacy", Path::new("/w")).unwrap());
        assert_eq!(fs::read(&path).unwrap(), once);
        assert_eq!(
            load(tmp.path(), "legacy", Path::new("/w")).unwrap().2,
            vec![user("first")]
        );
        let mut header: serde_json::Value =
            serde_json::from_slice(once.split(|b| *b == b'\n').next().unwrap()).unwrap();
        header.as_object_mut().unwrap().remove("version");
        let legacy = format!(
            "{header}\n{}\n",
            serde_json::to_string(&user("first")).unwrap()
        );
        fs::write(&path, legacy).unwrap();
        assert!(migrate(tmp.path(), "legacy", Path::new("/w")).unwrap());
        assert!(!migrate(tmp.path(), "legacy", Path::new("/w")).unwrap());
    }

    #[test]
    fn titles_are_trimmed_capped_and_printable() {
        assert_eq!(
            validate_title("  Fix the parser  ").unwrap(),
            "Fix the parser"
        );
        assert!(validate_title("  ")
            .unwrap_err()
            .to_string()
            .contains("/rename"));
        assert!(validate_title("bad\ntitle").is_err());
        assert!(validate_title(&"a".repeat(MAX_TITLE_BYTES + 1))
            .unwrap_err()
            .to_string()
            .contains("too long"));
        assert_eq!(
            validate_title(&"é".repeat(MAX_TITLE_BYTES / 2))
                .unwrap()
                .len(),
            MAX_TITLE_BYTES
        );
        assert_eq!(
            generated_title("\"Fix the parser bug.\"\nextra").unwrap(),
            "Fix the parser bug"
        );
        assert_eq!(
            generated_title("one two three four five six seven").unwrap(),
            "one two three four five six"
        );
        assert!(generated_title("   ").is_err());
    }

    #[test]
    fn a_second_open_is_refused_until_the_first_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut s = Session::create(dir, Path::new("/w"), "m").unwrap();
        s.append(&user("hi")).unwrap();
        match load(dir, "last", Path::new("/w")) {
            Err(e) => assert!(e.to_string().contains("already open"), "{e}"),
            Ok(_) => panic!("a second open of the same session should be refused"),
        }
        let id = s.id.clone();
        drop(s);
        assert_eq!(load(dir, &id, Path::new("/w")).unwrap().2, vec![user("hi")]);
    }

    #[test]
    fn deleting_a_session_removes_its_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let mut s = Session::create(&sessions, Path::new("/w"), "m").unwrap();
        s.append(&user("hi")).unwrap();
        let big = "x".repeat(crate::tools::artifacts::SPILL_AT + 8);
        let _ = crate::tools::artifacts::spill(tmp.path(), &s.id, "c", "bash", big);
        let id = s.id.clone();
        assert!(tmp.path().join("artifacts").join(&id).is_dir());
        drop(s);
        delete(&sessions, &id, Path::new("/w")).unwrap();
        assert!(!tmp.path().join("artifacts").join(&id).exists());
    }

    #[cfg(unix)]
    #[test]
    fn last_matches_a_directory_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("proj");
        fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut s = Session::create(tmp.path(), &link, "m").unwrap();
        s.append(&user("via link")).unwrap();
        drop(s);
        assert_eq!(find(tmp.path(), "last", &real).unwrap().title, "via link");
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn last_ignores_ascii_case_when_the_path_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let upper = tmp.path().join("NoSuch");
        let lower = tmp.path().join("nosuch");
        assert!(same_cwd(&upper, &lower));
    }
}
