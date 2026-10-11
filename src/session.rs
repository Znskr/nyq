//! Saved conversations: one JSONL file per session under `<home>/sessions`.
//!
//! Line 1 is a header (`{"type":"session",...}`). Later metadata lines are
//! `{"type":"title","title":...}` from `/rename`, `{"type":"prefs",...}` with
//! the model settings in effect, and `{"type":"permissions","state":...}` with
//! the session's permission grants (the latest of each wins). Every other
//! line is one [`Item`]. Files are append-only, so a crash can at worst
//! truncate the last line, which loading tolerates.
//!
//! Listings read each file once and cache the result in `<id>.summary`,
//! reused while the transcript's size and modification time are unchanged.

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
const PREVIEW_MAX: usize = 240;
/// Longest `/rename` title, in bytes. Listings may still shorten it.
pub const MAX_TITLE_BYTES: usize = 240;
/// Shown when no prompt can name the session.
pub const UNTITLED: &str = "Untitled session";
/// The title of a session whose first prompt is only images.
pub const IMAGE_TITLE: &str = "Image session";
/// Sessions larger than this are reported by `nyq doctor`.
pub const OVERSIZED_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub created: DateTime<Utc>,
    /// Last activity: when the transcript was last written.
    pub updated: DateTime<Utc>,
    pub cwd: PathBuf,
    pub model: String,
    /// `/rename` title, or the first prompt when the session was never renamed.
    pub title: String,
    /// The first prompt, on one line (searched by the picker).
    #[serde(default)]
    pub preview: String,
    /// User turns in the whole transcript.
    #[serde(default)]
    pub turns: usize,
    /// Model settings last used in the session.
    #[serde(default)]
    pub prefs: Option<Prefs>,
    #[serde(skip)]
    pub path: PathBuf,
}

