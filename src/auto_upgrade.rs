//! Automatic background self-upgrade for interactive sessions.
//!
//! Ten seconds after the TUI starts, and then every 30 minutes, the newest
//! release on the saved channel is compared with the running version. A newer
//! one is downloaded, verified against SHA256SUMS and installed over the
//! executable (the same steps as `nyq upgrade`), without a prompt. The status
//! line then offers Ctrl+G, which relaunches the new binary on the same
//! conversation. Disabled by `auto_upgrade = false` in settings.toml,
//! `NYQ_AUTO_UPGRADE=0`, and for binaries inside a cargo `target` directory.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use crate::upgrade::{self, Channel, Github, Release};

const INITIAL_DELAY: Duration = Duration::from_secs(10);
const CHECK_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Longest version kept for the status line.
const MAX_VERSION_LEN: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Idle = 0,
    Checking = 1,
    Waiting = 2,
    Downloading = 3,
    Ready = 4,
    Failed = 5,
}

impl State {
    fn from_u8(raw: u8) -> State {
        match raw {
            1 => State::Checking,
            2 => State::Waiting,
            3 => State::Downloading,
            4 => State::Ready,
            5 => State::Failed,
            _ => State::Idle,
        }
    }
}

/// Whether `NYQ_AUTO_UPGRADE` leaves auto-upgrade on (anything but a false value).
pub fn env_allows(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// Whether auto-upgrade is wanted: the setting and the environment both allow it.
/// `NYQ_NO_UPDATE_CHECK` (no release checks at all) turns it off too.
pub fn enabled(setting: bool) -> bool {
    setting
        && env_allows(std::env::var("NYQ_AUTO_UPGRADE").ok().as_deref())
        && std::env::var_os("NYQ_NO_UPDATE_CHECK").is_none()
}

/// A binary built in a cargo `target/{debug,release}` directory (including
/// `target/<triple>/{debug,release}`) is a development build: it is never
/// replaced by a release.
pub fn is_development_build_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split(['/', '\\']).collect();
    let profile = |p: &str| p == "debug" || p == "release";
    parts.windows(2).any(|w| w[0] == "target" && profile(w[1]))
        || parts.windows(3).any(|w| w[0] == "target" && w[1].contains('-') && profile(w[2]))
}

/// Whether the background task may start for the executable at `exe`.
pub fn should_start(setting: bool, exe: Option<&Path>) -> bool {
    enabled(setting)
        && upgrade::platform_archive().is_some()
        && exe.is_some_and(|p| !is_development_build_path(&p.to_string_lossy()))
}

/// Where releases come from. Split out so the state machine is testable
/// without the network.
#[async_trait::async_trait]
pub trait Source: Send {
    /// The newest version on the channel, or None when the check fails.
    async fn latest(&mut self) -> Option<String>;
    /// Download, verify and install `version` over the running executable.
    async fn install(&mut self, version: &str, cancel: &CancellationToken) -> Result<()>;
}

/// GitHub releases of nyq, installed over the running executable.
pub struct GithubSource {
    home: PathBuf,
    channel: Channel,
    found: Option<(Github, Release)>,
}

impl GithubSource {
    pub fn new(home: PathBuf, channel: Channel) -> Self {
        Self { home, channel, found: None }
    }
}

#[async_trait::async_trait]
impl Source for GithubSource {
    async fn latest(&mut self) -> Option<String> {
        self.found = None;
        let mut gh = Github::new(Duration::from_secs(300)).ok()?;
        let (release, latest) = upgrade::newest(&mut gh, self.channel).await.ok()?;
        upgrade::record_check(&self.home, Some(&latest));
        self.found = Some((gh, release));
        Some(latest)
    }

