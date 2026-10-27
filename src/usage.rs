//! Append-only profile usage and period reports.
//!
//! `usage.jsonl` keeps one record per model request for every session. Each
//! saved session also gets a `<id>.usage` copy of its own records next to its
//! transcript, so resuming does not rescan the profile log. The profile log
//! drops records older than [`RETAIN_DAYS`] once it grows past [`COMPACT_AT`].
use crate::types::{Item, Usage};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// Records older than this are dropped when the log is compacted.
const RETAIN_DAYS: i64 = 35;
/// The log is compacted once it is larger than this.
const COMPACT_AT: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum Period {
    #[value(name = "24h")]
    Day,
    #[value(name = "7d")]
    Week,
    #[default]
    #[value(name = "30d")]
    Month,
}
impl Period {
    fn hours(self) -> i64 {
        match self {
            Self::Day => 24,
            Self::Week => 168,
            Self::Month => 720,
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "24h" => Ok(Self::Day),
            "7d" => Ok(Self::Week),
            "30d" => Ok(Self::Month),
            _ => anyhow::bail!("period must be 24h, 7d or 30d"),
        }
    }
}
#[derive(Serialize, Deserialize, Debug)]
pub struct Record {
    pub ts: DateTime<Utc>,
    pub session: String,
    pub provider: String,
    pub model: String,
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write: u64,
    pub cost_usd: Option<f64>,
}
fn is_zero(n: &u64) -> bool {
    *n == 0
}
impl Record {
    fn usage(&self) -> Usage {
        Usage {
            input_tokens: self.input,
            output_tokens: self.output,
            cached_tokens: self.cached,
            reasoning_tokens: self.reasoning,
            cache_write_tokens: self.cache_write,
        }
    }
}

/// `<home>/sessions/<id>.usage`, the per-session copy of its records.
fn sidecar(home: &Path, session: &str) -> Option<PathBuf> {
    let safe = !session.is_empty()
        && session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    safe.then(|| home.join("sessions").join(format!("{session}.usage")))
}

/// When the profile log started (or, after compaction, how far back it is complete).
fn since_path(home: &Path) -> PathBuf {
    home.join("usage-since")
}

fn open_append(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Append one line under an exclusive lock, rolling back a partial write.
/// Returns the file (still locked) and its length before the write.
fn locked_append(path: &Path, line: &[u8]) -> Result<(File, u64)> {
    let mut file = open_append(path)?;
    // Serialize concurrent writers and roll back a partial record on failure.
    file.lock()?;
    let before = file.metadata()?.len();
    let write = (|| -> std::io::Result<()> {
        if before > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        file.write_all(line)?;
        file.sync_data()
    })();
    if let Err(e) = write {
        let _ = file.set_len(before);
        return Err(e.into());
    }
    Ok((file, before))
}

pub fn append(
    home: &Path,
    session: &str,
    provider: &str,
    pricing_provider: &str,
    model: &str,
    u: Usage,
) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let now = Utc::now();
    let record = Record {
        ts: now,
        session: session.into(),
        provider: provider.into(),
        model: model.into(),
        input: u.input_tokens,
        cached: u.cached_tokens,
        output: u.output_tokens,
        reasoning: u.reasoning_tokens,
        cache_write: u.cache_write_tokens,
        cost_usd: crate::pricing::lookup(home, pricing_provider, model).map(|p| p.cost(u)),
    };
    let mut line = serde_json::to_vec(&record)?;
    line.push(b'\n');
    let (mut file, before) = locked_append(&home.join("usage.jsonl"), &line)?;
    if before == 0 && !since_path(home).exists() {
        let _ = crate::session::write_private(&since_path(home), now.to_rfc3339().as_bytes());
    }
    if before + line.len() as u64 > COMPACT_AT {
        if let Err(e) = compact(home, &mut file, now) {
            crate::ui::error(&format!("could not compact the usage log: {e:#}"));
        }
    }
    file.unlock()?;
    // Only saved sessions get a copy; subagents share their parent's.
    if let Some(path) = sidecar(home, session).filter(|p| p.with_extension("jsonl").exists()) {
        locked_append(&path, &line)?.0.unlock()?;
    }
    Ok(())
}

