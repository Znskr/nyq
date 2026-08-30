//! Durable, append-only profile usage ledger.
//!
//! Mirrors `zig-reference/src/core/session/profile_usage_store.zig`.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::core::mcp::json_value::{parse_json, Value};
use crate::core::session::generation_fact_codec;
use crate::core::session::usage_report::{
    self, Completeness, GenerationFact, Incident, PendingMarker, ProfileEvent,
};
use crate::core::shared::{io as io_mod, profile_paths};

const USAGE_LOCK_FILE: &str = "usage.lock";
const LOCK_DEADLINE_MS: u64 = 2_000;
const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_RECORDS: usize = 200_000;
const COMPACTION_THRESHOLD_BYTES: usize = 8 * 1024 * 1024;
const RETENTION_MS: i64 = 35 * 24 * 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppendOutcome {
    Appended,
    Duplicate,
    Conflict,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Loaded {
    pub coverage_started_at_ms: Option<i64>,
    pub facts: Vec<GenerationFact>,
    pub pending: Vec<PendingMarker>,
    pub incidents: Vec<Incident>,
    pub record_count: usize,
}

impl Loaded {
    pub fn deinit(&mut self) {
        self.facts.clear();
        self.pending.clear();
        self.incidents.clear();
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    DurablePathUnsafe,
    PrivateStatePermissionsUnsupported,
    DurableLayoutFailed,
    UsageLockBusy,
    UsageLockUnsupported,
    UsageRecordTooLarge,
    UsageCapacityExceeded,
    UsageReadFailed,
    UsageWriteFailed,
    UsageCommitIndeterminate,
    UsageStoreIncomplete,
    InvalidUsageStore,
    InvalidEvent,
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Store {
    pub home_path: PathBuf,
    pub durable_home: Option<io_mod::VerifiedDir>,
}

impl Store {
    #[allow(non_snake_case)]
    pub fn initFromHome(home_path: impl AsRef<Path>) -> Result<Store> {
        let home_path = home_path.as_ref().to_path_buf();
        let durable_home = open_existing_durable_home(&home_path)?;
        Ok(Store {
            home_path,
            durable_home,
        })
    }

    pub fn deinit(&mut self) {
        self.durable_home = None;
    }

    fn append_fact(&mut self, fact: &GenerationFact) -> Result<AppendOutcome> {
        self.appendEvent(&ProfileEvent::Generation(fact.clone()))
    }

    #[allow(non_snake_case)]
    pub fn appendEvent(&mut self, event: &ProfileEvent) -> Result<AppendOutcome> {
        validate_event(event)?;
        let event_line = serialize_event(event)?;
        if event_line.len() > MAX_RECORD_BYTES {
            return Err(Error::UsageRecordTooLarge);
        }
        self.ensure_writable()?;
        let lock = io_mod::acquire_timed_advisory_lock(
            self.durable_home.as_ref().unwrap(),
            USAGE_LOCK_FILE,
            LOCK_DEADLINE_MS,
        )
        .map_err(map_lock_error)?;

        let path = self.usage_path();
        reject_unsafe_leaf_if_present(&path)?;
        let mut bytes = match fs::read(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(Error::Io(error)),
        };
        if bytes.len() > MAX_FILE_BYTES {
            return Err(Error::UsageCapacityExceeded);
        }
        let boundary = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let incomplete = boundary != bytes.len();
        let loaded = load_bytes(&bytes[..boundary])?;
        let decision = classify_event(&loaded, event);
        let now_ms = io_mod::milli_timestamp().max(0);
        let mut appended = Vec::new();
        if loaded.coverage_started_at_ms.is_none() {
            write_coverage(&mut appended, now_ms)?;
        }
        if incomplete {
            write_incident(
                &mut appended,
                Incident {
                    occurred_at_ms: now_ms,
                    completeness: Completeness::Incomplete,
                },
            )?;
        }
        if decision.1 {
            appended.extend_from_slice(&event_line);
        }
        if appended.is_empty() {
            drop(lock);
            return Ok(decision.0);
        }

        let additions = usize::from(loaded.coverage_started_at_ms.is_none())
            + usize::from(incomplete)
            + usize::from(decision.1);
        let next_count = loaded
            .record_count
            .checked_add(additions)
            .ok_or(Error::UsageCapacityExceeded)?;
        if next_count > MAX_RECORDS {
            return Err(Error::UsageCapacityExceeded);
        }
        bytes.truncate(boundary);
        bytes.extend_from_slice(&appended);
        if bytes.len() > MAX_FILE_BYTES {
            if has_expired_records(&loaded, now_ms) {
                bytes = retained_records(&loaded, now_ms)?;
                bytes.extend_from_slice(&appended);
            }
            if bytes.len() > MAX_FILE_BYTES {
                return Err(Error::UsageCapacityExceeded);
            }
        }
        replace_usage(self.durable_home.as_ref().unwrap(), &bytes)?;
        if bytes.len() > COMPACTION_THRESHOLD_BYTES
            && (has_expired_records(&loaded, now_ms)
                || event_timestamp(event) < retention_cutoff(now_ms))
        {
            let compacted = retained_records(&load_bytes(&bytes)?, now_ms)?;
            replace_usage(self.durable_home.as_ref().unwrap(), &compacted)?;
        }
        drop(lock);
        Ok(decision.0)
    }

    pub fn load(&mut self) -> Result<Loaded> {
        if self.durable_home.is_none() {
            self.durable_home = open_existing_durable_home(&self.home_path)?;
        }
        let Some(dir) = self.durable_home.as_ref() else {
            return Ok(empty_loaded());
        };
        validate_private_dir(&dir.path)?;
        let lock_path = dir.path.join(USAGE_LOCK_FILE);
        let lock = if lock_path.exists() {
            Some(
                io_mod::acquire_timed_advisory_lock(dir, USAGE_LOCK_FILE, LOCK_DEADLINE_MS)
                    .map_err(map_lock_error)?,
            )
        } else {
            None
        };
        let path = self.usage_path();
        reject_unsafe_leaf_if_present(&path)?;
        let bytes = match fs::read(path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(empty_loaded()),
            Err(error) => return Err(Error::Io(error)),
        };
        if bytes.len() > MAX_FILE_BYTES {
            return Err(Error::UsageCapacityExceeded);
        }
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(Error::UsageStoreIncomplete);
        }
        let loaded = load_bytes(&bytes)?;
        drop(lock);
        Ok(loaded)
    }

    fn usage_path(&self) -> PathBuf {
        self.durable_home
            .as_ref()
            .map(|d| d.path.join(profile_paths::usage_file_name))
            .unwrap_or_else(|| {
                self.home_path
                    .join(profile_paths::root_dir_name)
                    .join(profile_paths::usage_file_name)
            })
    }

    fn ensure_writable(&mut self) -> Result<()> {
        if self.durable_home.is_none() {
            let path = self.home_path.join(profile_paths::root_dir_name);
            fs::create_dir(&path)
                .or_else(|e| {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })
                .map_err(|_| Error::DurableLayoutFailed)?;
            set_mode(&path, 0o700).map_err(|_| Error::PrivateStatePermissionsUnsupported)?;
            self.durable_home = Some(io_mod::VerifiedDir {
                dir: File::open(&path)?,
                path,
            });
        }
        let path = &self.durable_home.as_ref().unwrap().path;
        set_mode(path, 0o700).map_err(|_| Error::PrivateStatePermissionsUnsupported)?;
        validate_private_dir(path)
    }
}

fn empty_loaded() -> Loaded {
    Loaded {
        coverage_started_at_ms: None,
        facts: Vec::new(),
        pending: Vec::new(),
        incidents: Vec::new(),
        record_count: 0,
    }
}

fn map_lock_error(error: io_mod::Error) -> Error {
    match error {
        io_mod::Error::LockBusy => Error::UsageLockBusy,
        io_mod::Error::LockUnsupported => Error::UsageLockUnsupported,
        io_mod::Error::DurablePathUnsafe => Error::DurablePathUnsafe,
        io_mod::Error::PrivateStatePermissionsUnsupported => {
            Error::PrivateStatePermissionsUnsupported
        }
        io_mod::Error::Io(error) => Error::Io(error),
        _ => Error::UsageWriteFailed,
    }
}

fn open_existing_durable_home(home: &Path) -> Result<Option<io_mod::VerifiedDir>> {
    if !home.is_absolute() {
        return Err(Error::DurableLayoutFailed);
    }
    let path = home.join(profile_paths::root_dir_name);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            Err(Error::DurablePathUnsafe)
        }
        Ok(_) => Ok(Some(io_mod::VerifiedDir {
            dir: File::open(&path)?,
            path,
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Io(error)),
    }
}

fn validate_private_dir(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(Error::DurablePathUnsafe);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o777 != 0o700 {
            return Err(Error::PrivateStatePermissionsUnsupported);
        }
    }
    Ok(())
}

fn reject_unsafe_leaf_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Err(Error::DurablePathUnsafe)
        }
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() != 1 {
                    return Err(Error::DurablePathUnsafe);
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

fn replace_usage(dir: &io_mod::VerifiedDir, bytes: &[u8]) -> Result<()> {
    io_mod::durable_replace_verified(dir, profile_paths::usage_file_name, bytes).map_err(|error| {
        match error {
            io_mod::Error::DurableReplacePostRenameFailed => Error::UsageCommitIndeterminate,
            io_mod::Error::DurableReplacePreRenameFailed => Error::UsageWriteFailed,
            io_mod::Error::DurablePathUnsafe => Error::DurablePathUnsafe,
            other => map_lock_error(other),
        }
    })
}

fn load_bytes(bytes: &[u8]) -> Result<Loaded> {
    if bytes.is_empty() {
        return Ok(empty_loaded());
    }
    if !bytes.ends_with(b"\n") {
        return Err(Error::UsageStoreIncomplete);
    }
    let mut loaded = empty_loaded();
    let mut facts: HashMap<Vec<u8>, (usize, Option<usize>)> = HashMap::new();
    let mut pending: HashMap<Vec<u8>, (usize, Option<usize>)> = HashMap::new();
    for line in bytes.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        loaded.record_count += 1;
        if loaded.record_count > MAX_RECORDS || line.len() > MAX_RECORD_BYTES {
            return Err(Error::UsageCapacityExceeded);
        }
        match parse_record(line)? {
            ParsedRecord::Coverage(time) => match loaded.coverage_started_at_ms {
                Some(existing) if existing != time => return Err(Error::InvalidUsageStore),
                None => loaded.coverage_started_at_ms = Some(time),
                _ => {}
            },
            ParsedRecord::Generation(fact) => {
                if loaded.coverage_started_at_ms.is_none() {
                    return Err(Error::InvalidUsageStore);
                }
                if let Some((first, second)) = facts.get_mut(&fact.id) {
                    if loaded.facts[*first] == fact
                        || second.is_some_and(|i| loaded.facts[i] == fact)
                    {
                        continue;
                    }
                    if second.is_none() {
                        loaded.facts.push(fact);
                        *second = Some(loaded.facts.len() - 1);
                    }
                } else {
                    let id = fact.id.clone();
                    loaded.facts.push(fact);
                    facts.insert(id, (loaded.facts.len() - 1, None));
                }
            }
            ParsedRecord::Pending(marker) => {
                if loaded.coverage_started_at_ms.is_none() {
                    return Err(Error::InvalidUsageStore);
                }
                if let Some((first, second)) = pending.get_mut(&marker.id) {
                    if loaded.pending[*first] == marker
                        || second.is_some_and(|i| loaded.pending[i] == marker)
                    {
                        continue;
                    }
                    if second.is_none() {
                        let when = marker.observed_at_ms;
                        loaded.pending.push(marker);
                        *second = Some(loaded.pending.len() - 1);
                        loaded.incidents.push(Incident {
                            occurred_at_ms: when,
                            completeness: Completeness::Incomplete,
                        });
                    }
                } else {
                    let id = marker.id.clone();
                    loaded.pending.push(marker);
                    pending.insert(id, (loaded.pending.len() - 1, None));
                }
            }
            ParsedRecord::Incident(incident) => loaded.incidents.push(incident),
        }
    }
    Ok(loaded)
}

