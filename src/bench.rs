//! `nyq bench`: run a directory of tasks headlessly and record one JSONL
//! result per task; `nyq bench report a.jsonl b.jsonl` compares two runs.
//!
//! A task is a directory with a `task.toml`:
//!
//! ```toml
//! prompt = "What to ask the agent"
//! fixture = "fixture"      # directory copied into the run's tempdir (default: fixture)
//! setup = "setup.sh"       # run inside the tempdir before the agent (optional)
//! check = "check.sh"       # run inside the tempdir after; exit 0 is a pass
//! schema = "schema.json"   # handed to the run as verify.schema (optional)
//! timeout_secs = 600
//! ```
//!
//! Each task runs the built binary itself (`nyq -p --json --no-save`) in a
//! fresh temporary copy of its fixture, under `--profile minimal` unless
//! `--profile` says otherwise; `--ablate key=value` pairs become the child's
//! `--set` overrides.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

/// One task directory's `task.toml`.
#[derive(Debug)]
pub struct TaskDef {
    pub dir: PathBuf,
    /// The task directory's name.
    pub name: String,
    pub prompt: String,
    pub setup: Option<String>,
    pub check: Option<String>,
    pub fixture: Option<String>,
    pub schema: Option<String>,
    pub timeout_secs: u64,
}

fn string(m: &Map<String, Value>, key: &str) -> Option<String> {
    m.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse `<dir>/task.toml`; `prompt` is required, everything else optional.
pub fn parse_task(dir: &Path) -> Result<TaskDef> {
    let file = dir.join("task.toml");
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let map = crate::settings::file::parse_str(&text)
        .map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let prompt = string(&map, "prompt")
        .with_context(|| format!("{}: a task needs a `prompt`", file.display()))?;
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("task")
        .to_string();
    Ok(TaskDef {
        dir: dir.to_path_buf(),
        name,
        prompt,
        setup: string(&map, "setup"),
        check: string(&map, "check"),
        fixture: string(&map, "fixture"),
        schema: string(&map, "schema"),
        timeout_secs: map
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(600),
    })
}

/// Every task directory under `root` (sorted by name); nested tasks are
/// picked up too, but a task directory is not descended into.
pub fn tasks(root: &Path) -> Result<Vec<TaskDef>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.join("task.toml").is_file() {
            out.push(parse_task(&dir)?);
            continue;
        }
        let entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        stack.extend(entries);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Copy `src`'s contents into `dst` (the fixture into the run's tempdir).
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

/// A task's temporary workspace: a directory under the system tempdir,
/// removed when it goes out of scope.
struct WorkDir(PathBuf);