/// Drop records older than [`RETAIN_DAYS`], in place under the caller's lock
/// (so concurrent writers keep appending to the same file). Cheap when the
/// oldest record is still recent.
fn compact(home: &Path, file: &mut File, now: DateTime<Utc>) -> Result<()> {
    let cutoff = now - chrono::Duration::days(RETAIN_DAYS);
    file.seek(SeekFrom::Start(0))?;
    let mut first = String::new();
    BufReader::new(&*file).read_line(&mut first)?;
    if serde_json::from_str::<Record>(&first).is_ok_and(|r| r.ts >= cutoff) {
        return Ok(());
    }
    file.seek(SeekFrom::Start(0))?;
    let mut kept = Vec::new();
    for line in BufReader::new(&*file).split(b'\n') {
        let line = line?;
        if serde_json::from_slice::<Record>(&line).is_ok_and(|r| r.ts >= cutoff) {
            kept.extend_from_slice(&line);
            kept.push(b'\n');
        }
    }
    // Rewrite through the locked handle (Windows locks are mandatory, so a
    // second handle could not write). It appends, so cut first.
    file.set_len(0)?;
    file.write_all(&kept)?;
    file.sync_data()?;
    let since = tracking_since(home).map_or(cutoff, |s| s.max(cutoff));
    crate::session::write_private(&since_path(home), since.to_rfc3339().as_bytes())?;
    Ok(())
}

