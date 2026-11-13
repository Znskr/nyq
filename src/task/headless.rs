//! Headless typed tasks: `nyq -p "<prompt>" --schema <file|json>` with
//! `--validate`, `--budget` and `--memo`.
//!
//! The run prints the validated JSON on stdout and exits 0; a budget that
//! runs out or an escalation exits 2 with a JSON report on stderr; anything
//! else that goes wrong exits 1.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, Ui};

use super::budget::BudgetLimits;
use super::engine::{parse_budget, MemoSpec, TaskSpec, ValidatorSpec};
use super::memo::{MemoEvidence, MemoKey, MemoStore, Replay};

/// The task flags of `nyq -p` / `nyq ask`.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct TaskOpts {
    /// Finish with a JSON value matching this JSON Schema (a file or inline JSON), print it and exit 0; exit 2 if the budget runs out (implies -p).
    #[arg(long, value_name = "FILE|JSON")]
    pub schema: Option<String>,
    /// Check the result before accepting it: build, tests, lint, cmd:<shell>, exists:<json-pointer>, judge:<rubric>. Repeatable (implies -p).
    #[arg(long = "validate", value_name = "CHECK")]
    pub validate: Vec<String>,
    /// Limits for a typed task: syntax=N,semantic=N,steps=N,tokens=N,cost=X,wall=S (implies -p).
    #[arg(long, value_name = "LIMITS")]
    pub budget: Option<String>,
    /// Reuse the cached result of an identical earlier task (same prompt, schema, checks, model and workspace: the --memo-file contents, else the git state); the checks run again on it (implies -p).
    #[arg(long)]
    pub memo: bool,
    /// A file whose contents are part of the --memo key, in place of the git state. Repeatable.
    #[arg(long = "memo-file", value_name = "PATH")]
    pub memo_file: Vec<PathBuf>,
}

/// The schema used when checks are given without `--schema`: a summary.
fn default_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string", "description": "What you did, in a few sentences."}
        },
        "required": ["summary"]
    })
}

impl TaskOpts {
    /// Whether these flags ask for a typed task (and so for print mode).
    pub fn is_task(&self) -> bool {
        self.schema.is_some()
            || !self.validate.is_empty()
            || self.budget.is_some()
            || self.memo
            || !self.memo_file.is_empty()
    }

    /// The task for `goal`, or None for a plain answer. Errors are bad flags.
    pub fn spec(&self, goal: &str, cwd: &Path) -> Result<Option<TaskSpec>> {
        if !self.is_task() {
            return Ok(None);
        }
        let schema = match &self.schema {
            Some(arg) => load_schema(arg, cwd)?,
            None => default_schema(),
        };
        let validators = self
            .validate
            .iter()
            .map(|v| ValidatorSpec::parse(v).map_err(anyhow::Error::msg))
            .collect::<Result<Vec<_>>>()?;
        if let Some(v) = validators.iter().find(|v| v.is_check()) {
            anyhow::ensure!(
                super::build::detect_build_system(cwd).is_some(),
                "--validate {}: no build system found in {} (cargo, go, uv/pytest, npm/pnpm/yarn, cmake or a Makefile)",
                v.label(),
                cwd.display()
            );
        }
        let limits = match &self.budget {
            Some(b) => parse_budget(b, BudgetLimits::default()).map_err(anyhow::Error::msg)?,
            None => BudgetLimits::default(),
        };
        let memo = (self.memo || !self.memo_file.is_empty()).then(|| MemoSpec {
            files: self.memo_file.clone(),
        });
        let mut spec = TaskSpec::typed(goal.trim(), schema);
        spec.validators = validators;
        spec.limits = limits;
        spec.memo = memo;
        spec.headless = true;
        Ok(Some(spec))
    }
}

