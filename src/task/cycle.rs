//! Loop detection over a sliding window of steps and intents (design §4.3).
//!
//! A step is `(tool, args hash, outcome hash)`. Including the outcome means an
//! edit that changes the result of the next build breaks the pattern, so
//! only repetition that gets nowhere is flagged. The detector looks for
//! period-1, -2 and -3 repetition in the tail of the window, and for the model
//! restating the same intent again and again.

use std::collections::VecDeque;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// One tool call and what came of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub tool: String,
    pub args_hash: u64,
    pub outcome_hash: u64,
}

fn hash64(bytes: &[u8]) -> u64 {
    let digest = Sha256::digest(bytes);
    u64::from_be_bytes(digest[..8].try_into().expect("sha256 has 32 bytes"))
}

/// Compact JSON with object keys sorted, whatever order the map keeps.
pub(super) fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&m[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().map(canonical).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

impl Step {
    /// `args` hashes by its compact JSON text with object keys sorted, so
    /// key order does not matter. `outcome` is the tool's result text and
    /// whether it was an error.
    pub fn new(tool: &str, args: &Value, outcome: &str, is_error: bool) -> Self {
        let mut o = Vec::with_capacity(outcome.len() + 1);
        o.push(is_error as u8);
        o.extend_from_slice(outcome.as_bytes());
        Self {
            tool: tool.to_string(),
            args_hash: hash64(canonical(args).as_bytes()),
            outcome_hash: hash64(&o),
        }
    }
}

/// A loop the detector found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cycle {
    /// The same `period` steps, repeated `repeats` times in a row.
    Repeat {
        period: usize,
        repeats: usize,
        tools: Vec<String>,
    },
    /// The model keeps stating nearly the same intent.
    SimilarIntents { count: usize, sample: String },
}

