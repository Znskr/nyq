//! The background verifier (design §4.2): after writes, a debounced build
//! (and tests, when configured) runs off the request path, and its result is
//! recorded in a [`VerifyState`] the trailing note shows.
//!
//! Writes coalesce: every write restarts the debounce, and writes made while
//! a run is going cause exactly one more run after it. The model request never
//! waits for a run that has not started; one that is running gets a short,
//! bounded wait ([`Verifier::settle`]).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::build::{BuildSystem, Exec, TestScope, VerifyState};

/// Distinct failure texts remembered for the note, oldest first.
const KEEP_FAILURES: usize = 4;

#[derive(Default)]
struct Status {
    verify: VerifyState,
    /// Distinct failure summaries, oldest first; the last is the newest.
    failures: Vec<String>,
    dirty: bool,
    /// Skip the debounce for the pending run (the baseline).
    now: bool,
    last_write: Option<Instant>,
    running: bool,
    /// Finished runs.
    runs: u64,
    /// Writes scheduled so far.
    writes: u64,
    /// `writes` when the last finished run started.
    ran_at_writes: u64,
}

struct Shared {
    status: Mutex<Status>,
    wake: Notify,
    done: Notify,
    debounce: Duration,
}

/// What the verifier knows right now.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub verify: VerifyState,
    /// Failure summaries before the newest one, oldest first.
    pub older_failures: Vec<String>,
    pub runs: u64,
    pub running: bool,
    pub pending: bool,
    /// Writes scheduled when the latest finished run started.
    pub ran_at_writes: u64,
    /// Writes scheduled so far.
    pub writes: u64,
}

/// A running verifier. Dropping it stops the worker and cancels a running
/// build.
pub struct Verifier {
    shared: Arc<Shared>,
    cancel: CancellationToken,
}

impl Verifier {
    /// Start the worker. `tests` adds a test run after a green build.
    pub fn start(bs: Arc<dyn BuildSystem>, exec: Exec, tests: bool, debounce: Duration) -> Self {
        let shared = Arc::new(Shared {
            status: Mutex::new(Status::default()),
            wake: Notify::new(),
            done: Notify::new(),
            debounce,
        });
        let cancel = CancellationToken::new();
        let exec = exec.with_cancel(cancel.clone());
        tokio::spawn(worker(
            shared.clone(),
            bs,
            exec,
            tests,
            debounce,
            cancel.clone(),
        ));
        Self { shared, cancel }
    }