impl WorkDir {
    fn create(task: &TaskDef) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "nyq-bench-{}-{}-{}",
            std::process::id(),
            task.name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(WorkDir(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `sh script` in `work`; scripts are paths relative to the task directory.
async fn script(task: &TaskDef, script: &str, work: &Path) -> Result<i32> {
    let status = tokio::process::Command::new("sh")
        .arg(task.dir.join(script))
        .current_dir(work)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .with_context(|| format!("running {script}"))?;
    Ok(status.code().unwrap_or(-1))
}

/// The one JSON object a `--json` child printed: the last line that parses
/// as an object with `exit_code`.
fn run_output(stdout: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("exit_code").is_some())
}

/// Run every task under `dir` and write one JSONL record per task (to
/// `output`, or stdout).
pub async fn run(
    dir: &Path,
    profile: Option<String>,
    ablate: &[String],
    output: Option<&Path>,
    model: Option<&str>,
    provider: Option<&str>,
) -> Result<()> {
    let tasks = tasks(dir)?;
    if tasks.is_empty() {
        bail!("no task.toml found under {}", dir.display());
    }
    let exe = std::env::current_exe().context("locating the nyq binary")?;
    let profile = profile.unwrap_or_else(|| "minimal".to_string());
    let mut out: Box<dyn Write> = match output {
        Some(path) => Box::new(
            std::fs::File::create(path).with_context(|| format!("writing {}", path.display()))?,
        ),
        None => Box::new(std::io::stdout().lock()),
    };
    let mut passed = 0usize;
    for task in &tasks {
        let record = run_one(&exe, task, &profile, ablate, model, provider).await?;
        writeln!(out, "{}", serde_json::to_string(&record)?)?;
        out.flush()?;
        if record["pass"] == true {
            passed += 1;
        }
        eprintln!(
            "bench {:<24} {} ({}ms)",
            task.name,
            if record["pass"] == true {
                "pass"
            } else {
                "FAIL"
            },
            record["wall_ms"]
        );
    }
    eprintln!("bench: {passed}/{} tasks passed", tasks.len());
    Ok(())
}

/// One task: copy the fixture, run setup, run the agent headless, run the
/// check. The result row is the JSONL record.
async fn run_one(
    exe: &Path,
    task: &TaskDef,
    profile: &str,
    ablate: &[String],
    model: Option<&str>,
    provider: Option<&str>,
) -> Result<Value> {
    let started = Instant::now();
    let work = WorkDir::create(task)?;
    let fixture = task
        .fixture
        .clone()
        .map(|f| task.dir.join(f))
        .unwrap_or_else(|| task.dir.join("fixture"));
    if fixture.is_dir() {
        copy_tree(&fixture, work.path())?;
    }
    let mut record = Map::new();
    record.insert("task".into(), task.name.clone().into());
    if !ablate.is_empty() {
        let kv: Map<String, Value> = ablate
            .iter()
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect();
        record.insert("ablate".into(), kv.into());
    }
    if let Some(setup) = &task.setup {
        let code = script(task, setup, work.path()).await?;
        if code != 0 {
            record.insert("pass".into(), false.into());
            record.insert("setup_exit".into(), code.into());
            record.insert(
                "wall_ms".into(),
                (started.elapsed().as_millis() as u64).into(),
            );
            return Ok(record.into());
        }
    }
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("-p")
        .arg("--json")
        .arg("--no-save")
        .arg("--profile")
        .arg(profile)
        .arg("--yolo");
    for pair in ablate {
        cmd.arg("--set").arg(pair);
    }
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    if let Some(p) = provider {
        cmd.arg("--provider").arg(p);
    }
    if let Some(schema) = &task.schema {
        cmd.arg("--set")
            .arg(format!("verify.schema={}", task.dir.join(schema).display()));
    }
    cmd.arg(&task.prompt)
        .current_dir(work.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Dropping the future on timeout kills the process group.
        .kill_on_drop(true);
    let child = cmd.spawn().context("spawning the task agent")?;
    let limit = std::time::Duration::from_secs(task.timeout_secs);
    let (result, timed_out) = match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(r) => (Some(r.context("waiting for the task agent")?), false),
        Err(_) => (None, true),
    };
    let stdout = result.as_ref().and_then(|o| run_output(&o.stdout));
    let agent_exit = stdout
        .as_ref()
        .and_then(|v| v["exit_code"].as_i64())
        .unwrap_or(-1);
    if timed_out {
        record.insert("timed_out".into(), true.into());
    }
    record.insert("agent_exit".into(), agent_exit.into());
    if let Some(v) = &stdout {
        if let Some(steps) = v.get("steps") {
            record.insert("steps".into(), steps.clone());
        }
        if let Some(u) = v.get("usage") {
            for key in ["input_tokens", "output_tokens", "cached_tokens"] {
                if let Some(n) = u.get(key) {
                    record.insert(key.into(), n.clone());
                }
            }
        }
        if let Some(c) = v.get("cost_usd") {
            record.insert("cost_usd".into(), c.clone());
        }
        if let Some(id) = v.get("session_id") {
            record.insert("session_id".into(), id.clone());
        }
        if let Some(e) = v.get("error").and_then(Value::as_str) {
            record.insert("agent_error".into(), e.into());
        }
    } else {
        let tail = result
            .as_ref()
            .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
            .unwrap_or_default();
        if let Some(tail) = tail.lines().last().filter(|l| !l.is_empty()) {
            record.insert("agent_error".into(), tail.into());
        }
    }
    let pass = if timed_out || agent_exit != 0 {
        false
    } else if let Some(check) = &task.check {
        script(task, check, work.path()).await? == 0
    } else {
        true
    };
    record.insert("pass".into(), pass.into());
    record.insert(
        "wall_ms".into(),
        (started.elapsed().as_millis() as u64).into(),
    );
    Ok(record.into())
}

/// `nyq bench report a.jsonl b.jsonl`: per-task and total comparison.
pub fn report(a: &Path, b: &Path) -> Result<()> {
    let a = read_results(a)?;
    let b = read_results(b)?;
    let mut names: Vec<String> = a.keys().cloned().collect();
    for k in b.keys() {
        if !names.contains(k) {
            names.push(k.clone());
        }
    }
    if names.is_empty() {
        println!("no results to compare");
        return Ok(());
    }
    println!(
        "{:<28} {:^9} {:^9} {:>10} {:>10} {:>10} {:>10}",
        "task", "A", "B", "steps A→B", "tokens A→B", "cost A→B", "ms A→B"
    );
    let mut pass_a = 0usize;
    let mut pass_b = 0usize;
    let mut tok_a = 0u64;
    let mut tok_b = 0u64;
    let mut cost_a = 0f64;
    let mut cost_b = 0f64;
    for name in &names {
        let ra = a.get(name);
        let rb = b.get(name);
        let pa = ra.is_some_and(|r| r["pass"] == true);
        let pb = rb.is_some_and(|r| r["pass"] == true);
        pass_a += pa as usize;
        pass_b += pb as usize;
        tok_a += tokens(ra);
        tok_b += tokens(rb);
        cost_a += cost(ra);
        cost_b += cost(rb);
        println!(
            "{:<28} {:^9} {:^9} {:>10} {:>10} {:>10} {:>10}",
            name,
            verdict(pa, ra.is_some()),
            verdict(pb, rb.is_some()),
            delta(
                ra.and_then(|r| r["steps"].as_i64()),
                rb.and_then(|r| r["steps"].as_i64()),
                ""
            ),
            format!("{}→{}", tokens(ra), tokens(rb)),
            format!("{:.4}→{:.4}", cost(ra), cost(rb)),
            delta(
                ra.and_then(|r| r["wall_ms"].as_i64()),
                rb.and_then(|r| r["wall_ms"].as_i64()),
                ""
            ),
        );
    }
    println!("{}", "-".repeat(86));
    println!(
        "pass: {}/{} → {}/{} · tokens: {} → {} · cost: {:.4} → {:.4}",
        pass_a,
        names.len(),
        pass_b,
        names.len(),
        tok_a,
        tok_b,
        cost_a,
        cost_b
    );
    Ok(())
}

fn verdict(pass: bool, present: bool) -> &'static str {
    if !present {
        "—"
    } else if pass {
        "pass"
    } else {
        "fail"
    }
}

fn delta(a: Option<i64>, b: Option<i64>, unit: &str) -> String {
    let f = |v: Option<i64>| v.map_or("—".to_string(), |n| format!("{n}{unit}"));
    format!("{}→{}", f(a), f(b))
}

fn tokens(r: Option<&Value>) -> u64 {
    r.map_or(0, |r| {
        r["input_tokens"].as_u64().unwrap_or(0) + r["output_tokens"].as_u64().unwrap_or(0)
    })
}

fn cost(r: Option<&Value>) -> f64 {
    r.and_then(|r| r["cost_usd"].as_f64()).unwrap_or(0.0)
}

/// A results file as `task -> record`; blank/broken lines are skipped.
fn read_results(path: &Path) -> Result<BTreeMap<String, Value>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(name) = v.get("task").and_then(Value::as_str) {
            out.insert(name.to_string(), v);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_dir(root: &Path, name: &str, toml: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("task.toml"), toml).unwrap();
        dir
    }