/// Model settings saved with a session and restored when it is resumed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Prefs {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub fast: bool,
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
        } else if is_meta_line(line) {
            let value: serde_json::Value = serde_json::from_slice(line)?;
            let valid = match value["type"].as_str() {
                Some("title") => value["title"].is_string(),
                Some("permissions") => true,
                _ => serde_json::from_value::<Prefs>(value.clone()).is_ok(),
            };
            if !valid {
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
        if line.iter().all(u8::is_ascii_whitespace) || is_meta_line(line) {
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
    /// A prefs line not yet written; a new session writes it with its header.
    pending_prefs: Option<String>,
    /// The latest prefs given to `set_prefs` (or read when loading).
    prefs: Option<Prefs>,
    file: Option<File>,
    /// Held while the session is open (see `lock_session`).
    lock: Option<File>,
    /// The latest permissions line: read on load, then what was last written.
    permissions: Option<serde_json::Value>,
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
            pending_prefs: None,
            prefs: None,
            file: None,
            lock: None,
            permissions: None,
        })
    }

    /// Record the model settings in use. Only changes are written; a session
    /// that is not saved yet writes them with its first item.
    pub fn set_prefs(&mut self, prefs: &Prefs) -> Result<()> {
        if self.prefs.as_ref() == Some(prefs) {
            return Ok(());
        }
        let mut value = serde_json::to_value(prefs)?;
        value["type"] = "prefs".into();
        self.pending_prefs = Some(serde_json::to_string(&value)?);
        self.prefs = Some(prefs.clone());
        self.flush_queue()
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
        let prefs_only = self.pending_prefs.is_some() && self.pending_header.is_none();
        if self.queued.is_empty() && !prefs_only {
            return Ok(());
        }
        self.ensure_file()?;
        let file = self.file.as_mut().expect("file opened above");
        let start = file
            .seek(SeekFrom::End(0))
            .with_context(|| format!("saving session {}", self.id))?;
        let mut payload = String::new();
        for line in self
            .pending_header
            .iter()
            .chain(&self.pending_prefs)
            .chain(&self.queued)
        {
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
        self.pending_prefs = None;
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

    /// Record the session's permission state (skipped when unchanged).
    pub fn save_permissions(&mut self, state: serde_json::Value) -> Result<()> {
        if self.permissions.as_ref() == Some(&state) {
            return Ok(());
        }
        let line = serde_json::to_string(&serde_json::json!({ "type": "permissions", "state": state }))
            .with_context(|| format!("encoding permissions for {}", self.id))?;
        self.queued.push(line);
        self.flush_queue()?;
        self.permissions = Some(state);
        Ok(())
    }

    /// The permission state saved with this session, if any.
    pub fn saved_permissions(&self) -> Option<&serde_json::Value> {
        self.permissions.as_ref()
    }

    /// Replace the file with its header, keeping the session id and settings.
    /// The new header is written by rename, then the file is reopened for append,
    /// so a crash cannot leave a zero-length session.
    pub fn clear_transcript(&mut self) -> Result<()> {
        self.queued.clear();
        self.permissions = None;
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
        // The settings stay; write them again.
        match self.prefs.take() {
            Some(prefs) => self.set_prefs(&prefs),
            None => Ok(()),
        }
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

/// All sessions, most recently active first.
pub fn list(dir: &Path) -> Result<Vec<Meta>> {
    Ok(list_report(dir)?.0)
}

/// All readable sessions (most recently active first) and the paths of
/// session files that could not be read.
pub fn list_report(dir: &Path) -> Result<(Vec<Meta>, Vec<PathBuf>)> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let (mut all, mut skipped) = (Vec::new(), Vec::new());
    for path in entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
    {
        match cached_meta(&path) {
            Some(meta) => all.push(meta),
            None => skipped.push(path),
        }
    }
    all.sort_by_key(|m: &Meta| std::cmp::Reverse(m.updated));
    Ok((all, skipped))
}

/// The `nyq sessions` warning for unreadable files, if any.
pub fn skipped_warning(skipped: usize) -> Option<String> {
    (skipped > 0).then(|| {
        format!("skipped {skipped} unreadable saved session{} (see nyq doctor)", if skipped == 1 { "" } else { "s" })
    })
}

/// `<id>.jsonl` → `<id>.summary`.
fn summary_path(path: &Path) -> PathBuf {
    path.with_extension("summary")
}

#[derive(Serialize, Deserialize)]
struct Summary {
    len: u64,
    modified: DateTime<Utc>,
    meta: Meta,
}

/// [`read_meta`], reusing the cached summary while the file is unchanged.
fn cached_meta(path: &Path) -> Option<Meta> {
    let stat = fs::metadata(path).ok()?;
    let modified: DateTime<Utc> = stat.modified().ok()?.into();
    let cache = summary_path(path);
    if let Some(summary) = fs::read(&cache)
        .ok()
        .and_then(|b| serde_json::from_slice::<Summary>(&b).ok())
        .filter(|s| s.len == stat.len() && s.modified == modified)
    {
        return Some(Meta { path: path.into(), ..summary.meta });
    }
    let mut meta = read_meta(path)?;
    meta.updated = modified;
    let summary = Summary { len: stat.len(), modified, meta };
    if let Ok(bytes) = serde_json::to_vec(&summary) {
        let _ = write_private(&cache, &bytes);
    }
    Some(summary.meta)
}

/// Header, title, first prompt, turn count and latest prefs; `None` if the
/// file is unreadable. `updated` is the creation time until the caller sets it.
fn read_meta(path: &Path) -> Option<Meta> {
    let mut lines = BufReader::new(File::open(path).ok()?).lines();
    let header: Header = serde_json::from_str(&lines.next()?.ok()?).ok()?;
    let mut custom = None;
    let mut first: Option<(String, String)> = None;
    let mut image_only = false;
    let mut turns = 0;
    let mut prefs = None;
    for line in lines.map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("title") => {
                if let Some(title) = value
                    .get("title")
                    .and_then(|t| t.as_str())
                    .filter(|t| !t.trim().is_empty())
                {
                    custom = Some(title.to_string());
                }
            }
            Some("prefs") => prefs = serde_json::from_value(value).ok().or(prefs),
            Some("user") => {
                turns += 1;
                if first.is_some() {
                    continue;
                }
                let text = value.get("text").and_then(|t| t.as_str()).unwrap_or("");
                let trimmed = text.trim();
                if !trimmed.is_empty() && !is_slash_only(trimmed) {
                    first = Some((title_of(text), preview_of(text)));
                } else if value["images"].as_array().is_some_and(|i| !i.is_empty()) {
                    image_only = true;
                }
            }
            _ => {}
        }
    }
    let (derived, preview) = first.unzip();
    // An image-only first prompt names the session even if text follows.
    let derived = if image_only { Some(IMAGE_TITLE.to_string()) } else { derived };
    let preview = preview.unwrap_or_default();
    Some(Meta {
        id: header.id,
        created: header.created,
        updated: header.created,
        cwd: header.cwd,
        model: header.model,
        title: custom.or(derived).unwrap_or_else(|| UNTITLED.into()),
        preview,
        turns,
        prefs,
        path: path.into(),
    })
}