    async fn install(&mut self, version: &str, cancel: &CancellationToken) -> Result<()> {
        let (gh, release) = self.found.take().context("no release to install")?;
        let name = upgrade::platform_archive().context("no prebuilt release for this platform")?;
        let exe = upgrade::current_exe()?;
        let bytes = upgrade::download_verified(&gh, &release, &name).await?;
        anyhow::ensure!(!cancel.is_cancelled(), "cancelled");
        let version = version.to_string();
        tokio::task::spawn_blocking(move || upgrade::install_archive(&bytes, &name, &version, &exe)).await?
    }
}

/// The background upgrader shared by the task, the status line and Ctrl+G.
pub struct Upgrader {
    state: AtomicU8,
    render_dirty: AtomicBool,
    latest: Mutex<String>,
    cancel: CancellationToken,
}

impl Default for Upgrader {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(State::Idle as u8),
            render_dirty: AtomicBool::new(false),
            latest: Mutex::new(String::new()),
            cancel: CancellationToken::new(),
        }
    }
}

impl Upgrader {
    /// Start checking `channel` in the background.
    pub fn start(home: &Path) -> Arc<Upgrader> {
        let upgrader = Arc::new(Upgrader::default());
        let source = GithubSource::new(home.to_path_buf(), upgrade::saved_channel(home));
        tokio::spawn(upgrader.clone().run(source, INITIAL_DELAY, CHECK_INTERVAL));
        upgrader
    }