impl Cycle {
    /// The nudge to inject (ladder rung 1).
    pub fn note(&self) -> String {
        match self {
            Cycle::Repeat {
                period,
                repeats,
                tools,
            } => {
                let what = if *period == 1 {
                    format!("the same `{}` call", tools[0])
                } else {
                    format!("this sequence of calls: {}", tools.join(" → "))
                };
                format!(
                    "You are repeating yourself: {what} has now run {repeats} times with the same result. Repeating it will not change the outcome. Try a different approach, or look at a different file or command."
                )
            }
            Cycle::SimilarIntents { count, sample } => format!(
                "Your last {count} stated intents are nearly identical (\"{sample}\"). You do not seem to be making progress. Step back, say what is blocking you, and try something different."
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CycleConfig {
    /// Steps remembered.
    pub window: usize,
    /// Intents remembered.
    pub intent_window: usize,
    /// Same step in a row to count as a period-1 loop.
    pub period1_repeats: usize,
    /// Repeats of a 2- or 3-step sequence to count as a loop.
    pub period_n_repeats: usize,
    /// Jaccard similarity of intent words at or above which two intents are
    /// near-duplicates.
    pub intent_similarity: f64,
    /// Near-duplicate intents (counting the newest) that raise the alarm.
    pub intent_repeats: usize,
}

impl Default for CycleConfig {
    fn default() -> Self {
        Self {
            window: 24,
            intent_window: 8,
            period1_repeats: 3,
            period_n_repeats: 2,
            intent_similarity: 0.8,
            intent_repeats: 3,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CycleDetector {
    cfg: CycleConfig,
    steps: VecDeque<Step>,
    intents: VecDeque<String>,
}

impl CycleDetector {
    pub fn new(cfg: CycleConfig) -> Self {
        Self {
            cfg,
            steps: VecDeque::new(),
            intents: VecDeque::new(),
        }
    }

    /// Record a step and report a loop in the tail of the window, if any.
    pub fn push_step(&mut self, step: Step) -> Option<Cycle> {
        self.steps.push_back(step);
        while self.steps.len() > self.cfg.window.max(1) {
            self.steps.pop_front();
        }
        self.detect_repeat()
    }

    /// Record an intent (`did / why / next` joined into one string) and report
    /// when the newest one has too many near-duplicates in the window.
    pub fn push_intent(&mut self, intent: &str) -> Option<Cycle> {
        self.intents.push_back(intent.to_string());
        while self.intents.len() > self.cfg.intent_window.max(1) {
            self.intents.pop_front();
        }
        self.detect_intents()
    }

    /// Forget everything, e.g. after a context reset.
    pub fn clear(&mut self) {
        self.steps.clear();
        self.intents.clear();
    }

    fn detect_repeat(&self) -> Option<Cycle> {
        let n = self.steps.len();
        let v: Vec<&Step> = self.steps.iter().collect();
        for period in 1..=3usize {
            let repeats = if period == 1 {
                self.cfg.period1_repeats
            } else {
                self.cfg.period_n_repeats
            }
            .max(2);
            let need = period * repeats;
            if n < need {
                continue;
            }
            let tail = &v[n - need..];
            let unit = &tail[..period];
            if !tail
                .iter()
                .enumerate()
                .all(|(i, s)| **s == *unit[i % period])
            {
                continue;
            }
            // A unit made of one repeated step is a period-1 loop, which the
            // period-1 pass owns (with its own, stricter count).
            if period > 1 && unit.iter().all(|s| **s == *unit[0]) {
                continue;
            }
            // Count how far back the pattern really goes.
            let mut total = repeats;
            while n >= period * (total + 1)
                && (0..period).all(|i| *v[n - period * (total + 1) + i] == *unit[i])
            {
                total += 1;
            }
            return Some(Cycle::Repeat {
                period,
                repeats: total,
                tools: unit.iter().map(|s| s.tool.clone()).collect(),
            });
        }
        None
    }

    fn detect_intents(&self) -> Option<Cycle> {
        let newest = self.intents.back()?;
        let words = words(newest);
        if words.is_empty() {
            return None;
        }
        let similar = self
            .intents
            .iter()
            .take(self.intents.len() - 1)
            .filter(|i| jaccard(&words, &self::words(i)) >= self.cfg.intent_similarity)
            .count()
            + 1;
        (similar >= self.cfg.intent_repeats.max(2)).then(|| Cycle::SimilarIntents {
            count: similar,
            sample: newest.chars().take(120).collect(),
        })
    }
}

/// Lower-cased alphanumeric words, sorted and deduplicated.
fn words(s: &str) -> Vec<String> {
    let mut w: Vec<String> = s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect();
    w.sort();
    w.dedup();
    w
}

fn jaccard(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.iter().filter(|w| b.binary_search(w).is_ok()).count();
    let union = a.len() + b.len() - inter;
    inter as f64 / union as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(tool: &str, arg: &str, out: &str) -> Step {
        Step::new(tool, &json!({ "a": arg }), out, false)
    }

    #[test]
    fn step_hash_is_stable_and_key_order_independent() {
        let a = Step::new("t", &json!({"x": 1, "y": 2}), "ok", false);
        let b = Step::new("t", &json!({"y": 2, "x": 1}), "ok", false);
        assert_eq!(a, b);
        assert_ne!(a, Step::new("t", &json!({"x": 1, "y": 3}), "ok", false));
        assert_ne!(a, Step::new("t", &json!({"x": 1, "y": 2}), "other", false));
        assert_ne!(a, Step::new("t", &json!({"x": 1, "y": 2}), "ok", true));
        assert_ne!(a, Step::new("u", &json!({"x": 1, "y": 2}), "ok", false));
    }

    #[test]
    fn no_cycle_in_varied_steps() {
        let mut d = CycleDetector::default();
        for i in 0..20 {
            assert_eq!(d.push_step(step("read", &i.to_string(), "x")), None);
        }
    }

    #[test]
    fn period_one_needs_three_in_a_row() {
        let mut d = CycleDetector::default();
        assert_eq!(d.push_step(step("bash", "ls", "same")), None);
        assert_eq!(d.push_step(step("bash", "ls", "same")), None);
        let c = d
            .push_step(step("bash", "ls", "same"))
            .expect("third repeat");
        assert_eq!(
            c,
            Cycle::Repeat {
                period: 1,
                repeats: 3,
                tools: vec!["bash".into()]
            }
        );
        // And it keeps counting.
        let c = d.push_step(step("bash", "ls", "same")).unwrap();
        assert!(matches!(
            c,
            Cycle::Repeat {
                period: 1,
                repeats: 4,
                ..
            }
        ));
    }

    #[test]
    fn same_call_with_changing_outcome_is_not_a_loop() {
        let mut d = CycleDetector::default();
        for i in 0..6 {
            assert_eq!(
                d.push_step(step("bash", "cargo build", &format!("err {i}"))),
                None
            );
        }
    }

    #[test]
    fn period_two() {
        let mut d = CycleDetector::default();
        let seq = [("read", "a"), ("edit", "b"), ("read", "a")];
        for (t, a) in seq {
            assert_eq!(d.push_step(step(t, a, "o")), None);
        }
        let c = d.push_step(step("edit", "b", "o")).expect("A B A B");
        assert_eq!(
            c,
            Cycle::Repeat {
                period: 2,
                repeats: 2,
                tools: vec!["read".into(), "edit".into()]
            }
        );
        // The loop is still a loop one step later (seen from the other phase).
        let c = d.push_step(step("read", "a", "o")).unwrap();
        assert!(matches!(
            c,
            Cycle::Repeat {
                period: 2,
                repeats: 2,
                ..
            }
        ));
        let c = d.push_step(step("edit", "b", "o")).unwrap();
        assert!(matches!(
            c,
            Cycle::Repeat {
                period: 2,
                repeats: 3,
                ..
            }
        ));
    }

    #[test]
    fn period_three() {
        let mut d = CycleDetector::default();
        let seq = [("a", "1"), ("b", "2"), ("c", "3")];
        for _ in 0..1 {
            for (t, a) in seq {
                assert_eq!(d.push_step(step(t, a, "o")), None);
            }
        }
        let mut last = None;
        for (t, a) in seq {
            last = d.push_step(step(t, a, "o"));
        }
        assert_eq!(
            last,
            Some(Cycle::Repeat {
                period: 3,
                repeats: 2,
                tools: vec!["a".into(), "b".into(), "c".into()]
            })
        );
    }

    #[test]
    fn period_four_is_not_detected() {
        let mut d = CycleDetector::default();
        let mut last = None;
        for _ in 0..3 {
            for t in ["a", "b", "c", "d"] {
                last = d.push_step(step(t, "x", "o"));
            }
        }
        assert_eq!(last, None);
    }

    #[test]
    fn alternating_same_tool_different_outcome_is_not_period_one() {
        let mut d = CycleDetector::default();
        let mut last = None;
        for i in 0..6 {
            last = d.push_step(step("bash", "x", if i % 2 == 0 { "a" } else { "b" }));
        }
        // It *is* a period-2 loop (a b a b a b).
        assert!(matches!(last, Some(Cycle::Repeat { period: 2, .. })));
    }

    #[test]
    fn a_different_step_breaks_the_run() {
        let mut d = CycleDetector::default();
        d.push_step(step("bash", "ls", "same"));
        d.push_step(step("bash", "ls", "same"));
        assert_eq!(d.push_step(step("read", "f", "x")), None);
        assert_eq!(d.push_step(step("bash", "ls", "same")), None);
    }

    #[test]
    fn window_forgets_old_steps() {
        let mut d = CycleDetector::new(CycleConfig {
            window: 3,
            ..CycleConfig::default()
        });
        d.push_step(step("bash", "ls", "s"));
        d.push_step(step("bash", "ls", "s"));
        d.push_step(step("other", "x", "y"));
        assert_eq!(d.push_step(step("bash", "ls", "s")), None);
        assert_eq!(d.steps.len(), 3);
    }

    #[test]
    fn clear_resets() {
        let mut d = CycleDetector::default();
        d.push_step(step("bash", "ls", "s"));
        d.push_step(step("bash", "ls", "s"));
        d.clear();
        assert_eq!(d.push_step(step("bash", "ls", "s")), None);
    }

    #[test]
    fn near_duplicate_intents() {
        let mut d = CycleDetector::default();
        assert_eq!(
            d.push_intent("Fix the type error in parser.rs by adding a cast"),
            None
        );
        assert_eq!(
            d.push_intent("Fix the type error in parser.rs by adding a cast."),
            None
        );
        let c = d
            .push_intent("fix the type error in parser.rs, by adding a cast")
            .expect("third near-duplicate");
        assert!(matches!(c, Cycle::SimilarIntents { count: 3, .. }));
    }

    #[test]
    fn different_intents_are_fine() {
        let mut d = CycleDetector::default();
        for i in [
            "read the config loader",
            "add a retry to the http client",
            "write a test for the retry",
            "update the changelog",
        ] {
            assert_eq!(d.push_intent(i), None);
        }
    }

    #[test]
    fn two_similar_intents_are_not_enough() {
        let mut d = CycleDetector::default();
        d.push_intent("run the tests again");
        d.push_intent("something else entirely");
        assert_eq!(d.push_intent("run the tests again"), None);
    }

    #[test]
    fn empty_intent_is_ignored() {
        let mut d = CycleDetector::default();
        for _ in 0..5 {
            assert_eq!(d.push_intent("  ... "), None);
        }
    }

    #[test]
    fn notes_mention_what_repeated() {
        let n = Cycle::Repeat {
            period: 1,
            repeats: 3,
            tools: vec!["bash".into()],
        }
        .note();
        assert!(n.contains("`bash`") && n.contains("3 times"), "{n}");
        let n = Cycle::Repeat {
            period: 2,
            repeats: 2,
            tools: vec!["read".into(), "edit".into()],
        }
        .note();
        assert!(n.contains("read → edit"), "{n}");
        let n = Cycle::SimilarIntents {
            count: 3,
            sample: "fix it".into(),
        }
        .note();
        assert!(n.contains("fix it"), "{n}");
    }

    #[test]
    fn jaccard_basics() {
        assert_eq!(jaccard(&words("a b c"), &words("c b a")), 1.0);
        assert_eq!(jaccard(&words("a b"), &words("c d")), 0.0);
        assert!((jaccard(&words("a b c d"), &words("a b c e")) - 0.6).abs() < 1e-9);
    }
}