/// A lone slash command (`/compact`), which says nothing about the session.
fn is_slash_only(trimmed: &str) -> bool {
    trimmed.starts_with('/') && !trimmed.contains('\n')
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

/// A `title`, `prefs` or `permissions` metadata line (not a transcript item).
fn is_meta_line(line: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(line)
        .ok()
        .is_some_and(|value| matches!(value["type"].as_str(), Some("title" | "prefs" | "permissions")))
}

/// The state of the last `permissions` line.
fn latest_permissions(data: &[u8]) -> Option<serde_json::Value> {
    data.rsplit(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find(|v| v["type"] == "permissions")
        .map(|mut v| v["state"].take())
}

/// The first non-empty line, at most `max` characters.
fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    clip(line, max)
}

fn clip(line: &str, max: usize) -> String {
    if line.chars().count() > max {
        let cut: String = line.chars().take(max - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        line.to_string()
    }
}

/// The title a session gets from its first prompt.
pub fn title_of(text: &str) -> String {
    first_line(text, TITLE_MAX)
}

/// The whole prompt on one line, for searching and previews.
fn preview_of(text: &str) -> String {
    clip(&text.split_whitespace().collect::<Vec<_>>().join(" "), PREVIEW_MAX)
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

/// Print a short transcript summary for `nyq session show` (with `json`,
/// the session's metadata and every stored item).
pub fn print_summary(dir: &Path, which: &str, cwd: &Path, json: bool) -> Result<()> {
    let meta = find(dir, which, cwd)?;
    let items = read_stored_items(&meta.path)?;
    if json {
        let mut value = meta.to_json();
        let count = |f: fn(&Item) -> bool| items.iter().filter(|i| f(i)).count();
        value["stats"] = serde_json::json!({
            "items": items.len(),
            "user": count(|i| matches!(i, Item::User { .. })),
            "assistant": count(|i| matches!(i, Item::Assistant { text } if !text.trim().is_empty())),
            "tool_calls": count(|i| matches!(i, Item::ToolCall { .. })),
        });
        value["items"] = serde_json::to_value(&items)?;
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", format_summary(&meta, &items));
    }
    Ok(())
}

impl Meta {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "title": self.title,
            "created": self.created.to_rfc3339(),
            "updated": self.updated.to_rfc3339(),
            "cwd": self.cwd,
            "model": self.model,
            "preview": self.preview,
            "turns": self.turns,
            "prefs": self.prefs,
        })
    }
}

/// Delete one saved session. Does not open it for append.
pub fn delete(dir: &Path, which: &str, cwd: &Path) -> Result<Meta> {
    let meta = find(dir, which, cwd)?;
    fs::remove_file(&meta.path).with_context(|| format!("deleting {}", meta.path.display()))?;
    let _ = fs::remove_file(lock_path(&meta.path));
    let _ = fs::remove_file(summary_path(&meta.path));
    // Its named subagents go with it.
    let _ = fs::remove_dir_all(crate::subagents::store_dir(&meta.path));
    if let Some(home) = dir.parent() {
        crate::tools::artifacts::remove_session(home, &meta.id);
        crate::usage::remove_session(home, &meta.id);
        let _ = fs::remove_file(home.join("tapes").join(format!("{}.cast", meta.id)));
    }
    Ok(meta)
}