    /// Stop the background task (at its next await point).
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    pub fn state(&self) -> State {
        State::from_u8(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, state: State) {
        if self.state.swap(state as u8, Ordering::AcqRel) != state as u8 {
            self.render_dirty.store(true, Ordering::Release);
        }
    }

    fn set_latest(&self, version: &str) {
        let v = version.trim().trim_start_matches('v');
        let v: String = v.chars().take(MAX_VERSION_LEN).collect();
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = v;
        self.render_dirty.store(true, Ordering::Release);
    }

    fn latest(&self) -> String {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// What the status line shows (empty: nothing).
    pub fn status_label(&self) -> String {
        match self.state() {
            State::Downloading => format!("upgrading to {}...", self.latest()),
            State::Ready => format!("update ready: {} to reload", crate::ui::keys::hint(crate::ui::keys::KeyAction::Reload)),
            State::Failed => "upgrade failed".into(),
            State::Idle | State::Checking | State::Waiting => String::new(),
        }
    }

    /// Whether the state changed since the last call (the status line needs a redraw).
    pub fn take_render_dirty(&self) -> bool {
        self.render_dirty.swap(false, Ordering::AcqRel)
    }

    /// Sleep unless stopped; false when stopped.
    async fn sleep(&self, d: Duration) -> bool {
        tokio::select! {
            _ = self.cancel.cancelled() => false,
            _ = tokio::time::sleep(d) => true,
        }
    }

    /// The check loop: stops after `Ready` or when stopped.
    async fn run(self: Arc<Self>, mut source: impl Source, initial: Duration, interval: Duration) {
        if !self.sleep(initial).await {
            return;
        }
        while !self.cancel.is_cancelled() {
            if self.state() == State::Ready {
                return;
            }
            self.set_state(State::Checking);
            self.run_once(&mut source).await;
            let post = self.state();
            if post == State::Ready {
                return;
            }
            if post != State::Failed {
                self.set_state(State::Waiting);
            }
            if !self.sleep(interval).await {
                return;
            }
        }
    }

    /// One check: a failed check is silent; a failed install shows `Failed`.
    async fn run_once(&self, source: &mut impl Source) {
        let latest = tokio::select! {
            _ = self.cancel.cancelled() => return,
            l = source.latest() => l,
        };
        let Some(latest) = latest.filter(|l| upgrade::is_newer(l, upgrade::CURRENT)) else {
            return;
        };
        self.set_latest(&latest);
        self.set_state(State::Downloading);
        let installed = tokio::select! {
            _ = self.cancel.cancelled() => Err(anyhow::anyhow!("cancelled")),
            r = source.install(&latest, &self.cancel) => r,
        };
        self.set_state(if installed.is_ok() { State::Ready } else { State::Failed });
    }
}

// ---------------------------------------------------------------- apply (Ctrl+G)

/// What the app is doing when Ctrl+G is pressed.
#[derive(Clone, Debug, Default)]
pub struct ApplyFacts {
    /// A reply is streaming or a command runs.
    pub busy: bool,
    pub queued: bool,
    pub question: bool,
    pub approval: bool,
    pub secret: bool,
    pub session_picker: bool,
    /// The model switcher, settings or permission-mode menu.
    pub menu: bool,
    pub transcript: bool,
    pub draft: bool,
    pub attachments: bool,
    /// Auto-upgrade runs in this session.
    pub enabled: bool,
    pub state: Option<State>,
}

/// Why Ctrl+G can't relaunch now; None when the installed upgrade can be applied.
pub fn apply_refusal(f: &ApplyFacts) -> Option<&'static str> {
    let busy = [
        (f.busy, "upgrade is unavailable until the response finishes"),
        (f.queued, "upgrade is unavailable while prompts are queued"),
        (f.question, "upgrade is unavailable while a question is open"),
        (f.approval, "upgrade is unavailable while an approval is open"),
        (f.secret, "upgrade is unavailable during API key entry"),
        (f.session_picker, "close the session picker before upgrading"),
        (f.menu, "close the menu before upgrading"),
        (f.transcript, "close the transcript view before upgrading"),
        (f.draft, "submit or clear the current prompt before upgrading"),
        (f.attachments, "submit or clear pending images before upgrading"),
    ];
    if let Some((_, notice)) = busy.iter().find(|(on, _)| *on) {
        return Some(notice);
    }
    if !f.enabled {
        return Some("auto-upgrade is disabled");
    }
    if f.state != Some(State::Ready) {
        return Some("no installed upgrade is ready");
    }
    None
}

/// The new process to start in place of this one.
#[derive(Debug)]
pub struct Relaunch {
    pub exe: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

/// Options of the interactive command line that take a value (kept with it).
pub(crate) const VALUE_LONG: &[&str] = &[
    "--model",
    "--provider",
    "--permission-mode",
    "--sandbox",
    "--add-dir",
    "--context-limit",
    "--system",
    "--image",
];
pub(crate) const VALUE_SHORT: &[char] = &['m'];
/// Flags that must not carry over: they pick or start a conversation, or (a
/// tape) can't be reopened for the same session.
const DROP_LONG: &[&str] = &["--continue", "--print", "--record"];
const DROP_SHORT: &[char] = &['c', 'p'];

/// The relaunch command line: the original arguments (without the program)
/// minus the prompt, subcommand words, `-c`/`-r` and `--record`, plus
/// `--resume <id>` when the conversation was saved.
pub fn relaunch_args(args: &[OsString], resume: Option<&str>) -> Vec<OsString> {
    let mut out: Vec<OsString> = Vec::new();
    let mut it = args.iter().peekable();
    let looks_like_value = |a: Option<&&OsString>| a.is_some_and(|a| !a.to_string_lossy().starts_with('-'));
    while let Some(arg) = it.next() {
        let Some(s) = arg.to_str() else {
            continue; // a non-UTF-8 prompt word
        };
        if s == "--" {
            break; // everything after is prompt text
        }
        if let Some(long) = s.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, _)) => (format!("--{n}"), true),
                None => (s.to_string(), false),
            };
            if name == "--resume" {
                if !inline && looks_like_value(it.peek()) {
                    it.next();
                }
            } else if DROP_LONG.contains(&name.as_str()) {
            } else if VALUE_LONG.contains(&name.as_str()) {
                out.push(arg.clone());
                if !inline {
                    out.extend(it.next().cloned());
                }
            } else {
                out.push(arg.clone());
            }
            continue;
        }
        if s.len() > 1 && s.starts_with('-') {
            // A cluster of short flags, e.g. `-c`, `-mgpt-5`, `-cm gpt-5`, `-r ID`.
            let mut kept = String::from("-");
            let chars: Vec<char> = s[1..].chars().collect();
            for (i, ch) in chars.iter().enumerate() {
                let rest: String = chars[i + 1..].iter().collect();
                if *ch == 'r' {
                    if rest.is_empty() && looks_like_value(it.peek()) {
                        it.next();
                    }
                    break;
                }
                if VALUE_SHORT.contains(ch) {
                    if kept.len() > 1 {
                        out.push(OsString::from(kept.as_str()));
                    }
                    if rest.is_empty() {
                        out.push(OsString::from(format!("-{ch}")));
                        out.extend(it.next().cloned());
                    } else {
                        out.push(OsString::from(format!("-{ch}{rest}")));
                    }
                    kept.clear();
                    break;
                }
                if !DROP_SHORT.contains(ch) {
                    kept.push(*ch);
                }
            }
            if kept.len() > 1 {
                out.push(OsString::from(kept));
            }
            continue;
        }
        // A positional: prompt words, or `session resume [ID]`.
    }
    if let Some(id) = resume {
        out.push("--resume".into());
        out.push(id.into());
    }
    out
}

