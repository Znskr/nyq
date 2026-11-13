//! Validators: checks that run when a task calls its `submit` tool (design §4.2).
//!
//! A [`Validator`] looks at the submitted JSON value and the workspace and
//! answers with a [`Verdict`]. A failure is either *syntax* (the value is
//! the wrong shape) or *semantic* (the shape is fine but the work is not); the
//! budget counts the two separately.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::build::{condense, BuildSystem, Exec, OutcomeKind, TestScope};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailKind {
    /// The value is malformed.
    Syntax,
    /// The value is well formed but the work does not hold up.
    Semantic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail {
        kind: FailKind,
        feedback: String,
    },
    /// The validator cannot decide or the model cannot fix this: go up the
    /// ladder.
    Escalate(String),
}

impl Verdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, Verdict::Pass)
    }

    pub fn syntax(feedback: impl Into<String>) -> Self {
        Verdict::Fail {
            kind: FailKind::Syntax,
            feedback: feedback.into(),
        }
    }

    pub fn semantic(feedback: impl Into<String>) -> Self {
        Verdict::Fail {
            kind: FailKind::Semantic,
            feedback: feedback.into(),
        }
    }
}

/// What validators can look at besides the submitted value.
#[derive(Clone)]
pub struct ValidateCx {
    pub root: PathBuf,
    pub build_system: Option<Arc<dyn BuildSystem>>,
    pub exec: Exec,
    /// Files this task changed so far, as relative or absolute paths.
    pub touched: Vec<PathBuf>,
}

impl ValidateCx {
    pub fn new(root: impl Into<PathBuf>, exec: Exec) -> Self {
        Self {
            root: root.into(),
            build_system: None,
            exec,
            touched: Vec::new(),
        }
    }

    /// Detect the build system from the root directory.
    pub fn detect(root: impl Into<PathBuf>, exec: Exec) -> Self {
        let root = root.into();
        let build_system = super::build::detect_build_system(&root).map(Arc::from);
        Self {
            root,
            build_system,
            exec,
            touched: Vec::new(),
        }
    }

    pub fn with_build_system(mut self, bs: Arc<dyn BuildSystem>) -> Self {
        self.build_system = Some(bs);
        self
    }

    pub fn with_touched(mut self, touched: Vec<PathBuf>) -> Self {
        self.touched = touched;
        self
    }
}

#[async_trait]
pub trait Validator: Send + Sync {
    async fn check(&self, value: &Value, cx: &ValidateCx) -> Verdict;
    fn name(&self) -> &str;
}

/// Passes everything. A plain chat turn is `Task<Reply>` with this.
pub struct AlwaysPass;