/// `--schema`: inline JSON when it starts like JSON, otherwise a file
/// (relative to `cwd`).
pub fn load_schema(arg: &str, cwd: &Path) -> Result<Value> {
    let trimmed = arg.trim_start();
    let (text, from) = if trimmed.starts_with('{') || trimmed.starts_with('[') {
        (arg.to_string(), "inline --schema".to_string())
    } else {
        let path = cwd.join(arg);
        (
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading --schema {}", path.display()))?,
            path.display().to_string(),
        )
    };
    let schema: Value =
        serde_json::from_str(&text).with_context(|| format!("{from} is not valid JSON"))?;
    anyhow::ensure!(
        schema.is_object(),
        "{from} must be a JSON Schema object, e.g. {{\"type\": \"object\", \"properties\": {{…}}}}"
    );
    Ok(schema)
}

/// What `--memo` found for a task.
#[derive(Debug, PartialEq)]
pub enum MemoLookup {
    /// No `--memo`, or memoisation is off for this run (the notice says why).
    Off(Option<String>),
    /// A cached result that passed the task's checks again: use it.
    Hit(Value),
    /// Run the task, and store its result under this key.
    Miss(MemoKey),
}

/// The model a memo key is tied to: provider and model id.
fn model_id(agent: &Agent) -> String {
    format!("{}/{}", agent.provider.name(), agent.provider.model())
}

/// Look up `spec`'s cached result in `home`'s cache for the agent's
/// workspace. A cached value counts only when the task's checks (schema,
/// validators, TaskValidate hooks) pass on it again; one that fails is
/// dropped and the task runs.
pub async fn memo_lookup(
    agent: &mut Agent,
    spec: &TaskSpec,
    home: &Path,
    ui: &mut dyn Ui,
    cancel: &CancellationToken,
) -> MemoLookup {
    let key = match spec.memo_key(&agent.cwd, &model_id(agent)) {
        Ok(Some(key)) => key,
        Ok(None) => return MemoLookup::Off(None),
        Err(notice) => return MemoLookup::Off(Some(notice)),
    };
    let store = MemoStore::for_workspace(home, &agent.cwd);
    if let Some((value, evidence)) = store.get_with::<Value>(&key) {
        let replay = Replay {
            evidence: &evidence,
            fingerprint: key.fingerprint(),
        };
        if let Some(judges) = agent.recheck(spec, &value, Some(&replay), ui, cancel).await {
            // A judge that ran again (another judge model, say) and passed
            // is recorded, so the next hit reuses it too.
            let mut updated = evidence.clone();
            for (rubric, model) in &judges {
                updated.add_judge(rubric, model, &value, key.fingerprint());
            }
            if updated != evidence {
                let _ = store.put_with(&key, &value, &updated);
            }
            return MemoLookup::Hit(value);
        }
        store.invalidate(&key);
    }
    MemoLookup::Miss(key)
}

