//! Append-only profile usage and period reports.
use crate::types::{Item, Usage};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
};

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
    pub cost_usd: Option<f64>,
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
    let record = Record {
        ts: Utc::now(),
        session: session.into(),
        provider: provider.into(),
        model: model.into(),
        input: u.input_tokens,
        cached: u.cached_tokens,
        output: u.output_tokens,
        cost_usd: crate::pricing::lookup(home, pricing_provider, model).map(|p| p.cost(u)),
    };
    let mut line = serde_json::to_vec(&record)?;
    line.push(b'\n');
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(home.join("usage.jsonl"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
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
        file.write_all(&line)?;
        file.sync_data()
    })();
    if let Err(e) = write {
        let _ = file.set_len(before);
        return Err(e.into());
    }
    file.unlock()?;
    Ok(())
}
#[derive(Default, Debug)]
pub struct Total {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cached: u64,
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
        if r.provider == "codex" || r.provider == "grok" {
            self.subscription += 1;
        } else if let Some(c) = r.cost_usd {
            self.cost += c;
        } else {
            self.unknown += 1;
        }
    }
    fn line(&self, label: &str) -> String {
        format!(
            "  {label}: {} requests · input {} (cached {}) · output {} · ${:.6} estimated{}{}",
            self.requests,
            self.input,
            self.cached,
            self.output,
            self.cost,
            if self.unknown > 0 {
                format!(" · {} unknown cost", self.unknown)
            } else {
                String::new()
            },
            if self.subscription > 0 {
                format!(" · {} subscription", self.subscription)
            } else {
                String::new()
            }
        )
    }
}
#[derive(Default)]
pub struct Report {
    pub providers: BTreeMap<String, Total>,
    pub models: BTreeMap<(String, String), Total>,
    pub malformed: usize,
}
impl Report {
    fn add(&mut self, r: Record, period: Period, now: DateTime<Utc>) {
        if r.ts < now - chrono::Duration::hours(period.hours()) || r.ts > now {
            return;
        }
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
        lines.extend(
            self.models
                .iter()
                .map(|((p, m), t)| t.line(&format!("{p}/{m}"))),
        );
        if self.models.is_empty() {
            lines.push("  No usage recorded in this period.".into());
        }
        if self.malformed > 0 {
            lines.push(format!(
                "  Skipped {} malformed usage records.",
                self.malformed
            ));
        }
        lines
    }
}
pub fn read(home: &Path, period: Period) -> Result<Report> {
    let path = home.join("usage.jsonl");
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Report::default()),
        Err(e) => return Err(e).context("reading usage log"),
    };
    let mut report = Report::default();
    let now = Utc::now();
    for line in BufReader::new(file).lines() {
        let line = line?;
        match serde_json::from_str(&line) {
            Ok(r) => report.add(r, period, now),
            Err(_) => report.malformed += 1,
        }
    }
    Ok(report)
}
pub fn show(home: &Path, period: Period) -> Result<()> {
    crate::ui::list(&read(home, period)?.lines());
    Ok(())
}

pub struct Stats {
    pub id: String,
    pub turns: u64,
    pub requests: u64,
    pub tools: BTreeMap<String, u64>,
    started: DateTime<Utc>,
}
impl Default for Stats {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            turns: 0,
            requests: 0,
            tools: BTreeMap::new(),
            started: Utc::now(),
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
        *self = Self::default();
        if let Ok(file) = std::fs::File::open(&session.path) {
            for line in BufReader::new(file).lines() {
                let line = line?;
                if let Ok(item) = serde_json::from_str::<Item>(&line) {
                    self.item(&item);
                } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    if let Some(created) = v["created"].as_str().and_then(|s| s.parse().ok()) {
                        self.started = created;
                    }
                }
            }
        }
        let mut usage = Usage::default();
        if let Some(home) = home {
            match std::fs::File::open(home.join("usage.jsonl")) {
                Ok(file) => {
                    for line in BufReader::new(file).lines() {
                        if let Ok(r) = serde_json::from_str::<Record>(&line?) {
                            if r.session == session.id {
                                self.requests += 1;
                                usage.input_tokens += r.input;
                                usage.output_tokens += r.output;
                                usage.cached_tokens += r.cached;
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
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
    pub fn show(&self, u: Usage) {
        let mut lines = vec![
            format!(
                "{} turns · {} requests · duration {}s",
                self.turns,
                self.requests,
                (Utc::now() - self.started).num_seconds().max(0)
            ),
            format!(
                "input {} · output {} · cached {}",
                u.input_tokens, u.output_tokens, u.cached_tokens
            ),
        ];
        lines.extend(
            self.tools
                .iter()
                .map(|(name, n)| format!("  {name}: {n} tool calls")),
        );
        crate::ui::list(&lines);
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
                            },
                        )
                        .unwrap();
                    }
                });
            }
        });
        let report = read(dir.path(), Period::Day).unwrap();
        assert_eq!(report.malformed, 1);
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
                        cost_usd: c,
                    },
                    period,
                    now,
                );
            }
            assert_eq!(report.models.len(), 3);
            let t = &report.providers["openai"];
            assert_eq!(
                (t.requests, t.input, t.cached, t.output, t.unknown),
                (2, 20, 4, 6, 1)
            );
            assert_eq!(t.cost, 1.0);
            assert_eq!(report.providers["codex"].subscription, 1);
        }
    }
}