/// Replace this process with `r` (Unix: exec). Returns only on failure.
#[cfg(unix)]
pub fn relaunch(r: &Relaunch) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(&r.exe).args(&r.args).current_dir(&r.cwd).exec()
}

/// Run `r` with this terminal and exit with its status (Windows has no exec).
/// Returns only on failure to start it.
#[cfg(not(unix))]
pub fn relaunch(r: &Relaunch) -> std::io::Error {
    match std::process::Command::new(&r.exe).args(&r.args).current_dir(&r.cwd).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => e,
    }
}

/// Display form of a relaunch for messages.
pub fn manual_hint(resume: Option<&str>) -> String {
    match resume {
        Some(id) => format!("restart nyq manually (nyq --resume {id})"),
        None => "restart nyq manually".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn strs(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn status_labels_follow_the_state() {
        let u = Upgrader::default();
        assert_eq!(u.state(), State::Idle);
        assert!(!u.take_render_dirty());
        assert_eq!(u.status_label(), "");
        u.set_state(State::Checking);
        assert!(u.take_render_dirty());
        assert!(!u.take_render_dirty());
        u.set_state(State::Checking);
        assert!(!u.take_render_dirty(), "same state does not re-render");
        assert_eq!(u.status_label(), "");
        u.set_state(State::Waiting);
        assert_eq!(u.status_label(), "");
        u.set_latest("v0.3.0");
        assert!(u.take_render_dirty());
        u.set_state(State::Downloading);
        assert_eq!(u.status_label(), "upgrading to 0.3.0...");
        u.set_state(State::Ready);
        assert_eq!(u.status_label(), "update ready: ctrl+g to reload");
        u.set_state(State::Failed);
        assert_eq!(u.status_label(), "upgrade failed");
        u.set_latest(&format!("v{}", "1".repeat(40)));
        assert_eq!(u.latest().len(), MAX_VERSION_LEN);
    }

    struct Fake {
        versions: Vec<Option<String>>,
        install_ok: bool,
        checks: Arc<AtomicUsize>,
        installs: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Source for Fake {
        async fn latest(&mut self) -> Option<String> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            if self.versions.is_empty() {
                None
            } else {
                self.versions.remove(0)
            }
        }
        async fn install(&mut self, _version: &str, _cancel: &CancellationToken) -> Result<()> {
            self.installs.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(self.install_ok, "boom");
            Ok(())
        }
    }

    fn fake(versions: &[Option<&str>], install_ok: bool) -> (Fake, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let checks = Arc::new(AtomicUsize::new(0));
        let installs = Arc::new(AtomicUsize::new(0));
        let f = Fake {
            versions: versions.iter().map(|v| v.map(String::from)).collect(),
            install_ok,
            checks: checks.clone(),
            installs: installs.clone(),
        };
        (f, checks, installs)
    }

    const MS: Duration = Duration::from_millis(1);

    async fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(MS).await;
        }
    }

    #[tokio::test]
    async fn a_newer_release_is_installed_and_the_loop_stops() {
        let (src, checks, installs) = fake(&[None, Some("0.0.1"), Some("v999.0.0"), Some("v1000.0.0")], true);
        let u = Arc::new(Upgrader::default());
        tokio::time::timeout(Duration::from_secs(5), u.clone().run(src, MS, MS)).await.unwrap();
        assert_eq!(u.state(), State::Ready);
        assert_eq!(u.latest(), "999.0.0");
        // A failed check and an older release are skipped; nothing after ready.
        assert_eq!((checks.load(Ordering::SeqCst), installs.load(Ordering::SeqCst)), (3, 1));
    }

    #[tokio::test]
    async fn a_failed_install_shows_failed_and_keeps_checking() {
        let (src, checks, installs) = fake(&[Some("999.0.0"), Some("999.0.0")], false);
        let u = Arc::new(Upgrader::default());
        let task = tokio::spawn(u.clone().run(src, MS, Duration::from_millis(20)));
        // Poll rather than sleep a fixed time: Windows timers tick ~15ms.
        wait_until("failed", || u.state() == State::Failed).await;
        assert_eq!(u.status_label(), "upgrade failed");
        wait_until("a second attempt", || checks.load(Ordering::SeqCst) >= 2 && installs.load(Ordering::SeqCst) >= 2).await;
        u.stop();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn up_to_date_waits_and_stop_interrupts_the_sleep() {
        let (src, checks, _) = fake(&[Some(upgrade::CURRENT)], true);
        let u = Arc::new(Upgrader::default());
        let task = tokio::spawn(u.clone().run(src, MS, Duration::from_secs(3600)));
        wait_until("waiting", || u.state() == State::Waiting).await;
        assert_eq!(checks.load(Ordering::SeqCst), 1);
        u.stop();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        // Stopped during the initial delay: never checks.
        let (src, checks, _) = fake(&[Some("999.0.0")], true);
        let u = Arc::new(Upgrader::default());
        u.stop();
        u.clone().run(src, Duration::from_secs(3600), MS).await;
        assert_eq!((u.state(), checks.load(Ordering::SeqCst)), (State::Idle, 0));
    }

    #[test]
    fn development_builds_are_detected() {
        assert!(is_development_build_path("/repo/target/debug/nyq"));
        assert!(is_development_build_path("/repo/target/release/nyq"));
        assert!(is_development_build_path("/repo/target/release/deps/nyq-1234"));
        assert!(is_development_build_path("/repo/target/x86_64-unknown-linux-gnu/release/nyq"));
        assert!(is_development_build_path("C:\\repo\\target\\debug\\nyq.exe"));
        assert!(is_development_build_path("C:\\repo\\target\\release\\nyq.exe"));
        assert!(is_development_build_path("C:\\repo\\target\\x86_64-pc-windows-msvc\\release\\nyq.exe"));
        assert!(!is_development_build_path("/home/me/.local/bin/nyq"));
        assert!(!is_development_build_path("/home/me/.cargo/bin/nyq"));
        assert!(!is_development_build_path("C:\\Users\\me\\bin\\nyq.exe"));
        assert!(!is_development_build_path("/srv/targets/release-notes/nyq"));
        assert!(!is_development_build_path("/opt/target/nyq"));
        assert!(!should_start(true, Some(Path::new("/repo/target/debug/nyq"))));
        assert!(!should_start(false, Some(Path::new("/usr/local/bin/nyq"))));
        assert!(!should_start(true, None));
    }

    #[test]
    fn env_and_setting_disable_auto_upgrade() {
        for off in ["0", "false", "OFF", " no "] {
            assert!(!env_allows(Some(off)), "{off}");
        }
        for on in ["1", "true", "yes", ""] {
            assert!(env_allows(Some(on)), "{on}");
        }
        assert!(env_allows(None));
        assert!(!enabled(false));
    }

    #[test]
    fn relaunch_drops_the_prompt_and_resume_flags() {
        let run = |a: &[&str], id: Option<&str>| strs(&relaunch_args(&args(a), id));
        assert_eq!(run(&["fix", "the", "bug"], Some("S")), ["--resume", "S"]);
        assert_eq!(
            run(&["--yolo", "-m", "gpt-5", "--provider=xai", "hello", "--sandbox", "off", "world"], Some("S")),
            ["--yolo", "-m", "gpt-5", "--provider=xai", "--sandbox", "off", "--resume", "S"]
        );
        assert_eq!(run(&["-c", "--allow-network", "more"], Some("S")), ["--allow-network", "--resume", "S"]);
        assert_eq!(run(&["--continue", "--add-dir", "/x", "--add-dir=/y"], Some("S")), ["--add-dir", "/x", "--add-dir=/y", "--resume", "S"]);
        assert_eq!(run(&["-r", "abc", "prompt"], Some("S")), ["--resume", "S"]);
        assert_eq!(run(&["-r", "--yolo"], Some("S")), ["--yolo", "--resume", "S"]);
        assert_eq!(run(&["--resume=abc", "-rabc", "--resume", "def"], Some("S")), ["--resume", "S"]);
        assert_eq!(run(&["--permission-mode", "auto", "session", "resume", "abc"], Some("S")), ["--permission-mode", "auto", "--resume", "S"]);
        assert_eq!(run(&["-cm", "gpt-5", "-mo3"], Some("S")), ["-m", "gpt-5", "-mo3", "--resume", "S"]);
        assert_eq!(run(&["--record", "--no-additional-dirs", "--", "--yolo"], Some("S")), ["--no-additional-dirs", "--resume", "S"]);
        // An unsaved conversation starts fresh.
        assert_eq!(run(&["hello", "--yolo"], None), ["--yolo"]);
        assert!(run(&[], None).is_empty());
    }

    #[test]
    fn apply_is_refused_while_the_app_is_busy() {
        let ready = ApplyFacts { enabled: true, state: Some(State::Ready), ..Default::default() };
        assert_eq!(apply_refusal(&ready), None);
        type Set = fn(&mut ApplyFacts);
        let cases: [(Set, &str); 10] = [
            (|f| f.busy = true, "until the response finishes"),
            (|f| f.queued = true, "while prompts are queued"),
            (|f| f.question = true, "while a question is open"),
            (|f| f.approval = true, "while an approval is open"),
            (|f| f.secret = true, "during API key entry"),
            (|f| f.session_picker = true, "close the session picker"),
            (|f| f.menu = true, "close the menu"),
            (|f| f.transcript = true, "close the transcript view"),
            (|f| f.draft = true, "current prompt"),
            (|f| f.attachments = true, "pending images"),
        ];
        for (set, want) in cases {
            let mut f = ready.clone();
            set(&mut f);
            assert!(apply_refusal(&f).unwrap().contains(want), "{want}");
        }
        // Busy states come first, then disabled, then not ready.
        let busy_off = ApplyFacts { busy: true, ..Default::default() };
        assert!(apply_refusal(&busy_off).unwrap().contains("response finishes"));
        assert_eq!(apply_refusal(&ApplyFacts::default()), Some("auto-upgrade is disabled"));
        for state in [None, Some(State::Idle), Some(State::Downloading), Some(State::Failed)] {
            let f = ApplyFacts { enabled: true, state, ..Default::default() };
            assert_eq!(apply_refusal(&f), Some("no installed upgrade is ready"));
        }
        assert_eq!(manual_hint(Some("x")), "restart nyq manually (nyq --resume x)");
    }
}