enum ParsedRecord {
    Coverage(i64),
    Generation(GenerationFact),
    Pending(PendingMarker),
    Incident(Incident),
}

fn parse_record(line: &[u8]) -> Result<ParsedRecord> {
    let text = std::str::from_utf8(line).map_err(|_| Error::InvalidUsageStore)?;
    let value = parse_json(text).map_err(|_| Error::InvalidUsageStore)?;
    let object = value.as_object().ok_or(Error::InvalidUsageStore)?;
    if parse_u64(object.get("schema_version"))? != 1 {
        return Err(Error::InvalidUsageStore);
    }
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(Error::InvalidUsageStore)?;
    match kind {
        "coverage" if object.len() == 3 => Ok(ParsedRecord::Coverage(parse_i64(
            object.get("started_at_ms"),
        )?)),
        "generation" if object.len() == 3 => {
            let fact =
                generation_fact_codec::parse(object.get("fact").ok_or(Error::InvalidUsageStore)?)
                    .map_err(|_| Error::InvalidUsageStore)?;
            Ok(ParsedRecord::Generation(fact))
        }
        "pending" if object.len() == 4 => {
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .ok_or(Error::InvalidUsageStore)?
                .as_bytes()
                .to_vec();
            let marker = PendingMarker {
                id,
                observed_at_ms: parse_i64(object.get("observed_at_ms"))?,
            };
            usage_report::validate_pending_marker(&marker).map_err(|_| Error::InvalidUsageStore)?;
            Ok(ParsedRecord::Pending(marker))
        }
        "incident" if object.len() == 4 => {
            let completeness = match object.get("completeness").and_then(Value::as_str) {
                Some("pending") => Completeness::Pending,
                Some("incomplete") => Completeness::Incomplete,
                _ => return Err(Error::InvalidUsageStore),
            };
            let incident = Incident {
                occurred_at_ms: parse_i64(object.get("occurred_at_ms"))?,
                completeness,
            };
            usage_report::validate_incident(incident).map_err(|_| Error::InvalidUsageStore)?;
            Ok(ParsedRecord::Incident(incident))
        }
        _ => Err(Error::InvalidUsageStore),
    }
}