/// Whether two working directories are the same place. Canonical paths match
/// through symlinks; Windows and macOS also ignore ASCII case.
pub fn same_cwd(saved: &Path, current: &Path) -> bool {
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
    let mut body = String::new();
    for item in items {
        let line = match item {
            Item::User { text, images } => {
                users += 1;
                body.push_str(&format!("\n[turn {users}]\n"));
                let images = match images.len() {
                    0 => String::new(),
                    1 => " [1 image]".into(),
                    n => format!(" [{n} images]"),
                };
                format!("user: {}{images}", preview_line(text))
            }
            Item::Assistant { text } if !text.trim().is_empty() => {
                assistants += 1;
                format!("assistant: {}", preview_line(text))
            }
            Item::ToolCall { name, .. } => {
                tools += 1;
                format!("tool: {name}")
            }
            Item::Compaction { summary } => format!("summary: {}", preview_line(summary)),
            _ => continue,
        };
        body.push_str(&line);
        body.push('\n');
    }
    let model = match &meta.prefs {
        Some(p) => format!(
            "{} · {}{}{}",
            p.provider,
            p.model,
            p.effort.as_deref().map(|e| format!(" · effort {e}")).unwrap_or_default(),
            if p.fast { " · fast" } else { "" }
        ),
        None => meta.model.clone(),
    };
    let mut out = format!(
        "{}\ntitle: {}\ncreated: {}\nupdated: {}\ndirectory: {}\nmodel: {}\nhistory: {} items · {} turns · {} assistant · {} tool calls\n",
        meta.id,
        meta.title,
        meta.created.to_rfc3339(),
        meta.updated.to_rfc3339(),
        meta.cwd.display(),
        model,
        items.len(),
        users,
        assistants,
        tools,
    );
    if body.is_empty() {
        out.push_str("\n(no messages yet)\n");
    } else {
        out.push_str(&body);
    }
    out
}