    /// A verifier that never runs anything and reports `state` (for tests and
    /// for callers that already know the result).
    pub fn fixed(state: VerifyState) -> Self {
        let runs = u64::from(state.build.is_some() || state.tests.is_some());
        let shared = Arc::new(Shared {
            status: Mutex::new(Status {
                failures: state
                    .build
                    .iter()
                    .chain(state.tests.iter())
                    .filter(|s| !s.ok)
                    .map(|s| s.summary.clone())
                    .collect(),
                verify: state,
                runs,
                ..Status::default()
            }),
            wake: Notify::new(),
            done: Notify::new(),
            debounce: Duration::ZERO,
        });
        Self {
            shared,
            cancel: CancellationToken::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Status> {
        self.shared.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A write happened: verify once things are quiet.
    pub fn schedule(&self) {
        {
            let mut s = self.lock();
            s.dirty = true;
            s.writes += 1;
            s.last_write = Some(Instant::now());
        }
        self.shared.wake.notify_one();
    }

    /// Verify as soon as possible, without the debounce (the baseline run at
    /// the start of a task).
    pub fn run_now(&self) {
        {
            let mut s = self.lock();
            s.dirty = true;
            s.now = true;
        }
        self.shared.wake.notify_one();
    }

    pub fn snapshot(&self) -> Snapshot {
        let s = self.lock();
        let older = s.failures.len().saturating_sub(1);
        Snapshot {
            verify: s.verify.clone(),
            older_failures: s.failures[..older].to_vec(),
            runs: s.runs,
            running: s.running,
            pending: s.dirty,
            ran_at_writes: s.ran_at_writes,
            writes: s.writes,
        }
    }

    /// If a run is in flight (running, or due: its debounce is over), wait
    /// for it to finish, at most `bound`. A run still inside its debounce
    /// window is not waited for.
    pub async fn settle(&self, bound: Duration) {
        let done = self.shared.done.notified();
        tokio::pin!(done);
        done.as_mut().enable();
        let in_flight = {
            let s = self.lock();
            s.running
                || (s.dirty
                    && (s.now
                        || s.last_write
                            .is_none_or(|t| t.elapsed() >= self.shared.debounce)))
        };
        if in_flight {
            let _ = tokio::time::timeout(bound, done).await;
        }
    }

    /// Wait until nothing is pending or running, at most `bound` (tests and
    /// headless shutdown).
    pub async fn idle(&self, bound: Duration) {
        let deadline = Instant::now() + bound;
        loop {
            let done = self.shared.done.notified();
            tokio::pin!(done);
            done.as_mut().enable();
            {
                let s = self.lock();
                if !s.running && !s.dirty {
                    return;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, done).await.is_err() {
                return;
            }
        }
    }
}

impl Drop for Verifier {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn worker(
    shared: Arc<Shared>,
    bs: Arc<dyn BuildSystem>,
    exec: Exec,
    tests: bool,
    debounce: Duration,
    cancel: CancellationToken,
) {
    let lock = || shared.status.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        // Wait for work.
        loop {
            if lock().dirty {
                break;
            }
            tokio::select! {
                _ = shared.wake.notified() => {}
                _ = cancel.cancelled() => return,
            }
        }
        // Debounce: wait until no write came in for `debounce`.
        loop {
            let wait = {
                let s = lock();
                if s.now {
                    Duration::ZERO
                } else {
                    s.last_write
                        .map(|t| (t + debounce).saturating_duration_since(Instant::now()))
                        .unwrap_or_default()
                }
            };
            if wait.is_zero() {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = cancel.cancelled() => return,
            }
        }
        let started_at_writes = {
            let mut s = lock();
            s.dirty = false;
            s.now = false;
            s.running = true;
            s.writes
        };
        let build = bs.build(&exec).await;
        let test = if tests && build.ok {
            Some(bs.test(&exec, &TestScope::All).await)
        } else {
            None
        };
        if cancel.is_cancelled() {
            return;
        }
        {
            let mut s = lock();
            s.verify.record_build(&build);
            if let Some(t) = &test {
                s.verify.record_tests(t);
            }
            let failed = [Some(&build), test.as_ref()]
                .into_iter()
                .flatten()
                .find(|o| !o.ok)
                .map(|o| o.summary.clone());
            if let Some(f) = failed {
                if s.failures.last() != Some(&f) {
                    s.failures.push(f);
                    let extra = s.failures.len().saturating_sub(KEEP_FAILURES);
                    s.failures.drain(..extra);
                }
            }
            s.running = false;
            s.runs += 1;
            s.ran_at_writes = started_at_writes;
        }
        shared.done.notify_waiters();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::task::build::{CommandOutput, FakeRunner};
    use std::path::{Path, PathBuf};

    /// A build system whose commands are `fake-build` and `fake-test`.
    pub(crate) struct FakeBuild(pub PathBuf);

    #[async_trait::async_trait]
    impl BuildSystem for FakeBuild {
        fn name(&self) -> &str {
            "fake"
        }
        fn root(&self) -> &Path {
            &self.0
        }
        fn detect(_: &Path) -> Option<Self> {
            None
        }
        fn is_source(&self, _: &str) -> bool {
            true
        }
        fn is_test(&self, _: &str) -> bool {
            false
        }
        fn build_command(&self) -> Option<String> {
            Some("fake-build".into())
        }
        fn test_command(&self, _: &TestScope) -> Option<String> {
            Some("fake-test".into())
        }
        fn is_error_start(&self, line: &str) -> bool {
            line.starts_with("error")
        }
    }

    fn builds(fake: &FakeRunner) -> usize {
        fake.calls()
            .iter()
            .filter(|c| c.contains("fake-build"))
            .count()
    }

    #[tokio::test]
    async fn writes_are_debounced_and_coalesced() {
        let fake = FakeRunner::new()
            .on("fake-build", vec![CommandOutput::ok("")])
            .on("fake-test", vec![CommandOutput::ok("")]);
        let v = Verifier::start(
            Arc::new(FakeBuild(PathBuf::from("."))),
            Exec::new(fake.clone()),
            true,
            Duration::from_millis(80),
        );
        for _ in 0..4 {
            v.schedule();
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        // Still inside the debounce window: nothing ran yet.
        assert_eq!(builds(&fake), 0);
        assert!(v.snapshot().pending);
        v.idle(Duration::from_secs(5)).await;
        assert_eq!(builds(&fake), 1, "four writes, one build");
        let snap = v.snapshot();
        assert_eq!(snap.runs, 1);
        assert_eq!(snap.ran_at_writes, 4);
        assert!(snap.verify.build.as_ref().unwrap().ok);
        assert!(
            snap.verify.tests.as_ref().unwrap().ok,
            "tests ran after the green build"
        );

        v.schedule();
        v.idle(Duration::from_secs(5)).await;
        assert_eq!(builds(&fake), 2);
    }

    #[tokio::test]
    async fn failures_are_recorded_and_settle_waits_for_a_running_build() {
        let fake = FakeRunner::new().on(
            "fake-build",
            vec![
                CommandOutput::failed(1, "error: first"),
                CommandOutput::failed(1, "error: second"),
            ],
        );
        let v = Verifier::start(
            Arc::new(FakeBuild(PathBuf::from("."))),
            Exec::new(fake.clone()),
            false,
            Duration::from_secs(60),
        );
        // `run_now` skips the long debounce.
        v.run_now();
        v.idle(Duration::from_secs(5)).await;
        v.run_now();
        v.idle(Duration::from_secs(5)).await;
        let snap = v.snapshot();
        assert!(snap.verify.build_is_red());
        assert_eq!(snap.verify.consecutive_build_failures, 2);
        assert!(snap.verify.build.unwrap().summary.contains("second"));
        assert_eq!(snap.older_failures.len(), 1);
        assert!(snap.older_failures[0].contains("first"));
        assert_eq!(builds(&fake), 2, "the test step never ran");
        // Nothing running: settle returns at once.
        let t = Instant::now();
        v.settle(Duration::from_secs(5)).await;
        assert!(t.elapsed() < Duration::from_secs(1));
        // A write inside its debounce window is not waited for.
        v.schedule();
        let t = Instant::now();
        v.settle(Duration::from_secs(5)).await;
        assert!(t.elapsed() < Duration::from_secs(1));
        assert!(v.snapshot().pending);
        // A due run is: `run_now` then settle sees its result.
        v.run_now();
        v.settle(Duration::from_secs(5)).await;
        assert_eq!(v.snapshot().runs, 3);
    }

    #[test]
    fn a_fixed_verifier_reports_its_state() {
        let v = Verifier::fixed(VerifyState::default());
        assert_eq!(v.snapshot().runs, 0);
    }
}