/// Cache a validated result under `key` in `home`'s cache for `root`, with
/// what the run that produced it recorded ([`Agent::take_memo_evidence`]):
/// the touched files' hashes and the judges that passed it.
pub fn memo_put(
    home: &Path,
    root: &Path,
    key: &MemoKey,
    value: &Value,
    evidence: Option<MemoEvidence>,
) -> std::io::Result<()> {
    let evidence = evidence
        .map(|e| e.seal(value, key.fingerprint()))
        .unwrap_or_default();
    MemoStore::for_workspace(home, root).put_with(key, value, &evidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> TaskOpts {
        TaskOpts::default()
    }

    #[test]
    fn flags_build_a_spec() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!opts().is_task());
        assert!(opts().spec("x", dir.path()).unwrap().is_none());

        std::fs::write(
            dir.path().join("s.json"),
            r#"{"type":"object","properties":{"n":{"type":"integer"}}}"#,
        )
        .unwrap();
        // `build` needs a build system to run.
        let build_only = TaskOpts {
            validate: vec!["build".into()],
            ..opts()
        };
        let err = build_only.spec("g", dir.path()).unwrap_err().to_string();
        assert!(err.contains("no build system found"), "{err}");
        std::fs::write(dir.path().join("Makefile"), "all:\n\ttrue\n").unwrap();
        let o = TaskOpts {
            schema: Some("s.json".into()),
            validate: vec!["build".into(), "cmd:true".into()],
            budget: Some("syntax=2,steps=9".into()),
            ..opts()
        };
        let spec = o.spec(" count it ", dir.path()).unwrap().unwrap();
        assert_eq!(spec.goal, "count it");
        assert_eq!(
            spec.schema.as_ref().unwrap()["properties"]["n"]["type"],
            "integer"
        );
        assert_eq!(
            spec.validators,
            vec![ValidatorSpec::Build, ValidatorSpec::Cmd("true".into())]
        );
        assert_eq!((spec.limits.syntax, spec.limits.steps), (2, 9));
        assert!(spec.headless && spec.memo.is_none());

        // Inline schemas; checks alone get the summary schema.
        let inline = TaskOpts {
            schema: Some(r#" {"type":"object"}"#.into()),
            ..opts()
        };
        assert_eq!(
            inline.spec("g", dir.path()).unwrap().unwrap().schema,
            Some(json!({"type": "object"}))
        );
        let checks = TaskOpts {
            validate: vec!["cmd:true".into()],
            ..opts()
        };
        assert_eq!(
            checks.spec("g", dir.path()).unwrap().unwrap().schema,
            Some(default_schema())
        );

        for bad in [
            TaskOpts {
                schema: Some("missing.json".into()),
                ..opts()
            },
            TaskOpts {
                schema: Some("[1]".into()),
                ..opts()
            },
            TaskOpts {
                schema: Some("{nope".into()),
                ..opts()
            },
            TaskOpts {
                validate: vec!["what".into()],
                ..opts()
            },
            TaskOpts {
                budget: Some("steps=x".into()),
                ..opts()
            },
        ] {
            assert!(bad.spec("g", dir.path()).is_err());
        }
    }

    /// A test agent in `cwd` (its model id is `fake/fake`).
    fn memo_agent(cwd: &Path) -> Agent {
        let mut agent = crate::agent::test_support::test_agent();
        agent.cwd = cwd.to_path_buf();
        agent
    }

    async fn lookup(agent: &mut Agent, spec: &TaskSpec, home: &Path) -> MemoLookup {
        let mut ui = crate::agent::test_support::QuietUi;
        memo_lookup(agent, spec, home, &mut ui, &CancellationToken::new()).await
    }

    #[tokio::test]
    async fn memo_hits_until_an_input_changes() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("in.txt"), "v1").unwrap();
        let o = TaskOpts {
            schema: Some(r#"{"type":"object"}"#.into()),
            memo: true,
            memo_file: vec!["in.txt".into()],
            ..opts()
        };
        let spec = o.spec("summarise in.txt", dir.path()).unwrap().unwrap();
        let mut agent = memo_agent(dir.path());
        let MemoLookup::Miss(key) = lookup(&mut agent, &spec, home.path()).await else {
            panic!("a first lookup misses");
        };
        memo_put(home.path(), dir.path(), &key, &json!({"s": 1}), None).unwrap();
        assert_eq!(
            lookup(&mut agent, &spec, home.path()).await,
            MemoLookup::Hit(json!({"s": 1}))
        );
        // Another check list is another task.
        let mut other = spec.clone();
        other.validators.push(ValidatorSpec::Cmd("true".into()));
        assert!(matches!(
            lookup(&mut agent, &other, home.path()).await,
            MemoLookup::Miss(_)
        ));
        // So is another model.
        let model = spec.memo_key(dir.path(), "fake/fake").unwrap().unwrap();
        assert_eq!(model, key);
        assert_ne!(
            spec.memo_key(dir.path(), "fake/other").unwrap().unwrap(),
            key
        );
        // A changed input file misses.
        std::fs::write(dir.path().join("in.txt"), "v2").unwrap();
        assert!(matches!(
            lookup(&mut agent, &spec, home.path()).await,
            MemoLookup::Miss(_)
        ));
        // Without --memo nothing is looked up.
        let plain = TaskOpts {
            schema: Some(r#"{"type":"object"}"#.into()),
            ..opts()
        }
        .spec("summarise in.txt", dir.path())
        .unwrap()
        .unwrap();
        assert_eq!(
            lookup(&mut agent, &plain, home.path()).await,
            MemoLookup::Off(None)
        );
    }

    #[tokio::test]
    async fn memo_without_files_or_git_is_off_with_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let spec = TaskOpts {
            schema: Some(r#"{"type":"object"}"#.into()),
            memo: true,
            ..opts()
        }
        .spec("g", dir.path())
        .unwrap()
        .unwrap();
        let mut agent = memo_agent(dir.path());
        match lookup(&mut agent, &spec, home.path()).await {
            MemoLookup::Off(Some(notice)) => assert!(notice.contains("not in a git repository")),
            other => panic!("{other:?}"),
        }
        // In a repository the git state keys it, and a change misses.
        let repo = crate::worktree::tests::repo();
        let mut agent = memo_agent(repo.path());
        let MemoLookup::Miss(key) = lookup(&mut agent, &spec, home.path()).await else {
            panic!("a first lookup misses");
        };
        memo_put(home.path(), repo.path(), &key, &json!({"ok": true}), None).unwrap();
        assert_eq!(
            lookup(&mut agent, &spec, home.path()).await,
            MemoLookup::Hit(json!({"ok": true}))
        );
        std::fs::write(repo.path().join("a.txt"), "changed\n").unwrap();
        assert!(matches!(
            lookup(&mut agent, &spec, home.path()).await,
            MemoLookup::Miss(_)
        ));
    }

    /// A hit runs the task's checks again: a cached value they now reject
    /// is a miss, and is dropped from the cache.
    #[tokio::test]
    async fn a_cached_result_that_fails_its_checks_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("in.txt"), "v1").unwrap();
        std::fs::write(dir.path().join("report.md"), "done").unwrap();
        let spec = TaskOpts {
            schema: Some(
                r#"{"type":"object","properties":{"file":{"type":"string"}},"required":["file"]}"#
                    .into(),
            ),
            validate: vec!["exists:/file".into()],
            memo: true,
            memo_file: vec!["in.txt".into()],
            ..opts()
        }
        .spec("write the report", dir.path())
        .unwrap()
        .unwrap();
        let mut agent = memo_agent(dir.path());
        let MemoLookup::Miss(key) = lookup(&mut agent, &spec, home.path()).await else {
            panic!("a first lookup misses");
        };
        memo_put(
            home.path(),
            dir.path(),
            &key,
            &json!({"file": "report.md"}),
            None,
        )
        .unwrap();
        assert_eq!(
            lookup(&mut agent, &spec, home.path()).await,
            MemoLookup::Hit(json!({"file": "report.md"}))
        );
        // The file the result names is gone (not a --memo-file, so the key
        // is unchanged): the `exists` check fails and it is a miss.
        std::fs::remove_file(dir.path().join("report.md")).unwrap();
        assert_eq!(
            lookup(&mut agent, &spec, home.path()).await,
            MemoLookup::Miss(key.clone())
        );
        assert!(
            MemoStore::for_workspace(home.path(), dir.path())
                .get::<Value>(&key)
                .is_none(),
            "the failing entry was dropped"
        );
        // A value that does not match the schema is a miss too.
        memo_put(home.path(), dir.path(), &key, &json!({"nope": 1}), None).unwrap();
        assert!(matches!(
            lookup(&mut agent, &spec, home.path()).await,
            MemoLookup::Miss(_)
        ));
        // A `cmd:` check runs again as well.
        let mut failing = spec.clone();
        failing.validators = vec![ValidatorSpec::Cmd("false".into())];
        let MemoLookup::Miss(fkey) = lookup(&mut agent, &failing, home.path()).await else {
            panic!("a first lookup misses");
        };
        memo_put(home.path(), dir.path(), &fkey, &json!({"file": "x"}), None).unwrap();
        assert!(matches!(
            lookup(&mut agent, &failing, home.path()).await,
            MemoLookup::Miss(_)
        ));
    }
}