fn parse_u64(value: Option<&Value>) -> Result<u64> {
    match value.ok_or(Error::InvalidUsageStore)? {
        Value::Integer(n) if *n >= 0 => Ok(*n as u64),
        Value::NumberString(s) => s.parse().map_err(|_| Error::InvalidUsageStore),
        _ => Err(Error::InvalidUsageStore),
    }
}
fn parse_i64(value: Option<&Value>) -> Result<i64> {
    i64::try_from(parse_u64(value)?).map_err(|_| Error::InvalidUsageStore)
}

fn validate_event(event: &ProfileEvent) -> Result<()> {
    match event {
        ProfileEvent::Generation(fact) => {
            usage_report::validate_fact(fact).map_err(|_| Error::InvalidEvent)
        }
        ProfileEvent::Pending(marker) => {
            usage_report::validate_pending_marker(marker).map_err(|_| Error::InvalidEvent)
        }
        ProfileEvent::Incident(incident) => {
            usage_report::validate_incident(*incident).map_err(|_| Error::InvalidEvent)
        }
    }
}

fn classify_event(loaded: &Loaded, event: &ProfileEvent) -> (AppendOutcome, bool) {
    match event {
        ProfileEvent::Generation(fact) => {
            classify_variants(loaded.facts.iter().filter(|v| v.id == fact.id), fact)
        }
        ProfileEvent::Pending(marker) => {
            classify_variants(loaded.pending.iter().filter(|v| v.id == marker.id), marker)
        }
        ProfileEvent::Incident(incident) => {
            if loaded.incidents.contains(incident) {
                (AppendOutcome::Duplicate, false)
            } else {
                (AppendOutcome::Appended, true)
            }
        }
    }
}
fn classify_variants<'a, T: PartialEq + 'a>(
    values: impl Iterator<Item = &'a T>,
    candidate: &T,
) -> (AppendOutcome, bool) {
    let mut variants = 0;
    for existing in values {
        if existing == candidate {
            return (AppendOutcome::Duplicate, false);
        }
        variants += 1;
    }
    if variants == 0 {
        (AppendOutcome::Appended, true)
    } else {
        (AppendOutcome::Conflict, variants < 2)
    }
}

