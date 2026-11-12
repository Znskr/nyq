//! Headless typed tasks: `nyq -p "<prompt>" --schema <file|json>` with
//! `--validate`, `--budget` and `--memo`.
//!
//! The run prints the validated JSON on stdout and exits 0; a budget that
//! runs out or an escalation exits 2 with a JSON report on stderr; anything
//! else that goes wrong exits 1.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::budget::BudgetLimits;
use super::engine::{parse_budget, MemoSpec, TaskSpec, ValidatorSpec};
use super::memo::MemoStore;

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
    /// Reuse the cached result of an identical earlier task (same prompt, schema, checks and --memo-file contents) (implies -p).
    #[arg(long)]
    pub memo: bool,
    /// A file whose contents are part of the --memo key. Repeatable.
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

/// The cached result for this task, if it has `--memo` and an entry.
pub fn memo_get(spec: &TaskSpec, root: &Path) -> Option<Value> {
    let key = spec.memo_key(root)?;
    MemoStore::new(root).get(&key)
}

/// Cache a validated result (no-op without `--memo`).
pub fn memo_put(spec: &TaskSpec, root: &Path, value: &Value) -> std::io::Result<()> {
    match spec.memo_key(root) {
        Some(key) => MemoStore::new(root).put(&key, value),
        None => Ok(()),
    }
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

    #[test]
    fn memo_hits_until_an_input_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("in.txt"), "v1").unwrap();
        let o = TaskOpts {
            schema: Some(r#"{"type":"object"}"#.into()),
            memo: true,
            memo_file: vec!["in.txt".into()],
            ..opts()
        };
        let spec = o.spec("summarise in.txt", dir.path()).unwrap().unwrap();
        assert_eq!(memo_get(&spec, dir.path()), None);
        memo_put(&spec, dir.path(), &json!({"s": 1})).unwrap();
        assert_eq!(memo_get(&spec, dir.path()), Some(json!({"s": 1})));
        // Another check list is another task.
        let mut other = spec.clone();
        other.validators.push(ValidatorSpec::Build);
        assert_eq!(memo_get(&other, dir.path()), None);
        // A changed input file misses.
        std::fs::write(dir.path().join("in.txt"), "v2").unwrap();
        assert_eq!(memo_get(&spec, dir.path()), None);
        // Without --memo nothing is stored or found.
        let plain = TaskOpts {
            schema: Some(r#"{"type":"object"}"#.into()),
            ..opts()
        }
        .spec("summarise in.txt", dir.path())
        .unwrap()
        .unwrap();
        memo_put(&plain, dir.path(), &json!(1)).unwrap();
        assert_eq!(memo_get(&plain, dir.path()), None);
    }
}