#[async_trait]
impl Validator for AlwaysPass {
    async fn check(&self, _value: &Value, _cx: &ValidateCx) -> Verdict {
        Verdict::Pass
    }
    fn name(&self) -> &str {
        "always_pass"
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const MAX_SCHEMA_ERRORS: usize = 10;
const MAX_SCHEMA_DEPTH: usize = 32;

/// Checks a value against a small JSON Schema subset: `type` (a name or a
/// list), `required`, `properties`, `items`, `enum`, `const`,
/// `additionalProperties: false`, plus `$ref` into `#/definitions` or
/// `#/$defs` and `anyOf`/`oneOf`, which schemars emits for options and enums.
/// Every problem is reported with its path, in the style of promptyped's
/// ShapeChecker: "missing field `x` at `.items[2]`".
pub struct SchemaValidator {
    schema: Value,
}

impl SchemaValidator {
    pub fn new(schema: Value) -> Self {
        Self { schema }
    }

    /// Every problem found, in document order, capped at ten.
    pub fn errors(&self, value: &Value) -> Vec<String> {
        let mut errs = Vec::new();
        check_schema(&self.schema, &self.schema, value, "", &mut errs, 0);
        errs
    }
}

#[async_trait]
impl Validator for SchemaValidator {
    async fn check(&self, value: &Value, _cx: &ValidateCx) -> Verdict {
        let errs = self.errors(value);
        if errs.is_empty() {
            return Verdict::Pass;
        }
        let mut text = String::from("The submitted value does not match the required schema:");
        for e in errs.iter().take(MAX_SCHEMA_ERRORS) {
            text.push_str("\n- ");
            text.push_str(e);
        }
        if errs.len() > MAX_SCHEMA_ERRORS {
            text.push_str(&format!(
                "\n- … and {} more",
                errs.len() - MAX_SCHEMA_ERRORS
            ));
        }
        Verdict::syntax(text)
    }
    fn name(&self) -> &str {
        "schema"
    }
}

fn show(path: &str) -> String {
    if path.is_empty() {
        "`.`".into()
    } else {
        format!("`{path}`")
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_integer(v: &Value) -> bool {
    match v {
        Value::Number(n) => {
            n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => false,
    }
}

fn matches_type(v: &Value, ty: &str) -> bool {
    match ty {
        "integer" => is_integer(v),
        "number" => v.is_number(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "null" => v.is_null(),
        _ => true, // an unknown type name is not ours to reject
    }
}

fn check_schema(
    root: &Value,
    schema: &Value,
    value: &Value,
    path: &str,
    errs: &mut Vec<String>,
    depth: usize,
) {
    if depth > MAX_SCHEMA_DEPTH || errs.len() >= 100 {
        return;
    }
    let Some(obj) = schema.as_object() else {
        return; // `true`, `false` and malformed schemas accept anything
    };

    if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
        if let Some(target) = r.strip_prefix('#').and_then(|p| root.pointer(p)) {
            check_schema(root, target, value, path, errs, depth + 1);
        }
        return;
    }

    for key in ["anyOf", "oneOf"] {
        if let Some(alts) = obj.get(key).and_then(Value::as_array) {
            let mut best: Option<Vec<String>> = None;
            for alt in alts {
                let mut e = Vec::new();
                check_schema(root, alt, value, path, &mut e, depth + 1);
                if e.is_empty() {
                    best = None;
                    break;
                }
                if best.as_ref().is_none_or(|b| e.len() < b.len()) {
                    best = Some(e);
                }
            }
            if let Some(best) = best {
                errs.push(format!(
                    "value at {} matches none of the {} allowed shapes; closest: {}",
                    show(path),
                    alts.len(),
                    best.first().cloned().unwrap_or_default()
                ));
                return;
            }
        }
    }

    if let Some(ty) = obj.get("type") {
        let names: Vec<&str> = match ty {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.is_empty() && !names.iter().any(|t| matches_type(value, t)) {
            errs.push(format!(
                "wrong type at {}: expected {}, found {}",
                show(path),
                names.join(" or "),
                type_name(value)
            ));
            return; // the rest of the schema assumes the type
        }
    }

    if let Some(options) = obj.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            let list: Vec<String> = options.iter().map(Value::to_string).collect();
            errs.push(format!(
                "value `{}` at {} is not one of [{}]",
                value,
                show(path),
                list.join(", ")
            ));
        }
    }
    if let Some(c) = obj.get("const") {
        if c != value {
            errs.push(format!(
                "value `{}` at {} must be `{}`",
                value,
                show(path),
                c
            ));
        }
    }

    if let Value::Object(map) = value {
        if let Some(req) = obj.get("required").and_then(Value::as_array) {
            for name in req.iter().filter_map(Value::as_str) {
                if !map.contains_key(name) {
                    errs.push(format!("missing field `{name}` at {}", show(path)));
                }
            }
        }
        let props = obj.get("properties").and_then(Value::as_object);
        if let Some(props) = props {
            for (k, sub) in props {
                if let Some(v) = map.get(k) {
                    check_schema(root, sub, v, &format!("{path}.{k}"), errs, depth + 1);
                }
            }
        }
        if obj.get("additionalProperties") == Some(&Value::Bool(false)) {
            for k in map.keys() {
                if !props.is_some_and(|p| p.contains_key(k)) {
                    errs.push(format!("unexpected field `{k}` at {}", show(path)));
                }
            }
        }
    }

    if let (Value::Array(items), Some(sub)) = (value, obj.get("items")) {
        for (i, item) in items.iter().enumerate() {
            check_schema(root, sub, item, &format!("{path}[{i}]"), errs, depth + 1);
        }
    }
}

// ---------------------------------------------------------------------------
// Build, tests, lint, format
// ---------------------------------------------------------------------------

fn no_build_system(cx: &ValidateCx, what: &str) -> Verdict {
    Verdict::Escalate(format!(
        "{what} needs a build system, but none was detected in {}.",
        cx.root.display()
    ))
}

/// Map a check outcome to a verdict: pass or skipped passes, a cancellation
/// escalates, anything else fails semantically with the condensed summary.
fn verdict_of(o: super::build::CheckOutcome) -> Verdict {
    match o.kind {
        OutcomeKind::Passed | OutcomeKind::Skipped => Verdict::Pass,
        OutcomeKind::Cancelled => Verdict::Escalate(o.summary),
        OutcomeKind::Failed | OutcomeKind::TimedOut => Verdict::semantic(o.summary),
    }
}

/// The project must compile.
pub struct BuildValidator;

#[async_trait]
impl Validator for BuildValidator {
    async fn check(&self, _value: &Value, cx: &ValidateCx) -> Verdict {
        match &cx.build_system {
            Some(bs) => verdict_of(bs.build(&cx.exec).await),
            None => no_build_system(cx, "BuildValidator"),
        }
    }
    fn name(&self) -> &str {
        "build"
    }
}

/// The tests in `scope` must pass.
pub struct TestsValidator {
    pub scope: TestScope,
}

impl TestsValidator {
    pub fn new(scope: TestScope) -> Self {
        Self { scope }
    }
}

#[async_trait]
impl Validator for TestsValidator {
    async fn check(&self, _value: &Value, cx: &ValidateCx) -> Verdict {
        match &cx.build_system {
            Some(bs) => verdict_of(bs.test(&cx.exec, &self.scope).await),
            None => no_build_system(cx, "TestsValidator"),
        }
    }
    fn name(&self) -> &str {
        "tests"
    }
}

/// The linter must be clean.
pub struct LintValidator;

#[async_trait]
impl Validator for LintValidator {
    async fn check(&self, _value: &Value, cx: &ValidateCx) -> Verdict {
        match &cx.build_system {
            Some(bs) => verdict_of(bs.lint(&cx.exec).await),
            None => no_build_system(cx, "LintValidator"),
        }
    }
    fn name(&self) -> &str {
        "lint"
    }
}

/// The formatter check must be clean.
pub struct FormatValidator;

#[async_trait]
impl Validator for FormatValidator {
    async fn check(&self, _value: &Value, cx: &ValidateCx) -> Verdict {
        match &cx.build_system {
            Some(bs) => verdict_of(bs.format_check(&cx.exec).await),
            None => no_build_system(cx, "FormatValidator"),
        }
    }
    fn name(&self) -> &str {
        "format"
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

fn normalise(root: &Path, p: &Path) -> PathBuf {
    let rel = p.strip_prefix(root).unwrap_or(p);
    rel.components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

/// The task must actually have changed these files (promptyped
/// `validateFileModifiedFine`).
pub struct FilesTouchedValidator {
    pub paths: Vec<PathBuf>,
}

impl FilesTouchedValidator {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }
}

#[async_trait]
impl Validator for FilesTouchedValidator {
    async fn check(&self, _value: &Value, cx: &ValidateCx) -> Verdict {
        let touched: Vec<PathBuf> = cx.touched.iter().map(|p| normalise(&cx.root, p)).collect();
        let missing: Vec<String> = self
            .paths
            .iter()
            .filter(|p| !touched.contains(&normalise(&cx.root, p)))
            .map(|p| p.display().to_string())
            .collect();
        if missing.is_empty() {
            return Verdict::Pass;
        }
        Verdict::semantic(format!(
            "No modifications have been made to {}. If you are confident the change is already present, make a small edit (such as a comment) so the file counts as touched, and submit again.",
            missing.join(", ")
        ))
    }
    fn name(&self) -> &str {
        "files_touched"
    }
}

/// Where an [`ExistsValidator`] gets its paths from.
pub enum PathSource {
    Fixed(Vec<PathBuf>),
    /// A JSON pointer into the submitted value, at a string or an array of
    /// strings, e.g. `/files`.
    Pointer(String),
}

/// Every path must exist: a guard against hallucinated file names (promptyped
/// `validateRelevantFilesExist`).
pub struct ExistsValidator {
    pub source: PathSource,
}

impl ExistsValidator {
    pub fn fixed(paths: Vec<PathBuf>) -> Self {
        Self {
            source: PathSource::Fixed(paths),
        }
    }

    pub fn in_value(pointer: impl Into<String>) -> Self {
        Self {
            source: PathSource::Pointer(pointer.into()),
        }
    }
}

#[async_trait]
impl Validator for ExistsValidator {
    async fn check(&self, value: &Value, cx: &ValidateCx) -> Verdict {
        let paths: Vec<PathBuf> = match &self.source {
            PathSource::Fixed(p) => p.clone(),
            PathSource::Pointer(ptr) => match value.pointer(ptr) {
                Some(Value::String(s)) => vec![PathBuf::from(s)],
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(PathBuf::from)
                    .collect(),
                _ => {
                    return Verdict::semantic(format!(
                        "Expected a path or a list of paths at `{ptr}` in the submitted value."
                    ))
                }
            },
        };
        let missing: Vec<String> = paths
            .iter()
            .filter(|p| !cx.root.join(p).exists())
            .map(|p| p.display().to_string())
            .collect();
        if missing.is_empty() {
            Verdict::Pass
        } else {
            Verdict::semantic(format!(
                "These paths do not exist: {}. Check the spelling and that they are relative to the project root.",
                missing.join(", ")
            ))
        }
    }
    fn name(&self) -> &str {
        "exists"
    }
}

// ---------------------------------------------------------------------------
// Custom command
// ---------------------------------------------------------------------------

/// Runs a shell command; exit code 0 means pass. This is how CI-style checks
/// plug in.
pub struct CustomCommandValidator {
    pub command: String,
}

impl CustomCommandValidator {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
        }
    }
}

#[async_trait]
impl Validator for CustomCommandValidator {
    async fn check(&self, _value: &Value, cx: &ValidateCx) -> Verdict {
        let out = cx
            .exec
            .run(&self.command, &cx.root, cx.exec.test_timeout)
            .await;
        if out.cancelled {
            return Verdict::Escalate(format!("`{}` was cancelled.", self.command));
        }
        if out.timed_out {
            return Verdict::semantic(format!(
                "`{}` timed out after {}s and was terminated.",
                self.command,
                cx.exec.test_timeout.as_secs()
            ));
        }
        if out.success() {
            return Verdict::Pass;
        }
        let code = out
            .exit_code
            .map_or("no exit code".to_string(), |c| format!("exit code {c}"));
        Verdict::semantic(format!(
            "`{}` failed ({code}):\n{}",
            self.command,
            condense(&out.output, |_| false, 1).text
        ))
    }
    fn name(&self) -> &str {
        "custom"
    }
}

// ---------------------------------------------------------------------------
// Combinators
// ---------------------------------------------------------------------------

/// Runs validators in order and stops at the first that does not pass.
pub struct All(pub Vec<Box<dyn Validator>>);

#[async_trait]
impl Validator for All {
    async fn check(&self, value: &Value, cx: &ValidateCx) -> Verdict {
        for v in &self.0 {
            let verdict = v.check(value, cx).await;
            if !verdict.is_pass() {
                return verdict;
            }
        }
        Verdict::Pass
    }
    fn name(&self) -> &str {
        "all"
    }
}

/// Runs every validator against the same value, without stopping, and reports
/// all failures together so the model can fix them in one go. An escalation
/// from any of them wins. (promptyped's `combineValidatorsSameRes` also
/// compared the values the validators returned; here validators do not
/// rewrite the value, so that part has nothing to check.)
pub struct Same(pub Vec<Box<dyn Validator>>);

#[async_trait]
impl Validator for Same {
    async fn check(&self, value: &Value, cx: &ValidateCx) -> Verdict {
        let mut feedback: Vec<String> = Vec::new();
        let mut kind = FailKind::Syntax;
        for v in &self.0 {
            match v.check(value, cx).await {
                Verdict::Pass => {}
                Verdict::Escalate(why) => return Verdict::Escalate(why),
                Verdict::Fail {
                    kind: k,
                    feedback: f,
                } => {
                    if k == FailKind::Semantic {
                        kind = FailKind::Semantic;
                    }
                    feedback.push(format!("[{}] {f}", v.name()));
                }
            }
        }
        if feedback.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Fail {
                kind,
                feedback: feedback.join("\n\n"),
            }
        }
    }
    fn name(&self) -> &str {
        "same"
    }
}

// ---------------------------------------------------------------------------
// Judge (stub)
// ---------------------------------------------------------------------------

/// What a judge decided: is the change present and complete?
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JudgeVerdict {
    pub complete: bool,
    pub feedback: String,
}

/// Injection point for the LLM judge. The wiring stage implements this with a
/// read-only child `Task<JudgeVerdict>` at the rubric's tier. An `Err` means
/// the judge itself could not run.
#[async_trait]
pub trait JudgeRunner: Send + Sync {
    async fn judge(
        &self,
        rubric: &str,
        value: &Value,
        cx: &ValidateCx,
    ) -> Result<JudgeVerdict, String>;

    /// The model that judges (`provider/model`), which a `--memo` replay
    /// compares before it reuses a cached pass. None never reuses one.
    fn model_id(&self) -> Option<String> {
        None
    }
}

/// An LLM checks that the change is present and complete (not that it is
/// correct; the build and the tests do that).
pub struct JudgeValidator {
    pub rubric: String,
    runner: Arc<dyn JudgeRunner>,
}

impl JudgeValidator {
    pub fn new(rubric: impl Into<String>, runner: Arc<dyn JudgeRunner>) -> Self {
        Self {
            rubric: rubric.into(),
            runner,
        }
    }
}

#[async_trait]
impl Validator for JudgeValidator {
    async fn check(&self, value: &Value, cx: &ValidateCx) -> Verdict {
        match self.runner.judge(&self.rubric, value, cx).await {
            Ok(v) if v.complete => Verdict::Pass,
            Ok(v) => Verdict::semantic(v.feedback),
            Err(e) => Verdict::Escalate(format!("judge could not run: {e}")),
        }
    }
    fn name(&self) -> &str {
        "judge"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::build::{CommandOutput, FakeRunner};
    use serde_json::json;
    use std::sync::Mutex;

    fn schema_errors(schema: Value, value: Value) -> Vec<String> {
        SchemaValidator::new(schema).errors(&value)
    }

    fn items_schema() -> Value {
        json!({
            "type": "object",
            "required": ["items"],
            "additionalProperties": false,
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["name", "n"],
                        "properties": {
                            "name": {"type": "string"},
                            "n": {"type": "integer"},
                            "kind": {"enum": ["a", "b"]}
                        }
                    }
                }
            }
        })
    }

    // --- schema ---

    #[test]
    fn schema_accepts_a_good_value() {
        let v = json!({"items": [{"name": "x", "n": 1, "kind": "a"}, {"name": "y", "n": 2}]});
        assert!(schema_errors(items_schema(), v).is_empty());
    }

    #[test]
    fn schema_missing_field_with_precise_path() {
        let v = json!({"items": [{"name": "x", "n": 1}, {"name": "y", "n": 2}, {"name": "z"}]});
        assert_eq!(
            schema_errors(items_schema(), v),
            vec!["missing field `n` at `.items[2]`"]
        );
    }

    #[test]
    fn schema_missing_field_at_root() {
        assert_eq!(
            schema_errors(items_schema(), json!({})),
            vec!["missing field `items` at `.`"]
        );
    }

    #[test]
    fn schema_wrong_type_with_path() {
        let v = json!({"items": [{"name": 5, "n": 1}]});
        assert_eq!(
            schema_errors(items_schema(), v),
            vec!["wrong type at `.items[0].name`: expected string, found number"]
        );
        assert_eq!(
            schema_errors(items_schema(), json!({"items": "nope"})),
            vec!["wrong type at `.items`: expected array, found string"]
        );
        assert_eq!(
            schema_errors(items_schema(), json!([1])),
            vec!["wrong type at `.`: expected object, found array"]
        );
    }

    #[test]
    fn schema_integer_vs_number() {
        let s = json!({"type": "integer"});
        assert!(schema_errors(s.clone(), json!(3)).is_empty());
        assert!(schema_errors(s.clone(), json!(3.0)).is_empty());
        assert_eq!(
            schema_errors(s, json!(3.5)),
            vec!["wrong type at `.`: expected integer, found number"]
        );
        assert!(schema_errors(json!({"type": "number"}), json!(3.5)).is_empty());
    }

    #[test]
    fn schema_enum_and_const() {
        let v = json!({"items": [{"name": "x", "n": 1, "kind": "z"}]});
        assert_eq!(
            schema_errors(items_schema(), v),
            vec!["value `\"z\"` at `.items[0].kind` is not one of [\"a\", \"b\"]"]
        );
        let e = schema_errors(json!({"const": "v1"}), json!("v2"));
        assert_eq!(e, vec!["value `\"v2\"` at `.` must be `\"v1\"`"]);
    }

    #[test]
    fn schema_additional_properties_false() {
        let e = schema_errors(items_schema(), json!({"items": [], "extra": 1, "more": 2}));
        assert_eq!(
            e,
            vec![
                "unexpected field `extra` at `.`".to_string(),
                "unexpected field `more` at `.`".to_string()
            ]
        );
        // Without the flag, extra fields are fine.
        let s = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        assert!(schema_errors(s, json!({"a": "x", "b": 1})).is_empty());
    }

    #[test]
    fn schema_reports_every_problem() {
        let v = json!({"items": [{"n": "x"}, {"name": 1, "n": 2}], "zzz": 1});
        let e = schema_errors(items_schema(), v);
        assert_eq!(e.len(), 4, "{e:?}");
        assert!(e.contains(&"missing field `name` at `.items[0]`".to_string()));
        assert!(
            e.contains(&"wrong type at `.items[0].n`: expected integer, found string".to_string())
        );
        assert!(e.contains(
            &"wrong type at `.items[1].name`: expected string, found number".to_string()
        ));
        assert!(e.contains(&"unexpected field `zzz` at `.`".to_string()));
    }

    #[test]
    fn schema_type_list_and_null() {
        let s = json!({"type": ["string", "null"]});
        assert!(schema_errors(s.clone(), json!(null)).is_empty());
        assert!(schema_errors(s.clone(), json!("x")).is_empty());
        assert_eq!(
            schema_errors(s, json!(1)),
            vec!["wrong type at `.`: expected string or null, found number"]
        );
    }

    #[test]
    fn schema_refs_and_any_of_as_schemars_emits() {
        let s = json!({
            "type": "object",
            "required": ["step", "note"],
            "properties": {
                "step": {"$ref": "#/definitions/Step"},
                "note": {"anyOf": [{"type": "string"}, {"type": "null"}]}
            },
            "definitions": {
                "Step": {"type": "object", "required": ["id"], "properties": {"id": {"type": "integer"}}}
            }
        });
        assert!(schema_errors(s.clone(), json!({"step": {"id": 1}, "note": null})).is_empty());
        assert!(schema_errors(s.clone(), json!({"step": {"id": 1}, "note": "hi"})).is_empty());
        assert_eq!(
            schema_errors(s.clone(), json!({"step": {}, "note": null})),
            vec!["missing field `id` at `.step`"]
        );
        let e = schema_errors(s, json!({"step": {"id": 1}, "note": 5}));
        assert_eq!(e.len(), 1);
        assert!(
            e[0].starts_with("value at `.note` matches none of the 2 allowed shapes"),
            "{e:?}"
        );
    }

    #[test]
    fn schema_ref_cycle_terminates() {
        let s = json!({"$defs": {"T": {"$ref": "#/$defs/T"}}, "$ref": "#/$defs/T"});
        assert!(schema_errors(s, json!(1)).is_empty());
        // A recursive but well-founded schema works.
        let tree = json!({
            "$defs": {"Node": {"type": "object", "properties": {"kids": {"type": "array", "items": {"$ref": "#/$defs/Node"}}}, "required": ["v"]}},
            "$ref": "#/$defs/Node"
        });
        let e = schema_errors(tree, json!({"v": 1, "kids": [{"v": 2, "kids": [{}]}]}));
        assert_eq!(e, vec!["missing field `v` at `.kids[0].kids[0]`"]);
    }

    #[test]
    fn schema_dangling_ref_and_odd_schemas_accept() {
        assert!(schema_errors(json!({"$ref": "#/nope"}), json!(1)).is_empty());
        assert!(schema_errors(json!(true), json!(1)).is_empty());
        assert!(schema_errors(json!({}), json!({"any": "thing"})).is_empty());
        assert!(schema_errors(json!({"type": "weird"}), json!(1)).is_empty());
    }

    fn fake_cx() -> (tempfile::TempDir, Arc<FakeRunner>, ValidateCx) {
        let d = tempfile::tempdir().unwrap();
        let f = FakeRunner::new();
        let cx = ValidateCx::new(d.path(), Exec::new(f.clone()));
        (d, f, cx)
    }

    fn cargo_cx(f: &Arc<FakeRunner>) -> (tempfile::TempDir, ValidateCx) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("Cargo.toml"), "").unwrap();
        let cx = ValidateCx::detect(d.path(), Exec::new(f.clone()));
        assert!(cx.build_system.is_some());
        (d, cx)
    }

    #[tokio::test]
    async fn schema_validator_verdicts() {
        let (_d, _f, cx) = fake_cx();
        let v = SchemaValidator::new(items_schema());
        assert_eq!(v.name(), "schema");
        assert_eq!(v.check(&json!({"items": []}), &cx).await, Verdict::Pass);
        let verdict = v.check(&json!({"items": [{"name": "x"}]}), &cx).await;
        match verdict {
            Verdict::Fail { kind, feedback } => {
                assert_eq!(kind, FailKind::Syntax);
                assert!(feedback.contains("does not match the required schema"));
                assert!(
                    feedback.contains("- missing field `n` at `.items[0]`"),
                    "{feedback}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn schema_validator_caps_the_feedback() {
        let (_d, _f, cx) = fake_cx();
        let v = SchemaValidator::new(json!({"type": "array", "items": {"type": "string"}}));
        let value = Value::Array((0..25).map(|i| json!(i)).collect());
        let Verdict::Fail { feedback, .. } = v.check(&value, &cx).await else {
            panic!()
        };
        assert!(feedback.contains("… and 15 more"), "{feedback}");
        assert_eq!(feedback.matches("\n- ").count(), 11);
    }

    // --- build, tests, lint, format ---

    #[tokio::test]
    async fn build_validator() {
        let f = FakeRunner::new().on("cargo build", vec![CommandOutput::ok("fine")]);
        let (_d, cx) = cargo_cx(&f);
        assert_eq!(BuildValidator.check(&json!({}), &cx).await, Verdict::Pass);

        let f = FakeRunner::new().on(
            "cargo build",
            vec![CommandOutput::failed(
                101,
                "error[E0308]: mismatched types\n --> src/lib.rs:1:1\n",
            )],
        );
        let (_d, cx) = cargo_cx(&f);
        let Verdict::Fail { kind, feedback } = BuildValidator.check(&json!({}), &cx).await else {
            panic!()
        };
        assert_eq!(kind, FailKind::Semantic);
        assert!(feedback.contains("E0308"));
    }

    #[tokio::test]
    async fn build_validator_without_a_build_system_escalates() {
        let (_d, _f, cx) = fake_cx();
        for v in [
            Box::new(BuildValidator) as Box<dyn Validator>,
            Box::new(TestsValidator::new(TestScope::All)),
            Box::new(LintValidator),
            Box::new(FormatValidator),
        ] {
            let verdict = v.check(&json!({}), &cx).await;
            assert!(
                matches!(&verdict, Verdict::Escalate(m) if m.contains("none was detected")),
                "{} {verdict:?}",
                v.name()
            );
        }
    }

    #[tokio::test]
    async fn tests_validator_passes_and_fails() {
        let f = FakeRunner::new().on("cargo test", vec![CommandOutput::ok("ok")]);
        let (_d, cx) = cargo_cx(&f);
        let v = TestsValidator::new(TestScope::All);
        assert_eq!(v.check(&json!({}), &cx).await, Verdict::Pass);

        let f = FakeRunner::new().on(
            "cargo test",
            vec![CommandOutput::failed(101, "test a::bad ... FAILED\n")],
        );
        let (_d, cx) = cargo_cx(&f);
        let Verdict::Fail { kind, feedback } = v.check(&json!({}), &cx).await else {
            panic!()
        };
        assert_eq!(kind, FailKind::Semantic);
        assert!(feedback.contains("1 failing test"));
    }

    #[tokio::test]
    async fn tests_validator_uses_its_scope() {
        let f = FakeRunner::new().on("cargo test", vec![CommandOutput::ok("ok")]);
        let (_d, cx) = cargo_cx(&f);
        TestsValidator::new(TestScope::Test("a::b".into()))
            .check(&json!({}), &cx)
            .await;
        assert!(f.calls()[0].contains("a::b -- --exact"));
    }

    #[tokio::test]
    async fn timeout_fails_cancel_escalates_skip_passes() {
        let f = FakeRunner::new().on("cargo build", vec![CommandOutput::timeout("slow")]);
        let (_d, cx) = cargo_cx(&f);
        let v = BuildValidator.check(&json!({}), &cx).await;
        assert!(
            matches!(&v, Verdict::Fail { kind: FailKind::Semantic, feedback } if feedback.contains("timed out")),
            "{v:?}"
        );

        let f = FakeRunner::new().on("cargo build", vec![CommandOutput::cancelled()]);
        let (_d, cx) = cargo_cx(&f);
        assert!(matches!(
            BuildValidator.check(&json!({}), &cx).await,
            Verdict::Escalate(_)
        ));

        // A Makefile with no lint target: lint is skipped, so it passes.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("Makefile"), "all:\n\ttrue\n").unwrap();
        let cx = ValidateCx::detect(d.path(), Exec::new(FakeRunner::new()));
        assert_eq!(LintValidator.check(&json!({}), &cx).await, Verdict::Pass);
    }

    #[tokio::test]
    async fn lint_and_format_validators() {
        let f = FakeRunner::new()
            .on(
                "clippy",
                vec![CommandOutput::failed(101, "error: unused\n")],
            )
            .on("fmt", vec![CommandOutput::ok("")]);
        let (_d, cx) = cargo_cx(&f);
        assert!(matches!(
            LintValidator.check(&json!({}), &cx).await,
            Verdict::Fail { .. }
        ));
        assert_eq!(FormatValidator.check(&json!({}), &cx).await, Verdict::Pass);
    }

    // --- files ---

    #[tokio::test]
    async fn files_touched() {
        let (d, _f, cx) = fake_cx();
        let root = d.path().to_path_buf();
        let v =
            FilesTouchedValidator::new(vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")]);
        let Verdict::Fail { kind, feedback } = v.check(&json!({}), &cx).await else {
            panic!()
        };
        assert_eq!(kind, FailKind::Semantic);
        assert!(feedback.contains("src/a.rs, src/b.rs"), "{feedback}");

        // Relative, `./`-prefixed and absolute paths all count.
        let cx = cx.with_touched(vec![PathBuf::from("./src/a.rs"), root.join("src/b.rs")]);
        assert_eq!(v.check(&json!({}), &cx).await, Verdict::Pass);

        let cx = ValidateCx::new(&root, Exec::new(FakeRunner::new()))
            .with_touched(vec![PathBuf::from("src/a.rs")]);
        let Verdict::Fail { feedback, .. } = v.check(&json!({}), &cx).await else {
            panic!()
        };
        assert!(
            feedback.contains("src/b.rs") && !feedback.contains("src/a.rs"),
            "{feedback}"
        );
    }

    #[tokio::test]
    async fn files_touched_with_nothing_required_passes() {
        let (_d, _f, cx) = fake_cx();
        assert_eq!(
            FilesTouchedValidator::new(vec![])
                .check(&json!({}), &cx)
                .await,
            Verdict::Pass
        );
    }

    #[tokio::test]
    async fn exists_fixed() {
        let (d, _f, cx) = fake_cx();
        std::fs::write(d.path().join("real.rs"), "").unwrap();
        let ok = ExistsValidator::fixed(vec![PathBuf::from("real.rs")]);
        assert_eq!(ok.check(&json!({}), &cx).await, Verdict::Pass);
        let bad = ExistsValidator::fixed(vec![PathBuf::from("real.rs"), PathBuf::from("ghost.rs")]);
        let Verdict::Fail { kind, feedback } = bad.check(&json!({}), &cx).await else {
            panic!()
        };
        assert_eq!(kind, FailKind::Semantic);
        assert!(
            feedback.contains("ghost.rs") && !feedback.contains("real.rs"),
            "{feedback}"
        );
    }

    #[tokio::test]
    async fn exists_in_value() {
        let (d, _f, cx) = fake_cx();
        std::fs::write(d.path().join("real.rs"), "").unwrap();
        let v = ExistsValidator::in_value("/files");
        assert_eq!(
            v.check(&json!({"files": ["real.rs"]}), &cx).await,
            Verdict::Pass
        );
        assert_eq!(
            v.check(&json!({"files": "real.rs"}), &cx).await,
            Verdict::Pass
        );
        let Verdict::Fail { feedback, .. } = v
            .check(&json!({"files": ["real.rs", "made/up.rs"]}), &cx)
            .await
        else {
            panic!()
        };
        assert!(feedback.contains("made/up.rs"));
        let Verdict::Fail { feedback, .. } = v.check(&json!({"other": 1}), &cx).await else {
            panic!()
        };
        assert!(feedback.contains("`/files`"));
    }

    // --- custom command ---

    #[tokio::test]
    async fn custom_command() {
        let (_d, f, cx) = fake_cx();
        let f = f
            .on("./ci.sh pass", vec![CommandOutput::ok("all good")])
            .on(
                "./ci.sh fail",
                vec![CommandOutput::failed(2, "line one\nthe check failed")],
            )
            .on("./ci.sh slow", vec![CommandOutput::timeout("")])
            .on("./ci.sh cancel", vec![CommandOutput::cancelled()]);
        assert_eq!(
            CustomCommandValidator::new("./ci.sh pass")
                .check(&json!({}), &cx)
                .await,
            Verdict::Pass
        );
        let Verdict::Fail { kind, feedback } = CustomCommandValidator::new("./ci.sh fail")
            .check(&json!({}), &cx)
            .await
        else {
            panic!()
        };
        assert_eq!(kind, FailKind::Semantic);
        assert!(
            feedback.contains("exit code 2") && feedback.contains("the check failed"),
            "{feedback}"
        );
        let v = CustomCommandValidator::new("./ci.sh slow")
            .check(&json!({}), &cx)
            .await;
        assert!(
            matches!(&v, Verdict::Fail { feedback, .. } if feedback.contains("timed out")),
            "{v:?}"
        );
        assert!(matches!(
            CustomCommandValidator::new("./ci.sh cancel")
                .check(&json!({}), &cx)
                .await,
            Verdict::Escalate(_)
        ));
        assert_eq!(f.calls().len(), 4);
    }

    #[tokio::test]
    async fn custom_command_runs_in_the_root() {
        struct Cwd(Mutex<Option<PathBuf>>);
        #[async_trait]
        impl crate::task::build::CommandRunner for Cwd {
            async fn run(&self, spec: &crate::task::build::CommandSpec) -> CommandOutput {
                *self.0.lock().unwrap() = Some(spec.cwd.clone());
                CommandOutput::ok("")
            }
        }
        let d = tempfile::tempdir().unwrap();
        let r = Arc::new(Cwd(Mutex::new(None)));
        let cx = ValidateCx::new(d.path(), Exec::new(r.clone()));
        CustomCommandValidator::new("true")
            .check(&json!({}), &cx)
            .await;
        assert_eq!(r.0.lock().unwrap().as_deref(), Some(d.path()));
    }

    // --- combinators ---

    struct Stub {
        verdict: Verdict,
        name: &'static str,
        calls: Calls,
    }

    #[async_trait]
    impl Validator for Stub {
        async fn check(&self, _v: &Value, _cx: &ValidateCx) -> Verdict {
            self.calls.lock().unwrap().push(self.name);
            self.verdict.clone()
        }
        fn name(&self) -> &str {
            self.name
        }
    }

    type Calls = Arc<Mutex<Vec<&'static str>>>;

    fn stubs(list: Vec<(&'static str, Verdict)>) -> (Vec<Box<dyn Validator>>, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let v = list
            .into_iter()
            .map(|(name, verdict)| {
                Box::new(Stub {
                    verdict,
                    name,
                    calls: calls.clone(),
                }) as Box<dyn Validator>
            })
            .collect();
        (v, calls)
    }

    #[tokio::test]
    async fn all_stops_at_the_first_failure() {
        let (_d, _f, cx) = fake_cx();
        let (vs, calls) = stubs(vec![
            ("one", Verdict::Pass),
            ("two", Verdict::semantic("two failed")),
            ("three", Verdict::syntax("never reached")),
        ]);
        let v = All(vs).check(&json!({}), &cx).await;
        assert_eq!(v, Verdict::semantic("two failed"));
        assert_eq!(*calls.lock().unwrap(), vec!["one", "two"]);
    }

    #[tokio::test]
    async fn all_passes_when_everything_passes_and_when_empty() {
        let (_d, _f, cx) = fake_cx();
        let (vs, calls) = stubs(vec![("a", Verdict::Pass), ("b", Verdict::Pass)]);
        assert_eq!(All(vs).check(&json!({}), &cx).await, Verdict::Pass);
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(All(vec![]).check(&json!({}), &cx).await, Verdict::Pass);
    }

    #[tokio::test]
    async fn all_stops_on_escalate() {
        let (_d, _f, cx) = fake_cx();
        let (vs, calls) = stubs(vec![
            ("a", Verdict::Escalate("help".into())),
            ("b", Verdict::Pass),
        ]);
        assert_eq!(
            All(vs).check(&json!({}), &cx).await,
            Verdict::Escalate("help".into())
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn same_runs_everything_and_merges_failures() {
        let (_d, _f, cx) = fake_cx();
        let (vs, calls) = stubs(vec![
            ("shape", Verdict::syntax("bad shape")),
            ("ok", Verdict::Pass),
            ("build", Verdict::semantic("bad build")),
        ]);
        let Verdict::Fail { kind, feedback } = Same(vs).check(&json!({}), &cx).await else {
            panic!()
        };
        assert_eq!(
            kind,
            FailKind::Semantic,
            "any semantic failure makes it semantic"
        );
        assert_eq!(feedback, "[shape] bad shape\n\n[build] bad build");
        assert_eq!(calls.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn same_with_only_syntax_failures_is_syntax() {
        let (_d, _f, cx) = fake_cx();
        let (vs, _) = stubs(vec![("a", Verdict::syntax("x")), ("b", Verdict::Pass)]);
        assert!(matches!(
            Same(vs).check(&json!({}), &cx).await,
            Verdict::Fail {
                kind: FailKind::Syntax,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn same_escalation_wins_and_all_pass_passes() {
        let (_d, _f, cx) = fake_cx();
        let (vs, _) = stubs(vec![
            ("a", Verdict::semantic("x")),
            ("b", Verdict::Escalate("stuck".into())),
        ]);
        assert_eq!(
            Same(vs).check(&json!({}), &cx).await,
            Verdict::Escalate("stuck".into())
        );
        let (vs, _) = stubs(vec![("a", Verdict::Pass)]);
        assert_eq!(Same(vs).check(&json!({}), &cx).await, Verdict::Pass);
    }

    #[tokio::test]
    async fn combinators_nest() {
        let (_d, _f, cx) = fake_cx();
        let (inner, _) = stubs(vec![
            ("a", Verdict::Pass),
            ("b", Verdict::semantic("b bad")),
        ]);
        let (tail, calls) = stubs(vec![("tail", Verdict::Pass)]);
        let mut outer: Vec<Box<dyn Validator>> = vec![Box::new(Same(inner))];
        outer.extend(tail);
        let v = All(outer).check(&json!({}), &cx).await;
        assert_eq!(v, Verdict::semantic("[b] b bad"));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn always_pass() {
        let (_d, _f, cx) = fake_cx();
        assert_eq!(AlwaysPass.check(&json!(null), &cx).await, Verdict::Pass);
        assert_eq!(AlwaysPass.name(), "always_pass");
    }

    #[tokio::test]
    async fn schema_then_build_pipeline() {
        // The usual shape: shape check first, then the expensive checks.
        let f = FakeRunner::new().on("cargo build", vec![CommandOutput::ok("")]);
        let (_d, cx) = cargo_cx(&f);
        let pipeline = All(vec![
            Box::new(SchemaValidator::new(
                json!({"type": "object", "required": ["done"]}),
            )),
            Box::new(BuildValidator),
        ]);
        assert!(matches!(
            pipeline.check(&json!({}), &cx).await,
            Verdict::Fail {
                kind: FailKind::Syntax,
                ..
            }
        ));
        assert!(
            f.calls().is_empty(),
            "the build did not run for a malformed value"
        );
        assert_eq!(
            pipeline.check(&json!({"done": true}), &cx).await,
            Verdict::Pass
        );
        assert_eq!(f.calls().len(), 1);
    }

    // --- judge ---

    struct FakeJudge(Result<JudgeVerdict, String>, Mutex<Vec<(String, Value)>>);

    #[async_trait]
    impl JudgeRunner for FakeJudge {
        async fn judge(
            &self,
            rubric: &str,
            value: &Value,
            _cx: &ValidateCx,
        ) -> Result<JudgeVerdict, String> {
            self.1
                .lock()
                .unwrap()
                .push((rubric.to_string(), value.clone()));
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn judge_validator() {
        let (_d, _f, cx) = fake_cx();
        let complete = Arc::new(FakeJudge(
            Ok(JudgeVerdict {
                complete: true,
                feedback: String::new(),
            }),
            Mutex::new(vec![]),
        ));
        let v = JudgeValidator::new("the retry is implemented", complete.clone());
        assert_eq!(v.check(&json!({"x": 1}), &cx).await, Verdict::Pass);
        assert_eq!(
            complete.1.lock().unwrap()[0],
            ("the retry is implemented".to_string(), json!({"x": 1}))
        );

        let incomplete = Arc::new(FakeJudge(
            Ok(JudgeVerdict {
                complete: false,
                feedback: "the retry is missing".into(),
            }),
            Mutex::new(vec![]),
        ));
        assert_eq!(
            JudgeValidator::new("r", incomplete)
                .check(&json!({}), &cx)
                .await,
            Verdict::semantic("the retry is missing")
        );

        let broken = Arc::new(FakeJudge(Err("no model".into()), Mutex::new(vec![])));
        let v = JudgeValidator::new("r", broken)
            .check(&json!({}), &cx)
            .await;
        assert_eq!(v, Verdict::Escalate("judge could not run: no model".into()));
    }

    #[test]
    fn verdict_helpers() {
        assert!(Verdict::Pass.is_pass());
        assert!(!Verdict::semantic("x").is_pass());
        assert!(!Verdict::Escalate("x".into()).is_pass());
        assert_eq!(
            Verdict::syntax("s"),
            Verdict::Fail {
                kind: FailKind::Syntax,
                feedback: "s".into()
            }
        );
    }

    #[test]
    fn validator_names() {
        assert_eq!(BuildValidator.name(), "build");
        assert_eq!(TestsValidator::new(TestScope::All).name(), "tests");
        assert_eq!(LintValidator.name(), "lint");
        assert_eq!(FormatValidator.name(), "format");
        assert_eq!(FilesTouchedValidator::new(vec![]).name(), "files_touched");
        assert_eq!(ExistsValidator::fixed(vec![]).name(), "exists");
        assert_eq!(CustomCommandValidator::new("x").name(), "custom");
        assert_eq!(All(vec![]).name(), "all");
        assert_eq!(Same(vec![]).name(), "same");
    }
}