fn serialize_event(event: &ProfileEvent) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match event {
        ProfileEvent::Generation(fact) => write_generation(&mut out, fact)?,
        ProfileEvent::Pending(marker) => write_pending(&mut out, marker)?,
        ProfileEvent::Incident(incident) => write_incident(&mut out, *incident)?,
    }
    Ok(out)
}
fn write_coverage(out: &mut Vec<u8>, time: i64) -> Result<()> {
    writeln!(
        out,
        "{{\"schema_version\":1,\"kind\":\"coverage\",\"started_at_ms\":{time}}}"
    )
    .map_err(Error::Io)
}
fn write_generation(out: &mut Vec<u8>, fact: &GenerationFact) -> Result<()> {
    out.extend_from_slice(b"{\"schema_version\":1,\"kind\":\"generation\",\"fact\":");
    generation_fact_codec::write(out, fact).map_err(Error::Io)?;
    out.extend_from_slice(b"}\n");
    Ok(())
}
fn write_pending(out: &mut Vec<u8>, marker: &PendingMarker) -> Result<()> {
    out.extend_from_slice(b"{\"schema_version\":1,\"kind\":\"pending\",\"id\":");
    write_json_string(out, &marker.id)?;
    writeln!(out, ",\"observed_at_ms\":{}}}", marker.observed_at_ms).map_err(Error::Io)
}
fn write_incident(out: &mut Vec<u8>, incident: Incident) -> Result<()> {
    let completeness = match incident.completeness {
        Completeness::Pending => "pending",
        Completeness::Incomplete => "incomplete",
        Completeness::Complete => "complete",
        Completeness::Legacy => "legacy",
    };
    writeln!(out, "{{\"schema_version\":1,\"kind\":\"incident\",\"occurred_at_ms\":{},\"completeness\":\"{}\"}}", incident.occurred_at_ms, completeness).map_err(Error::Io)
}
fn write_json_string(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidEvent)?;
    out.push(b'"');
    for ch in text.chars() {
        match ch {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if c <= '\u{1f}' => write!(out, "\\u{:04x}", c as u32).map_err(Error::Io)?,
            c => {
                let mut b = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
    }
    out.push(b'"');
    Ok(())
}

fn retention_cutoff(now: i64) -> i64 {
    now.checked_sub(RETENTION_MS).unwrap_or(0)
}
fn pending_resolved(loaded: &Loaded, id: &[u8]) -> bool {
    loaded.facts.iter().any(|f| f.id == id)
}
fn has_expired_records(loaded: &Loaded, now: i64) -> bool {
    let cutoff = retention_cutoff(now);
    loaded.facts.iter().any(|f| f.created_at_ms < cutoff)
        || loaded
            .pending
            .iter()
            .any(|p| p.observed_at_ms < cutoff || pending_resolved(loaded, &p.id))
        || loaded.incidents.iter().any(|i| i.occurred_at_ms < cutoff)
}
fn retained_records(loaded: &Loaded, now: i64) -> Result<Vec<u8>> {
    let cutoff = retention_cutoff(now);
    let mut out = Vec::new();
    if let Some(time) = loaded.coverage_started_at_ms {
        write_coverage(&mut out, time)?;
    }
    for fact in &loaded.facts {
        if fact.created_at_ms >= cutoff {
            write_generation(&mut out, fact)?;
        }
    }
    for marker in &loaded.pending {
        if marker.observed_at_ms >= cutoff && !pending_resolved(loaded, &marker.id) {
            write_pending(&mut out, marker)?;
        }
    }
    for incident in &loaded.incidents {
        if incident.occurred_at_ms >= cutoff {
            write_incident(&mut out, *incident)?;
        }
    }
    Ok(out)
}
fn event_timestamp(event: &ProfileEvent) -> i64 {
    match event {
        ProfileEvent::Generation(f) => f.created_at_ms,
        ProfileEvent::Pending(p) => p.observed_at_ms,
        ProfileEvent::Incident(i) => i.occurred_at_ms,
    }
}
fn checked_record_count(current: usize, additional: usize) -> Result<usize> {
    let next = current
        .checked_add(additional)
        .ok_or(Error::UsageCapacityExceeded)?;
    if next > MAX_RECORDS {
        Err(Error::UsageCapacityExceeded)
    } else {
        Ok(next)
    }
}
fn should_compact_before_append(
    next_length: usize,
    record_count: usize,
    additional: usize,
    loaded: &Loaded,
    now: i64,
) -> bool {
    (next_length > MAX_FILE_BYTES || checked_record_count(record_count, additional).is_err())
        && has_expired_records(loaded, now)
}
fn should_compact_after_append(
    next_length: usize,
    loaded: &Loaded,
    appended_at: i64,
    now: i64,
    compacted: bool,
) -> bool {
    !compacted
        && next_length > COMPACTION_THRESHOLD_BYTES
        && (has_expired_records(loaded, now) || appended_at < retention_cutoff(now))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}
#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    fn temp_home() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "nyq-profile-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&p).unwrap();
        p.canonicalize().unwrap()
    }
    fn fact(id: &[u8], input: u64) -> GenerationFact {
        GenerationFact {
            id: id.to_vec(),
            created_at_ms: 1000,
            model: b"provider/model".to_vec(),
            input_tokens: input,
            output_tokens: 2,
            cache_read_tokens: 1,
            cache_write_tokens: 0,
            reasoning_tokens: Some(1),
            billable_web_search_calls: 0,
            total_cost: 0.25,
        }
    }
    #[test]
    fn profile_usage_store_is_read_only_until_first_fact_and_dedupes_exact_replay() {
        let home = temp_home();
        let mut s = Store::initFromHome(&home).unwrap();
        assert_eq!(None, s.load().unwrap().coverage_started_at_ms);
        let f = fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 10);
        assert_eq!(AppendOutcome::Appended, s.append_fact(&f).unwrap());
        assert_eq!(AppendOutcome::Duplicate, s.append_fact(&f).unwrap());
        let l = s.load().unwrap();
        assert!(l.coverage_started_at_ms.unwrap() > 0);
        assert_eq!(1, l.facts.len());
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn profile_usage_store_discovers_profile_state_created_after_initialization() {
        let home = temp_home();
        let mut observer = Store::initFromHome(&home).unwrap();
        let mut writer = Store::initFromHome(&home).unwrap();
        let f = fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 10);
        writer.append_fact(&f).unwrap();
        assert_eq!(vec![f], observer.load().unwrap().facts);
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn profile_usage_readers_wait_for_the_writer_boundary() {
        let home = temp_home();
        let mut writer = Store::initFromHome(&home).unwrap();
        writer
            .append_fact(&fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 10))
            .unwrap();
        let dir = writer.durable_home.as_ref().unwrap();
        let held =
            io_mod::acquire_timed_advisory_lock(dir, USAGE_LOCK_FILE, LOCK_DEADLINE_MS).unwrap();
        let path = home.clone();
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s, d) = (started.clone(), done.clone());
        let thread = std::thread::spawn(move || {
            let mut reader = Store::initFromHome(path).unwrap();
            s.store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(1, reader.load().unwrap().facts.len());
            d.store(true, std::sync::atomic::Ordering::SeqCst)
        });
        while !started.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::yield_now()
        }
        for _ in 0..100 {
            std::thread::yield_now()
        }
        assert!(!done.load(std::sync::atomic::Ordering::SeqCst));
        drop(held);
        thread.join().unwrap();
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn conflicting_duplicates_are_preserved() {
        let home = temp_home();
        let mut s = Store::initFromHome(&home).unwrap();
        let a = fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 10);
        let b = fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 11);
        assert_eq!(AppendOutcome::Appended, s.append_fact(&a).unwrap());
        assert_eq!(AppendOutcome::Conflict, s.append_fact(&b).unwrap());
        assert_eq!(AppendOutcome::Duplicate, s.append_fact(&b).unwrap());
        assert_eq!(2, s.load().unwrap().facts.len());
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn incomplete_tail_is_repaired_and_recorded() {
        let home = temp_home();
        let mut s = Store::initFromHome(&home).unwrap();
        let a = fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 10);
        s.append_fact(&a).unwrap();
        OpenOptions::new()
            .append(true)
            .open(s.usage_path())
            .unwrap()
            .write_all(b"{\"schema_version\":1")
            .unwrap();
        assert!(matches!(s.load(), Err(Error::UsageStoreIncomplete)));
        let b = fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAW", 20);
        s.append_fact(&b).unwrap();
        let l = s.load().unwrap();
        assert_eq!(2, l.facts.len());
        assert_eq!(Completeness::Incomplete, l.incidents[0].completeness);
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn repaired_tail_is_recorded_when_fact_is_duplicate() {
        let home = temp_home();
        let mut s = Store::initFromHome(&home).unwrap();
        let f = fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 10);
        s.append_fact(&f).unwrap();
        OpenOptions::new()
            .append(true)
            .open(s.usage_path())
            .unwrap()
            .write_all(b"broken")
            .unwrap();
        assert_eq!(AppendOutcome::Duplicate, s.append_fact(&f).unwrap());
        let l = s.load().unwrap();
        assert_eq!(1, l.facts.len());
        assert_eq!(1, l.incidents.len());
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn repair_over_record_capacity_leaves_tail_intact() {
        let home = temp_home();
        let profile = home.join(".fx");
        fs::create_dir(&profile).unwrap();
        set_mode(&profile, 0o700).unwrap();
        let mut bytes = Vec::new();
        for _ in 0..MAX_RECORDS {
            write_coverage(&mut bytes, 1).unwrap()
        }
        bytes.extend_from_slice(b"broken");
        fs::write(profile.join(profile_paths::usage_file_name), &bytes).unwrap();
        set_mode(&profile.join(profile_paths::usage_file_name), 0o600).unwrap();
        let mut s = Store::initFromHome(&home).unwrap();
        let len = bytes.len() as u64;
        assert!(matches!(
            s.append_fact(&fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 1)),
            Err(Error::UsageCapacityExceeded)
        ));
        assert_eq!(len, fs::metadata(s.usage_path()).unwrap().len());
        assert!(matches!(s.load(), Err(Error::UsageStoreIncomplete)));
        fs::remove_dir_all(home).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn existing_profile_directory_is_repaired_to_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let home = temp_home();
        let profile = home.join(".fx");
        fs::create_dir(&profile).unwrap();
        set_mode(&profile, 0o755).unwrap();
        let mut s = Store::initFromHome(&home).unwrap();
        s.append_fact(&fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 10))
            .unwrap();
        assert_eq!(
            0o700,
            fs::metadata(profile).unwrap().permissions().mode() & 0o777
        );
        fs::remove_dir_all(home).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn reads_reject_unsafe_profile_directory_without_repairing() {
        use std::os::unix::fs::PermissionsExt;
        let home = temp_home();
        let profile = home.join(".fx");
        fs::create_dir(&profile).unwrap();
        set_mode(&profile, 0o755).unwrap();
        let mut bytes = Vec::new();
        write_coverage(&mut bytes, 1).unwrap();
        write_generation(&mut bytes, &fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 1)).unwrap();
        fs::write(profile.join(profile_paths::usage_file_name), bytes).unwrap();
        let mut s = Store::initFromHome(&home).unwrap();
        assert!(matches!(
            s.load(),
            Err(Error::PrivateStatePermissionsUnsupported)
        ));
        assert_eq!(
            0o755,
            fs::metadata(profile).unwrap().permissions().mode() & 0o777
        );
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn large_ledger_has_stable_id_indexing() {
        let home = temp_home();
        let profile = home.join(".fx");
        fs::create_dir(&profile).unwrap();
        set_mode(&profile, 0o700).unwrap();
        let mut bytes = Vec::new();
        write_coverage(&mut bytes, 1).unwrap();
        for i in 0..4096 {
            let id = format!("gen_{i:026}");
            let mut f = fact(id.as_bytes(), 1);
            f.created_at_ms = 1;
            write_generation(&mut bytes, &f).unwrap()
        }
        let ledger = profile.join(profile_paths::usage_file_name);
        fs::write(&ledger, bytes).unwrap();
        set_mode(&ledger, 0o600).unwrap();
        let mut s = Store::initFromHome(&home).unwrap();
        assert_eq!(4096, s.load().unwrap().facts.len());
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn compaction_eligibility_requires_expired_records() {
        let now = 100 * 24 * 60 * 60 * 1000;
        let mut recent = fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 1);
        recent.created_at_ms = now - 24 * 60 * 60 * 1000;
        let mut loaded = Loaded {
            coverage_started_at_ms: Some(0),
            facts: vec![recent],
            pending: vec![],
            incidents: vec![],
            record_count: 2,
        };
        assert!(!has_expired_records(&loaded, now));
        assert!(!should_compact_after_append(
            COMPACTION_THRESHOLD_BYTES + 1,
            &loaded,
            loaded.facts[0].created_at_ms,
            now,
            false
        ));
        assert!(!should_compact_before_append(
            MAX_FILE_BYTES + 1,
            2,
            1,
            &loaded,
            now
        ));
        loaded.facts[0].created_at_ms = now - 36 * 24 * 60 * 60 * 1000;
        assert!(has_expired_records(&loaded, now));
        assert!(should_compact_after_append(
            COMPACTION_THRESHOLD_BYTES + 1,
            &loaded,
            now - 1,
            now,
            false
        ));
        assert!(should_compact_before_append(
            MAX_FILE_BYTES + 1,
            2,
            1,
            &loaded,
            now
        ));
        assert!(should_compact_before_append(
            0,
            MAX_RECORDS,
            1,
            &loaded,
            now
        ));
    }
    #[test]
    fn record_capacity_is_checked_before_append() {
        assert_eq!(
            MAX_RECORDS,
            checked_record_count(MAX_RECORDS - 2, 2).unwrap()
        );
        assert!(matches!(
            checked_record_count(MAX_RECORDS, 1),
            Err(Error::UsageCapacityExceeded)
        ));
        assert!(matches!(
            checked_record_count(MAX_RECORDS - 1, 2),
            Err(Error::UsageCapacityExceeded)
        ));
    }
    #[test]
    fn concurrent_writers_serialize_without_losing_facts() {
        let home = temp_home();
        let a = home.clone();
        let b = home.clone();
        let t1 = std::thread::spawn(move || {
            let mut s = Store::initFromHome(a).unwrap();
            s.append_fact(&fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 10))
                .unwrap()
        });
        let t2 = std::thread::spawn(move || {
            let mut s = Store::initFromHome(b).unwrap();
            s.append_fact(&fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAW", 20))
                .unwrap()
        });
        assert_eq!(AppendOutcome::Appended, t1.join().unwrap());
        assert_eq!(AppendOutcome::Appended, t2.join().unwrap());
        let mut s = Store::initFromHome(&home).unwrap();
        assert_eq!(2, s.load().unwrap().facts.len());
        fs::remove_dir_all(home).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_ledger_leaf_is_refused() {
        use std::os::unix::fs::symlink;
        let home = temp_home();
        let mut s = Store::initFromHome(&home).unwrap();
        let f = fact(b"gen_01ARZ3NDEKTSV4RRFFQ69G5FAV", 10);
        s.append_fact(&f).unwrap();
        fs::remove_file(s.usage_path()).unwrap();
        fs::write(home.join("outside"), b"outside").unwrap();
        symlink("../outside", s.usage_path()).unwrap();
        assert!(matches!(s.load(), Err(Error::DurablePathUnsafe)));
        assert!(matches!(s.append_fact(&f), Err(Error::DurablePathUnsafe)));
        assert_eq!(b"outside", &fs::read(home.join("outside")).unwrap()[..]);
        fs::remove_dir_all(home).unwrap();
    }
}
