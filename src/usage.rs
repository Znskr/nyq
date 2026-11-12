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
/// Token estimates for context accounting, used until (and between) the
/// provider's own usage reports.
///
/// There is no tokenizer here: real vocabularies are megabytes each. Text
/// is split into classes that tokenize differently (letters and word
/// breaks, punctuation, digits, other non-ASCII, CJK), the letter rate
/// shifts from prose toward code as punctuation gets denser, and each model
/// family scales the result. A per-session correction factor, fitted to the
/// provider-reported input tokens, absorbs what the heuristic misses
/// (message framing, tool-schema rendering, a family's real vocabulary).
pub mod tokens {
    use base64::Engine;

    use crate::types::Image;

    /// Tokenizer families with measurably different rates.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum Family {
        /// Unknown model: slightly pessimistic rates.
        #[default]
        Other,
        /// GPT-4o and later (o200k-style vocabularies), Codex.
        OpenAi,
        Anthropic,
        Gemini,
        DeepSeek,
        /// Qwen, GLM, Kimi: large vocabularies with many CJK merges.
        Qwen,
    }

    impl Family {
        pub fn detect(provider: &str, model: &str) -> Family {
            let model = model.to_ascii_lowercase();
            let has = |s: &str| model.contains(s);
            if has("claude") || provider == "anthropic" {
                Family::Anthropic
            } else if has("gemini") || has("gemma") {
                Family::Gemini
            } else if has("deepseek") {
                Family::DeepSeek
            } else if has("qwen") || has("glm") || has("kimi") || has("moonshot") {
                Family::Qwen
            } else if has("gpt")
                || has("codex")
                || model.starts_with("o1")
                || model.starts_with("o3")
                || model.starts_with("o4")
                || matches!(provider, "openai" | "codex")
            {
                Family::OpenAi
            } else {
                Family::Other
            }
        }

        /// Multiplier on the Latin/code part, relative to an o200k-style
        /// vocabulary.
        fn latin(self) -> f64 {
            match self {
                Family::OpenAi | Family::Gemini => 1.0,
                Family::Anthropic => 1.12,
                Family::DeepSeek | Family::Qwen => 1.05,
                Family::Other => 1.1,
            }
        }

        /// Tokens per CJK character.
        fn cjk(self) -> f64 {
            match self {
                Family::DeepSeek | Family::Qwen => 0.7,
                Family::OpenAi | Family::Gemini => 0.85,
                Family::Anthropic => 1.1,
                Family::Other => 1.0,
            }
        }

        /// Tokens per other non-ASCII character (accented Latin, Cyrillic,
        /// symbols, emoji bytes).
        fn other(self) -> f64 {
            match self {
                Family::Anthropic | Family::Other => 0.8,
                _ => 0.5,
            }
        }
    }

    /// Letters (plus word breaks) per token in prose and in dense code.
    const PROSE_LETTERS: f64 = 4.4;
    const CODE_LETTERS: f64 = 3.6;
    /// Punctuation share (of non-space characters) at which text counts as
    /// fully code- or JSON-like.
    const CODE_DENSITY: f64 = 0.25;
    const PUNCT: f64 = 0.6;
    const DIGITS_PER_TOKEN: f64 = 2.5;
    /// An image whose size cannot be read.
    pub const IMAGE_FALLBACK: u64 = 1_500;
    /// Requests smaller than this tell too little to calibrate on.
    const MIN_CALIBRATION: f64 = 256.0;
    /// Weight of the newest sample in the running factor.
    const ALPHA: f64 = 0.3;

    /// Hangul, kana, CJK ideographs and fullwidth forms.
    fn is_cjk(c: char) -> bool {
        matches!(c as u32,
            0x1100..=0x11FF | 0x2E80..=0x2FDF | 0x3000..=0x30FF | 0x3100..=0x31FF
            | 0x3200..=0x4DBF | 0x4E00..=0x9FFF | 0xA960..=0xA97F | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
    }

    /// Family-independent counts for one text.
    #[derive(Default, Debug, PartialEq)]
    struct Counts {
        letters: u64,
        breaks: u64,
        punct: u64,
        digits: u64,
        other: u64,
        cjk: u64,
    }

    fn count(text: &str) -> Counts {
        let mut c = Counts::default();
        let mut in_space = false;
        for ch in text.chars() {
            if ch.is_whitespace() {
                // A run of whitespace (indentation, a blank line) mostly
                // merges into one token or into the next word.
                if !in_space {
                    c.breaks += 1;
                }
                in_space = true;
                continue;
            }
            in_space = false;
            if ch.is_ascii_alphabetic() || ch == '_' {
                c.letters += 1;
            } else if ch.is_ascii_digit() {
                c.digits += 1;
            } else if ch.is_ascii() {
                c.punct += 1;
            } else if is_cjk(ch) {
                c.cjk += 1;
            } else {
                c.other += 1;
            }
        }
        c
    }

    /// Running estimator for one session.
    #[derive(Clone, Debug)]
    pub struct Estimator {
        family: Family,
        /// Provider-reported tokens per estimated token, fitted per session.
        factor: f64,
        samples: u32,
    }

    impl Default for Estimator {
        fn default() -> Self {
            Estimator {
                family: Family::Other,
                factor: 1.0,
                samples: 0,
            }
        }
    }

    impl Estimator {
        pub fn new(family: Family) -> Self {
            Estimator {
                family,
                ..Default::default()
            }
        }

        pub fn family(&self) -> Family {
            self.family
        }

        /// Follow a model switch. A new family starts uncalibrated.
        pub fn set_family(&mut self, family: Family) {
            if family != self.family {
                *self = Estimator::new(family);
            }
        }

        pub fn factor(&self) -> f64 {
            self.factor
        }

        /// Uncalibrated tokens for `text`.
        pub fn raw_text(&self, text: &str) -> f64 {
            if text.is_empty() {
                return 0.0;
            }
            let c = count(text);
            let dense = (c.punct + c.letters + c.digits).max(1) as f64;
            let code = ((c.punct as f64 / dense) / CODE_DENSITY).min(1.0);
            let letters_per_token = PROSE_LETTERS + (CODE_LETTERS - PROSE_LETTERS) * code;
            let latin = (c.letters + c.breaks) as f64 / letters_per_token
                + c.punct as f64 * PUNCT
                + c.digits as f64 / DIGITS_PER_TOKEN;
            latin * self.family.latin()
                + c.cjk as f64 * self.family.cjk()
                + c.other as f64 * self.family.other()
        }

        /// Uncalibrated tokens for one image. Sizes are read from the PNG,
        /// GIF, JPEG or WebP header; anything else costs a flat amount.
        pub fn raw_image(&self, image: &Image) -> f64 {
            let Some((w, h)) = image_size(&image.data) else {
                return IMAGE_FALLBACK as f64;
            };
            let (w, h) = (w as f64, h as f64);
            match self.family {
                Family::OpenAi => {
                    // Fit 2048², shortest side to 768, then 512-px tiles.
                    let s = (2048.0 / w.max(h)).min(1.0);
                    let (w, h) = (w * s, h * s);
                    let s = (768.0 / w.min(h)).min(1.0);
                    let (w, h) = (w * s, h * s);
                    85.0 + 170.0 * ((w / 512.0).ceil() * (h / 512.0).ceil())
                }
                Family::Gemini if w <= 384.0 && h <= 384.0 => 258.0,
                Family::Gemini => 258.0 * (w / 768.0).ceil() * (h / 768.0).ceil(),
                _ => {
                    // Anthropic-style: long edge to 1568 px and about
                    // 1.15 MP, then one token per 750 px².
                    let s = (1568.0 / w.max(h)).min(1.0);
                    let (w, h) = (w * s, h * s);
                    let s = (1_150_000.0 / (w * h)).sqrt().min(1.0);
                    (w * s * h * s / 750.0).max(1.0)
                }
            }
        }

        /// Calibrated tokens for an uncalibrated amount.
        pub fn tokens(&self, raw: f64) -> u64 {
            (raw * self.factor).ceil() as u64
        }

        pub fn text(&self, text: &str) -> u64 {
            self.tokens(self.raw_text(text))
        }

        /// Fit the factor to a provider report: `raw` was this estimator's
        /// uncalibrated count for the whole request, `actual` the reported
        /// input tokens.
        pub fn calibrate(&mut self, raw: f64, actual: u64) {
            if raw < MIN_CALIBRATION || actual == 0 {
                return;
            }
            let ratio = (actual as f64 / raw).clamp(0.5, 2.5);
            self.factor = if self.samples == 0 {
                ratio
            } else {
                self.factor * (1.0 - ALPHA) + ratio * ALPHA
            };
            self.samples = self.samples.saturating_add(1);
        }

        /// The provider rejected a request estimated at `raw` as larger than
        /// `window`: estimates must at least reach the window from now on.
        pub fn overflowed(&mut self, raw: f64, window: u64) {
            if raw >= MIN_CALIBRATION && raw * self.factor < window as f64 {
                self.factor = (window as f64 * 1.05 / raw).clamp(self.factor, 2.5);
                self.samples = self.samples.max(1);
            }
        }
    }

    /// Width and height from an image's base64 data, when the format is known.
    pub fn image_size(data: &str) -> Option<(u32, u32)> {
        // Headers are at the start, except JPEG's frame header, which
        // follows any metadata segments: decode at most 64 KiB.
        let take = data.len().min(87_384) / 4 * 4;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&data.as_bytes()[..take])
            .ok()?;
        let be16 = |i: usize| -> Option<u32> {
            Some(u32::from(u16::from_be_bytes(
                bytes.get(i..i + 2)?.try_into().ok()?,
            )))
        };
        let le16 = |i: usize| -> Option<u32> {
            Some(u32::from(u16::from_le_bytes(
                bytes.get(i..i + 2)?.try_into().ok()?,
            )))
        };
        let le24 = |i: usize| -> Option<u32> {
            let b = bytes.get(i..i + 3)?;
            Some(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
        };
        let size = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            let w = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
            let h = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
            (w, h)
        } else if bytes.starts_with(b"GIF8") {
            (le16(6)?, le16(8)?)
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            match bytes.get(12..16)? {
                b"VP8X" => (le24(24)? + 1, le24(27)? + 1),
                b"VP8L" => {
                    let b = u32::from_le_bytes(bytes.get(21..25)?.try_into().ok()?);
                    ((b & 0x3FFF) + 1, ((b >> 14) & 0x3FFF) + 1)
                }
                b"VP8 " => (le16(26)? & 0x3FFF, le16(28)? & 0x3FFF),
                _ => return None,
            }
        } else if bytes.starts_with(&[0xFF, 0xD8]) {
            let mut i = 2;
            loop {
                if *bytes.get(i)? != 0xFF {
                    return None;
                }
                let marker = *bytes.get(i + 1)?;
                // SOF0..SOF15 except DHT, JPG and DAC carry the frame size.
                if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                    break (be16(i + 7)?, be16(i + 5)?);
                }
                i += 2 + be16(i + 2)? as usize;
            }
        } else {
            return None;
        };
        (size.0 > 0 && size.1 > 0).then_some(size)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn png(w: u32, h: u32) -> Image {
            let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
            bytes.extend(w.to_be_bytes());
            bytes.extend(h.to_be_bytes());
            bytes.extend([8, 6, 0, 0, 0]);
            Image {
                media_type: "image/png".into(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            }
        }

        #[test]
        fn families_are_detected_from_provider_and_model() {
            for (provider, model, family) in [
                ("anthropic", "x", Family::Anthropic),
                ("openrouter", "anthropic/claude-sonnet-5", Family::Anthropic),
                ("codex", "gpt-5.3-codex", Family::OpenAi),
                ("openai", "o3-mini", Family::OpenAi),
                ("deepseek", "deepseek-chat", Family::DeepSeek),
                ("opencode-go", "glm-5.3", Family::Qwen),
                ("google", "gemini-3-pro", Family::Gemini),
                ("ollama", "llama3", Family::Other),
            ] {
                assert_eq!(Family::detect(provider, model), family, "{model}");
            }
        }

        #[test]
        fn code_and_json_cost_more_per_char_than_prose_and_cjk_counts_per_char() {
            let e = Estimator::new(Family::OpenAi);
            let prose = "The quick brown fox jumps over the lazy dog while the farmer watches \
                         from the porch and wonders whether the fence will hold this winter.";
            let code = r#"fn main() { let x: Vec<u8> = vec![1, 2]; println!("{:?}", &x[..]); }"#;
            let json = r#"{"path":"src/main.rs","offset":10,"limit":[1,2,3],"ok":true}"#;
            let per_char = |s: &str| e.raw_text(s) / s.chars().count() as f64;
            assert!(per_char(prose) < 0.27, "{}", per_char(prose));
            assert!(per_char(code) > per_char(prose) * 1.3, "{}", per_char(code));
            assert!(per_char(json) > per_char(prose) * 1.3, "{}", per_char(json));
            // About 10 tokens for a classic 44-char sentence.
            let fox = e.raw_text("The quick brown fox jumps over the lazy dog.");
            assert!((9.0..12.0).contains(&fox), "{fox}");
            // Indentation is cheap: a run of spaces is one break.
            let flat = e.raw_text("a = b;\nc = d;");
            let indented = e.raw_text("a = b;\n                c = d;");
            assert!((indented - flat).abs() < 0.01);
            // CJK: one weight per character, above bytes/4.
            let cjk = "你好世界，这是一个测试";
            let n = cjk.chars().count() as f64;
            assert!((e.raw_text(cjk) - n * Family::OpenAi.cjk()).abs() < 1.0);
            assert!(e.raw_text(cjk) > cjk.len() as f64 / 4.0);
            let claude = Estimator::new(Family::Anthropic);
            assert!(claude.raw_text(prose) > e.raw_text(prose));
            assert_eq!(e.text(""), 0);
        }

        #[test]
        fn calibration_follows_provider_usage_and_overflow() {
            let mut e = Estimator::new(Family::Other);
            assert_eq!(e.factor(), 1.0);
            // Too small to tell anything.
            e.calibrate(100.0, 500);
            assert_eq!(e.factor(), 1.0);
            e.calibrate(1_000.0, 1_300);
            assert!((e.factor() - 1.3).abs() < 1e-9);
            assert_eq!(e.tokens(1_000.0), 1_300);
            // Later samples move the factor gradually.
            e.calibrate(1_000.0, 1_000);
            assert!((e.factor() - 1.21).abs() < 1e-9, "{}", e.factor());
            for _ in 0..40 {
                e.calibrate(2_000.0, 2_000);
            }
            assert!((e.factor() - 1.0).abs() < 0.01);
            // Outliers are clamped.
            let mut wild = Estimator::new(Family::Other);
            wild.calibrate(1_000.0, 100_000);
            assert_eq!(wild.factor(), 2.5);
            // An overflow lifts the estimate to at least the window.
            let mut e = Estimator::new(Family::Other);
            e.overflowed(50_000.0, 100_000);
            assert!(e.tokens(50_000.0) >= 100_000);
            let before = e.factor();
            e.overflowed(200_000.0, 100_000);
            assert_eq!(e.factor(), before);
            // A model switch to another family starts over.
            e.set_family(Family::Other);
            assert_eq!(e.factor(), before);
            e.set_family(Family::Anthropic);
            assert_eq!(e.factor(), 1.0);
        }

        #[test]
        fn image_cost_comes_from_dimensions() {
            assert_eq!(image_size(&png(800, 600).data), Some((800, 600)));
            let gif = base64::engine::general_purpose::STANDARD
                .encode(b"GIF89a\x40\x01\xf0\x00\x00\x00\x00");
            assert_eq!(image_size(&gif), Some((320, 240)));
            let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0];
            jpeg.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0xE0, 0x02, 0x80, 0x03]);
            let jpeg = base64::engine::general_purpose::STANDARD.encode(jpeg);
            assert_eq!(image_size(&jpeg), Some((640, 480)));
            let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
            webp.extend([0x1F, 0x03, 0x00, 0x57, 0x02, 0x00]);
            let webp = base64::engine::general_purpose::STANDARD.encode(webp);
            assert_eq!(image_size(&webp), Some((800, 600)));
            assert_eq!(image_size("not base64!"), None);

            let claude = Estimator::new(Family::Anthropic);
            assert_eq!(claude.raw_image(&png(750, 100)).round(), 100.0);
            let huge = claude.raw_image(&png(8000, 8000));
            assert!((1_400.0..1_600.0).contains(&huge), "{huge}");
            let gpt = Estimator::new(Family::OpenAi);
            assert_eq!(gpt.raw_image(&png(512, 512)), 255.0);
            assert_eq!(gpt.raw_image(&png(2048, 4096)), 85.0 + 170.0 * 6.0);
            let gemini = Estimator::new(Family::Gemini);
            assert_eq!(gemini.raw_image(&png(300, 300)), 258.0);
            let unknown = Image {
                media_type: "image/png".into(),
                data: "A".repeat(4_000),
            };
            assert_eq!(claude.raw_image(&unknown), IMAGE_FALLBACK as f64);
        }
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