fn tracking_since(home: &Path) -> Option<DateTime<Utc>> {
    std::fs::read_to_string(since_path(home))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[derive(Default, Debug)]
pub struct Total {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub reasoning: u64,
    pub cache_write: u64,
    pub cost: f64,
    pub unknown: u64,
    pub subscription: u64,
}
impl Total {
    fn add(&mut self, r: &Record) {
        self.requests += 1;
        self.input += r.input;
        self.output += r.output;
        self.cached += r.cached;
        self.reasoning += r.reasoning;
        self.cache_write += r.cache_write;
        if r.provider == "codex" || r.provider == "grok" {
            self.subscription += 1;
        } else if let Some(c) = r.cost_usd {
            self.cost += c;
        } else {
            self.unknown += 1;
        }
    }
    fn line(&self, label: &str) -> String {
        let extra = |n: u64, what: &str| {
            if n > 0 {
                format!(" · {n} {what}")
            } else {
                String::new()
            }
        };
        format!(
            "  {label}: {} requests · input {} (cached {}{}) · output {}{} · ${:.6} estimated{}{}",
            self.requests,
            self.input,
            self.cached,
            if self.cache_write > 0 {
                format!(", cache writes {}", self.cache_write)
            } else {
                String::new()
            },
            self.output,
            if self.reasoning > 0 {
                format!(" (reasoning {})", self.reasoning)
            } else {
                String::new()
            },
            self.cost,
            extra(self.unknown, "unknown cost"),
            extra(self.subscription, "subscription"),
        )
    }
}
#[derive(Default)]
pub struct Report {
    pub providers: BTreeMap<String, Total>,
    pub models: BTreeMap<(String, String), Total>,
    pub malformed: usize,
    /// How far back the log is complete.
    pub since: Option<DateTime<Utc>>,
}
impl Report {
    fn add(&mut self, r: Record, period: Period, now: DateTime<Utc>) {
        if r.ts < now - chrono::Duration::hours(period.hours()) || r.ts > now {
            return;
        }
        self.push(r);
    }
    fn push(&mut self, r: Record) {
        self.providers
            .entry(r.provider.clone())
            .or_default()
            .add(&r);
        self.models
            .entry((r.provider.clone(), r.model.clone()))
            .or_default()
            .add(&r);
    }
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec!["Usage by provider:".into()];
        lines.extend(self.providers.iter().map(|(p, t)| t.line(p)));
        lines.push("Usage by model:".into());
        let all: u64 = self.models.values().map(|t| t.input + t.output).sum();
        lines.extend(self.models.iter().map(|((p, m), t)| {
            let share = if all == 0 {
                0.0
            } else {
                (t.input + t.output) as f64 * 100.0 / all as f64
            };
            format!("{} · {share:.0}% of tokens", t.line(&format!("{p}/{m}")))
        }));
        if self.models.is_empty() {
            lines.push("  No usage recorded in this period.".into());
        }
        if self.malformed > 0 {
            lines.push(format!(
                "  Skipped {} malformed usage records.",
                self.malformed
            ));
        }
        if let Some(since) = self.since {
            lines.push(format!(
                "  Tracking since {} (records are kept for {RETAIN_DAYS} days).",
                since.with_timezone(&chrono::Local).format("%Y-%m-%d")
            ));
        }
        lines
    }
}
pub fn read(home: &Path, period: Period) -> Result<Report> {
    let now = Utc::now();
    let mut report = read_with(&home.join("usage.jsonl"), |report, r| {
        report.add(r, period, now)
    })?;
    report.since = tracking_since(home).or(report.since);
    Ok(report)
}
/// Every record of one session, whatever its age: from its own copy when it
/// has one (which outlives the profile log's retention), else the profile log.
pub fn read_session(home: &Path, session: &str) -> Result<Report> {
    let path = sidecar(home, session)
        .filter(|p| p.exists())
        .unwrap_or_else(|| home.join("usage.jsonl"));
    let mut report = read_with(&path, |report, r| {
        if r.session == session {
            report.push(r)
        }
    })?;
    // Retention applies to the profile log, not to one session's total.
    report.since = None;
    Ok(report)
}
fn read_with(path: &Path, mut add: impl FnMut(&mut Report, Record)) -> Result<Report> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Report::default()),
        Err(e) => return Err(e).context("reading usage log"),
    };
    let mut report = Report::default();
    let mut earliest: Option<DateTime<Utc>> = None;
    for line in BufReader::new(file).lines() {
        let line = line?;
        match serde_json::from_str::<Record>(&line) {
            Ok(r) => {
                earliest = Some(earliest.map_or(r.ts, |e| e.min(r.ts)));
                add(&mut report, r);
            }
            Err(_) => report.malformed += 1,
        }
    }
    report.since = report.since.or(earliest);
    Ok(report)
}
pub fn show(home: &Path, period: Period, json: bool) -> Result<()> {
    let report = read(home, period)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report.to_json(period))?);
    } else {
        crate::ui::list(&report.lines());
    }
    Ok(())
}
/// `/usage [session|24h|7d|30d]` (also `/cost`).
pub fn slash(home: &Path, args: &[&str], session: Option<&str>) -> Result<()> {
    match args {
        ["session"] => {
            let id = session.context("this conversation has no session yet")?;
            let mut lines = vec![format!(
                "This session ({}):",
                id.chars().take(8).collect::<String>()
            )];
            lines.extend(read_session(home, id)?.lines());
            crate::ui::list(&lines);
            Ok(())
        }
        [] => show(home, Period::Month, false),
        [p] => show(
            home,
            Period::parse(p).map_err(|_| anyhow::anyhow!("usage: /usage [session|24h|7d|30d]"))?,
            false,
        ),
        _ => anyhow::bail!("usage: /usage [session|24h|7d|30d]"),
    }
}

impl Total {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "requests": self.requests,
            "input_tokens": self.input,
            "output_tokens": self.output,
            "cached_tokens": self.cached,
            "estimated_cost_usd": self.cost,
            "unknown_cost_requests": self.unknown,
            "subscription_requests": self.subscription,
        })
    }
}

impl Report {
    pub fn to_json(&self, period: Period) -> serde_json::Value {
        let with = |mut v: serde_json::Value, extra: &[(&str, &str)]| {
            for (k, val) in extra {
                v[*k] = (*val).into();
            }
            v
        };
        serde_json::json!({
            "period_hours": period.hours(),
            "providers": self.providers.iter()
                .map(|(p, t)| with(t.to_json(), &[("provider", p)]))
                .collect::<Vec<_>>(),
            "models": self.models.iter()
                .map(|((p, m), t)| with(t.to_json(), &[("provider", p), ("model", m)]))
                .collect::<Vec<_>>(),
            "malformed_records": self.malformed,
        })
    }
}