    #[test]
    fn parses_a_full_task() {
        let t = tempfile::tempdir().unwrap();
        let dir = task_dir(
            t.path(),
            "demo",
            r#"prompt = "do the thing"
setup = "setup.sh"
check = "check.sh"
fixture = "files"
schema = "answer.schema.json"
timeout_secs = 42
"#,
        );
        let task = parse_task(&dir).unwrap();
        assert_eq!(task.name, "demo");
        assert_eq!(task.prompt, "do the thing");
        assert_eq!(task.setup.as_deref(), Some("setup.sh"));
        assert_eq!(task.check.as_deref(), Some("check.sh"));
        assert_eq!(task.fixture.as_deref(), Some("files"));
        assert_eq!(task.schema.as_deref(), Some("answer.schema.json"));
        assert_eq!(task.timeout_secs, 42);
    }

    #[test]
    fn prompt_is_required_and_defaults_apply() {
        let t = tempfile::tempdir().unwrap();
        let dir = task_dir(t.path(), "bare", "prompt = \"hi\"\n");
        let task = parse_task(&dir).unwrap();
        assert_eq!(task.timeout_secs, 600);
        assert!(task.check.is_none() && task.setup.is_none());

        let dir = task_dir(t.path(), "noprompt", "check = \"c.sh\"\n");
        assert!(parse_task(&dir).is_err());
    }

    #[test]
    fn tasks_finds_directories_sorted() {
        let t = tempfile::tempdir().unwrap();
        task_dir(t.path(), "b-task", "prompt = \"b\"\n");
        task_dir(t.path(), "a-task", "prompt = \"a\"\n");
        std::fs::create_dir_all(t.path().join("not-a-task")).unwrap();
        let nested = task_dir(&t.path().join("group"), "inner", "prompt = \"i\"\n");
        let found = tasks(t.path()).unwrap();
        let names: Vec<&str> = found.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["a-task", "b-task", "inner"]);
        assert!(found.iter().any(|t| t.dir == nested));
    }

    #[test]
    fn task_toml_must_parse() {
        let t = tempfile::tempdir().unwrap();
        let dir = task_dir(t.path(), "broken", "prompt = [unclosed\n");
        assert!(parse_task(&dir).is_err());
    }

    #[test]
    fn run_output_takes_the_last_json_line() {
        let stdout = b"some log\n{\"noise\":1}\n{\"exit_code\":0,\"steps\":3}\n";
        let v = run_output(stdout).unwrap();
        assert_eq!(v["steps"], 3);
        assert!(run_output(b"not json\n").is_none());
    }

    #[test]
    fn read_results_keys_by_task() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("r.jsonl");
        std::fs::write(
            &path,
            "{\"task\":\"a\",\"pass\":true}\nbroken\n{\"task\":\"b\",\"pass\":false}\n",
        )
        .unwrap();
        let m = read_results(&path).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m["a"]["pass"], true);
        assert_eq!(m["b"]["pass"], false);
    }
}