fn preview_line(text: &str) -> String {
    first_line(text, 120)
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
    let permissions = latest_permissions(&data[..good]);
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
        pending_prefs: None,
        prefs: meta.prefs.clone(),
        file: Some(file),
        lock: Some(lock),
        permissions,
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
        if n > 0 && !blank && !is_meta_line(line) {
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

/// `nyq doctor`: how many sessions are saved, and each one that can't be
/// resumed as is, with the command that repairs it.
pub fn health(dir: &Path) -> (usize, Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, Vec::new());
    };
    let mut count = 0;
    let mut problems = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        count += 1;
        let id = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let Ok(data) = fs::read(&path) else {
            problems.push(format!("{id}: unreadable"));
            continue;
        };
        let first = data.split(|b| *b == b'\n').next().unwrap_or_default();
        let header = serde_json::from_slice::<serde_json::Value>(first).ok();
        let problem = if !header.as_ref().is_some_and(|v| v["type"] == "session")
            || serde_json::from_slice::<Header>(first).is_err()
        {
            Some("missing or invalid header; run `nyq session recover ID`")
        } else if read_items(&data, &path).is_err() {
            Some("corrupt lines; run `nyq session recover ID`")
        } else if header.is_some_and(|v| v.get("version").is_none()) {
            Some("older format; run `nyq session migrate ID`")
        } else {
            None
        };
        if let Some(p) = problem {
            problems.push(format!("{id}: {}", p.replace("ID", &id)));
        } else if data.len() as u64 > OVERSIZED_BYTES {
            problems.push(format!(
                "{id}: over {} MB and slow to load; /compact it or delete it",
                OVERSIZED_BYTES / (1024 * 1024)
            ));
        }
    }
    (count, problems)
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
            std::thread::sleep(std::time::Duration::from_millis(20));
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
        assert_eq!(metas[0].title, UNTITLED);
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
    fn health_names_the_repair_for_each_broken_session() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert_eq!(health(&dir.join("missing")), (0, Vec::new()));
        let mut good = Session::create(dir, Path::new("/w"), "m").unwrap();
        good.append(&user("fine")).unwrap();
        good.flush().unwrap();
        fs::write(dir.join("headless.jsonl"), b"{broken}\n").unwrap();
        let mut corrupt = fs::read(&good.path).unwrap();
        corrupt.extend_from_slice(b"{bad}\n");
        corrupt.extend_from_slice(serde_json::to_string(&user("later")).unwrap().as_bytes());
        corrupt.push(b'\n');
        fs::write(dir.join("corrupt.jsonl"), corrupt).unwrap();
        let (count, mut problems) = health(dir);
        problems.sort();
        assert_eq!(count, 3);
        assert_eq!(
            problems,
            [
                "corrupt: corrupt lines; run `nyq session recover corrupt`",
                "headless: missing or invalid header; run `nyq session recover headless`",
            ]
        );
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
        let tape = tmp.path().join("tapes").join(format!("{id}.cast"));
        fs::create_dir(tmp.path().join("tapes")).unwrap();
        fs::write(&tape, "{}\n").unwrap();
        let usage = sessions.join(format!("{id}.usage"));
        fs::write(&usage, "").unwrap();
        drop(s);
        delete(&sessions, &id, Path::new("/w")).unwrap();
        assert!(!tmp.path().join("artifacts").join(&id).exists());
        assert!(!tape.exists() && !usage.exists());
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

    fn prefs(model: &str, effort: Option<&str>, fast: bool) -> Prefs {
        Prefs {
            provider: "codex".into(),
            model: model.into(),
            effort: effort.map(Into::into),
            fast,
        }
    }

    fn prefs_lines(path: &Path) -> usize {
        fs::read_to_string(path).unwrap().matches("\"type\":\"prefs\"").count()
    }

    #[test]
    fn prefs_are_saved_on_change_and_restored() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut s = Session::create(dir, Path::new("/w"), "m").unwrap();
        // Before the first item nothing is written; the latest prefs go with it.
        s.set_prefs(&prefs("a", None, false)).unwrap();
        s.set_prefs(&prefs("b", Some("high"), false)).unwrap();
        assert!(!s.path.exists());
        s.append(&user("hi")).unwrap();
        assert_eq!(prefs_lines(&s.path), 1);
        s.set_prefs(&prefs("b", Some("high"), false)).unwrap();
        assert_eq!(prefs_lines(&s.path), 1, "unchanged prefs are not written again");
        s.set_prefs(&prefs("b", Some("low"), true)).unwrap();
        assert_eq!(prefs_lines(&s.path), 2);
        assert_eq!(list(dir).unwrap()[0].prefs, Some(prefs("b", Some("low"), true)));
        let id = s.id.clone();
        drop(s);

        let (mut s, meta, items) = load(dir, &id, Path::new("/w")).unwrap();
        assert_eq!(meta.prefs, Some(prefs("b", Some("low"), true)));
        assert_eq!(items, vec![user("hi")], "prefs lines are not transcript items");
        s.set_prefs(&prefs("b", Some("low"), true)).unwrap();
        assert_eq!(prefs_lines(&s.path), 2);
        // /reset keeps the settings.
        s.clear_transcript().unwrap();
        assert_eq!(prefs_lines(&s.path), 1);
        drop(s);
        assert_eq!(find(dir, &id, Path::new("/w")).unwrap().prefs, Some(prefs("b", Some("low"), true)));
        assert!(!migrate(dir, &id, Path::new("/w")).unwrap());
    }

    #[test]
    fn recovery_keeps_prefs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Session::create(tmp.path(), Path::new("/w"), "m").unwrap();
        s.set_prefs(&prefs("a", None, true)).unwrap();
        s.append(&user("hi")).unwrap();
        let (id, kept, skipped) = recover(tmp.path(), &s.id, Path::new("/w")).unwrap();
        assert_eq!((kept, skipped), (1, 0));
        assert_eq!(find(tmp.path(), &id, Path::new("/w")).unwrap().prefs, Some(prefs("a", None, true)));
    }

    #[test]
    fn titles_skip_slash_commands_and_name_image_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let title = |items: Vec<Item>| {
            let mut s = Session::create(dir, Path::new("/w"), "m").unwrap();
            for item in &items {
                s.append(item).unwrap();
            }
            let meta = read_meta(&s.path).unwrap();
            fs::remove_file(&s.path).unwrap();
            (meta.title, meta.preview, meta.turns)
        };
        let image = Item::User {
            text: "  ".into(),
            images: vec![crate::types::Image {
                media_type: "image/png".into(),
                data: String::new(),
            }],
        };
        assert_eq!(
            title(vec![user("/compact"), user("\n  fix   the\nparser  ")]),
            ("fix   the".into(), "fix the parser".into(), 2)
        );
        assert_eq!(title(vec![image, user("what is this")]).0, IMAGE_TITLE);
        assert_eq!(title(vec![user("   ")]).0, UNTITLED);
        assert_eq!(title(vec![Item::Assistant { text: "hi".into() }]).0, UNTITLED);
        // A blank /rename title does not hide the derived one.
        let mut s = Session::create(dir, Path::new("/w"), "m").unwrap();
        s.append(&user("real")).unwrap();
        s.set_title(" ").unwrap();
        assert_eq!(read_meta(&s.path).unwrap().title, "real");
    }

    #[test]
    fn listing_follows_last_activity_and_reports_unreadable_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mut old = Session::create(dir, Path::new("/w"), "m").unwrap();
        old.append(&user("old")).unwrap();
        let mut new = Session::create(dir, Path::new("/w"), "m").unwrap();
        new.append(&user("new")).unwrap();
        // The older session was active more recently.
        let now = std::time::SystemTime::now();
        let at = |s: &Session, secs: u64| {
            let f = OpenOptions::new().append(true).open(&s.path).unwrap();
            f.set_modified(now - std::time::Duration::from_secs(secs)).unwrap();
        };
        at(&new, 600);
        at(&old, 60);
        let titles = |dir: &Path| list(dir).unwrap().into_iter().map(|m| m.title).collect::<Vec<_>>();
        assert_eq!(titles(dir), ["old", "new"]);
        assert_eq!(find(dir, "last", Path::new("/w")).unwrap().title, "old");
        let summary = summary_path(&old.path);
        assert!(summary.exists());
        // A cached summary is reused while the file is unchanged...
        let cached = fs::read_to_string(&summary).unwrap();
        fs::write(&summary, cached.replace("\"title\":\"old\"", "\"title\":\"cached\"")).unwrap();
        assert_eq!(titles(dir), ["cached", "new"]);
        // ...and refreshed once it changes.
        old.append(&user("more")).unwrap();
        at(&old, 30);
        let metas = list(dir).unwrap();
        assert_eq!((metas[0].title.as_str(), metas[0].turns), ("old", 2));

        fs::write(dir.join("broken.jsonl"), "not json\n").unwrap();
        let (all, skipped) = list_report(dir).unwrap();
        assert_eq!((all.len(), skipped, ), (2, vec![dir.join("broken.jsonl")]));
        assert_eq!(skipped_warning(0), None);
        assert_eq!(
            skipped_warning(2).unwrap(),
            "skipped 2 unreadable saved sessions (see nyq doctor)"
        );
        let id = old.id.clone();
        drop(old);
        delete(dir, &id, Path::new("/w")).unwrap();
        assert!(!summary.exists());
    }

    #[test]
    fn show_prints_every_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Session::create(tmp.path(), Path::new("/w"), "m").unwrap();
        s.set_prefs(&prefs("gpt", Some("high"), true)).unwrap();
        for i in 1..=12 {
            s.append(&user(&format!("question {i}"))).unwrap();
            s.append(&Item::Assistant { text: format!("answer {i}") }).unwrap();
        }
        s.append(&call("c1")).unwrap();
        let meta = find(tmp.path(), &s.id, Path::new("/w")).unwrap();
        let items = read_stored_items(&meta.path).unwrap();
        let out = format_summary(&meta, &items);
        assert!(out.contains("[turn 1]\nuser: question 1\nassistant: answer 1\n"), "{out}");
        assert!(out.contains("[turn 12]\nuser: question 12\nassistant: answer 12\ntool: bash\n"));
        assert!(out.contains("updated: "));
        assert!(out.contains("model: codex · gpt · effort high · fast"));
        assert!(out.contains("history: 25 items · 12 turns · 12 assistant · 1 tool calls"));
    }
}