/// Totals of a session's records: its sidecar, or (for sessions saved before
/// sidecars existed) a scan of the profile log that then becomes the sidecar.
fn session_records(home: &Path, session: &str) -> Result<Vec<Record>> {
    let parse = |data: &[u8]| -> Vec<Record> {
        data.split(|b| *b == b'\n')
            .filter_map(|l| serde_json::from_slice(l).ok())
            .collect()
    };
    let path = sidecar(home, session);
    if let Some(path) = &path {
        match std::fs::read(path) {
            Ok(data) => return Ok(parse(&data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut lines = Vec::new();
    match File::open(home.join("usage.jsonl")) {
        Ok(file) => {
            for line in BufReader::new(file).lines() {
                let line = line?;
                if serde_json::from_str::<Record>(&line).is_ok_and(|r| r.session == session) {
                    lines.extend_from_slice(line.as_bytes());
                    lines.push(b'\n');
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if let Some(path) = path.filter(|p| p.with_extension("jsonl").exists()) {
        let _ = crate::session::write_private(&path, &lines);
    }
    Ok(parse(&lines))
}

/// Remove a session's usage sidecar (the profile log keeps its records).
pub fn remove_session(home: &Path, session: &str) {
    if let Some(path) = sidecar(home, session) {
        let _ = std::fs::remove_file(path);
    }
}

pub struct Stats {
    pub id: String,
    pub turns: u64,
    pub requests: u64,
    pub tools: BTreeMap<String, u64>,
    /// When this run started (not when the session was created).
    started: DateTime<Utc>,
    /// Time spent waiting on model responses in this run.
    pub api_time: Duration,
    /// Lines changed by write_file and edit_file in this run.
    pub lines_added: u64,
    pub lines_removed: u64,
}
impl Default for Stats {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            turns: 0,
            requests: 0,
            tools: BTreeMap::new(),
            started: Utc::now(),
            api_time: Duration::ZERO,
            lines_added: 0,
            lines_removed: 0,
        }
    }
}
impl Stats {
    /// Recover counters from the complete saved transcript, including compacted turns.
    pub fn restore(
        &mut self,
        session: &crate::session::Session,
        home: Option<&Path>,
    ) -> Result<Usage> {
        let started = self.started;
        *self = Self {
            started,
            ..Self::default()
        };
        if let Ok(file) = File::open(&session.path) {
            for line in BufReader::new(file).lines() {
                if let Ok(item) = serde_json::from_str::<Item>(&line?) {
                    self.item(&item);
                }
            }
        }
        let mut usage = Usage::default();
        if let Some(home) = home {
            for r in session_records(home, &session.id)? {
                self.requests += 1;
                usage.add(&r.usage());
            }
        }
        Ok(usage)
    }

    pub fn item(&mut self, item: &Item) {
        match item {
            Item::User { .. } => self.turns += 1,
            Item::ToolCall { name, .. } => *self.tools.entry(name.clone()).or_default() += 1,
            _ => {}
        }
    }

    /// Count the lines a successful file write or edit changed.
    pub fn file_change(&mut self, tool: &str, args: &serde_json::Value, display: Option<&str>) {
        if !matches!(tool, "write_file" | "edit_file") {
            return;
        }
        let Some(display) = display else { return };
        if display.starts_with("---") || display.contains("\n@@") {
            for line in display.lines() {
                if line.starts_with('+') && !line.starts_with("+++") {
                    self.lines_added += 1;
                } else if line.starts_with('-') && !line.starts_with("---") {
                    self.lines_removed += 1;
                }
            }
        } else if let Some(content) = args["content"].as_str() {
            // A new file (or one too large or binary to diff).
            self.lines_added += content.lines().count() as u64;
        }
    }

    pub fn lines(&self, u: Usage) -> Vec<String> {
        let mut lines = vec![
            format!("{} turns · {} requests", self.turns, self.requests),
            format!(
                "this run: wall {} · API {} · lines +{} -{}",
                duration((Utc::now() - self.started).to_std().unwrap_or_default()),
                duration(self.api_time),
                self.lines_added,
                self.lines_removed
            ),
            format!(
                "input {} · output {} · cached {}{}{}",
                u.input_tokens,
                u.output_tokens,
                u.cached_tokens,
                if u.reasoning_tokens > 0 {
                    format!(" · reasoning {}", u.reasoning_tokens)
                } else {
                    String::new()
                },
                if u.cache_write_tokens > 0 {
                    format!(" · cache writes {}", u.cache_write_tokens)
                } else {
                    String::new()
                }
            ),
        ];
        lines.extend(
            self.tools
                .iter()
                .map(|(name, n)| format!("  {name}: {n} tool calls")),
        );
        lines
    }

    pub fn show(&self, u: Usage) {
        crate::ui::list(&self.lines(u));
    }
}

/// `1h 2m`, `3m 4s`, `5s`.
fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_concurrent_append_recovers_truncated_tail() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            r#"{"openai":{"models":{"test":{"cost":{"input":2,"output":8,"cache_read":0.5}}}}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("usage.jsonl"), b"{truncated").unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let home = dir.path();
                scope.spawn(move || {
                    for _ in 0..5 {
                        append(
                            home,
                            "s",
                            "openai",
                            "openai",
                            "test",
                            Usage {
                                input_tokens: 10,
                                cached_tokens: 2,
                                output_tokens: 3,
                                ..Default::default()
                            },
                        )
                        .unwrap();
                    }
                });
            }
        });
        let report = read(dir.path(), Period::Day).unwrap();
        assert_eq!(report.malformed, 1);
        assert_eq!(
            read_session(dir.path(), "s").unwrap().providers["openai"].requests,
            20
        );
        assert!(read_session(dir.path(), "other").unwrap().models.is_empty());
        assert_eq!(report.providers["openai"].requests, 20);
        assert!((report.providers["openai"].cost - 0.00082).abs() < 1e-10);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join("usage.jsonl"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn restore_keeps_compacted_turns_and_tool_counts() {
        let dir = tempfile::tempdir().unwrap();
        let mut session =
            crate::session::Session::create(&dir.path().join("sessions"), dir.path(), "m").unwrap();
        session.append(&Item::user("first")).unwrap();
        session
            .append(&Item::ToolCall {
                id: "t".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            })
            .unwrap();
        session
            .append(&Item::Compaction {
                summary: "summary".into(),
            })
            .unwrap();
        session.append(&Item::user("second")).unwrap();
        append(
            dir.path(),
            &session.id,
            "codex",
            "codex",
            "m",
            Usage {
                input_tokens: 10,
                output_tokens: 2,
                cached_tokens: 3,
                ..Default::default()
            },
        )
        .unwrap();
        let mut stats = Stats::default();
        let usage = stats.restore(&session, Some(dir.path())).unwrap();
        assert_eq!(
            (stats.turns, stats.requests, stats.tools["read_file"]),
            (2, 1, 1)
        );
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.cached_tokens),
            (10, 2, 3)
        );
        assert_eq!(
            read(dir.path(), Period::Day).unwrap().providers["codex"].subscription,
            1
        );
    }
    #[test]
    fn period_boundaries_and_groups() {
        let now = DateTime::from_timestamp(2_000_000_000, 0).unwrap();
        for (period, hours) in [(Period::Day, 24), (Period::Week, 168), (Period::Month, 720)] {
            let mut report = Report::default();
            for (age, p, m, c) in [
                (hours, "openai", "a", Some(1.0)),
                (0, "openai", "b", None),
                (1, "codex", "a", None),
                (hours + 1, "openai", "a", Some(10.0)),
                (-1, "openai", "a", None),
            ] {
                report.add(
                    Record {
                        ts: now - chrono::Duration::hours(age),
                        session: "s".into(),
                        provider: p.into(),
                        model: m.into(),
                        input: 10,
                        cached: 2,
                        output: 3,
                        reasoning: 1,
                        cache_write: 0,
                        cost_usd: c,
                    },
                    period,
                    now,
                );
            }
            assert_eq!(report.models.len(), 3);
            let lines = report.lines();
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("openai/b") && l.ends_with("· 33% of tokens")),
                "{lines:?}"
            );
            let t = &report.providers["openai"];
            assert_eq!(
                (
                    t.requests,
                    t.input,
                    t.cached,
                    t.output,
                    t.reasoning,
                    t.unknown
                ),
                (2, 20, 4, 6, 2, 1)
            );
            assert_eq!(t.cost, 1.0);
            assert_eq!(report.providers["codex"].subscription, 1);
            assert!(t.line("openai").contains("(reasoning 2)"));
        }
    }
    fn record_line(ts: DateTime<Utc>, session: &str) -> String {
        let r = Record {
            ts,
            session: session.into(),
            provider: "openai".into(),
            model: "m".into(),
            input: 4,
            cached: 0,
            output: 1,
            reasoning: 0,
            cache_write: 2,
            cost_usd: None,
        };
        serde_json::to_string(&r).unwrap() + "\n"
    }
    #[test]
    fn sessions_keep_their_own_usage_copy() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let mut session =
            crate::session::Session::create(&home.join("sessions"), home, "m").unwrap();
        session.append(&Item::user("hi")).unwrap();
        // A session saved before sidecars existed: backfilled from the profile log.
        std::fs::write(
            home.join("usage.jsonl"),
            record_line(Utc::now(), &session.id) + &record_line(Utc::now(), "other"),
        )
        .unwrap();
        let mut stats = Stats::default();
        let usage = stats.restore(&session, Some(home)).unwrap();
        assert_eq!(
            (stats.requests, usage.input_tokens, usage.cache_write_tokens),
            (1, 4, 2)
        );
        let sidecar = home.join("sessions").join(format!("{}.usage", session.id));
        assert!(sidecar.exists());
        let u = Usage {
            input_tokens: 6,
            output_tokens: 1,
            ..Default::default()
        };
        append(home, &session.id, "openai", "openai", "m", u).unwrap();
        // Unsaved sessions (and unknown ids) get no copy.
        append(home, "nosuch", "openai", "openai", "m", u).unwrap();
        assert!(!home.join("sessions").join("nosuch.usage").exists());
        // Resume reads only the copy.
        std::fs::remove_file(home.join("usage.jsonl")).unwrap();
        let usage = stats.restore(&session, Some(home)).unwrap();
        assert_eq!((stats.requests, usage.input_tokens), (2, 10));
        remove_session(home, &session.id);
        assert!(!sidecar.exists());
    }
    #[test]
    fn large_logs_drop_old_records_and_remember_tracking_start() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let old = record_line(Utc::now() - chrono::Duration::days(RETAIN_DAYS + 5), "s");
        let mut data = String::new();
        while data.len() as u64 <= COMPACT_AT {
            data.push_str(&old);
        }
        data.push_str(&record_line(Utc::now() - chrono::Duration::days(2), "s"));
        std::fs::write(home.join("usage.jsonl"), data).unwrap();
        append(home, "s", "openai", "openai", "m", Usage::default()).unwrap();
        let log = std::fs::read_to_string(home.join("usage.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 2);
        let report = read(home, Period::Month).unwrap();
        assert_eq!(report.providers["openai"].requests, 2);
        let since = report.since.unwrap();
        assert!(since > Utc::now() - chrono::Duration::days(RETAIN_DAYS + 1));
        assert!(report.lines().last().unwrap().contains("Tracking since"));
        // A small log is left alone.
        append(home, "s", "openai", "openai", "m", Usage::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join("usage.jsonl"))
                .unwrap()
                .lines()
                .count(),
            3
        );
    }
    #[test]
    fn activity_counts_changed_lines_and_formats_durations() {
        let mut stats = Stats::default();
        let diff = "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n+more\n keep\n";
        stats.file_change("edit_file", &serde_json::json!({}), Some(diff));
        stats.file_change(
            "write_file",
            &serde_json::json!({"content": "a\nb\nc\n"}),
            Some("new file: y\na\nb\nc"),
        );
        stats.file_change("bash", &serde_json::json!({}), Some(diff));
        assert_eq!((stats.lines_added, stats.lines_removed), (5, 1));
        stats.api_time = Duration::from_secs(75);
        let u = Usage {
            reasoning_tokens: 7,
            cache_write_tokens: 3,
            ..Default::default()
        };
        let lines = stats.lines(u);
        assert!(
            lines[1].starts_with("this run: wall 0s · API 1m 15s · lines +5 -1"),
            "{}",
            lines[1]
        );
        assert!(lines[2].contains("reasoning 7") && lines[2].contains("cache writes 3"));
        assert_eq!(duration(Duration::from_secs(3720)), "1h 2m");
    }
}
