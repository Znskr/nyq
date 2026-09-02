//! Native system URL opener behind the `host.UrlOpener` contract. Opens a URL
//! in the user's default browser via the platform launcher; shared by
//! authentication and user-consented URL flows.
//! (ported 1:1 from zig-reference/src/core/hosts/url_opener.zig)

use super::{host, target};
use crate::core::shared::debug_trace;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Term {
    Exited(i32),
    Signal(i32),
    Stopped(i32),
    Unknown(i32),
}

#[derive(Clone, Copy, Debug)]
struct LaunchResult {
    term: Term,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchError {
    SpawnFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchOutcome {
    Opened,
    Failed,
    Unsupported,
}

type LaunchFn = fn(usize, &[&[u8]]) -> Result<LaunchResult, LaunchError>;

#[derive(Clone, Copy)]
struct Launcher {
    ctx: usize,
    launch: LaunchFn,
}

impl Launcher {
    fn actual() -> Launcher {
        Launcher {
            ctx: 0,
            launch: launch_actual,
        }
    }
}

#[allow(non_upper_case_globals)]
pub const native_opener: host::UrlOpener = host::UrlOpener {
    context: 0,
    open_fn: open_url_for_host,
};

fn open_url_for_host(_: usize, url: &[u8]) -> Result<bool, host::UrlOpenError> {
    Ok(open_url(url))
}

fn open_url(url: &[u8]) -> bool {
    launch_url(url, target::current_os(), Launcher::actual()) == LaunchOutcome::Opened
}

fn launch_actual(_: usize, argv: &[&[u8]]) -> Result<LaunchResult, LaunchError> {
    #[cfg(unix)]
    let mut cmd = {
        use std::os::unix::ffi::OsStrExt;
        let mut c = std::process::Command::new(std::ffi::OsStr::from_bytes(argv[0]));
        for arg in &argv[1..] {
            c.arg(std::ffi::OsStr::from_bytes(arg));
        }
        c
    };
    #[cfg(not(unix))]
    let mut cmd = {
        let mut c = std::process::Command::new(std::str::from_utf8(argv[0]).unwrap_or(""));
        for arg in &argv[1..] {
            c.arg(std::str::from_utf8(arg).unwrap_or(""));
        }
        c
    };
    let output = cmd.output().map_err(|_| LaunchError::SpawnFailed)?;
    Ok(LaunchResult {
        term: term_from_status(output.status),
    })
}

fn term_from_status(status: std::process::ExitStatus) -> Term {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(code) = status.code() {
            Term::Exited(code)
        } else if let Some(sig) = status.signal() {
            Term::Signal(sig)
        } else {
            Term::Unknown(0)
        }
    }
    #[cfg(not(unix))]
    {
        match status.code() {
            Some(code) => Term::Exited(code),
            None => Term::Unknown(0),
        }
    }
}

fn launch_url(url: &[u8], os_tag: target::OsTag, launcher: Launcher) -> LaunchOutcome {
    let macos_argv: [&[u8]; 2] = [b"open", url];
    let linux_argv: [&[u8]; 2] = [b"xdg-open", url];
    let argv: &[&[u8]] = match os_tag {
        target::OsTag::Macos => &macos_argv,
        target::OsTag::Linux => &linux_argv,
        _ => return LaunchOutcome::Unsupported,
    };

    let result = match (launcher.launch)(launcher.ctx, argv) {
        Ok(result) => result,
        Err(_) => {
            debug_trace::logf(
                "core",
                format_args!("url opener launcher failed err=SpawnFailed"),
            );
            return LaunchOutcome::Failed;
        }
    };
    if let Term::Exited(0) = result.term {
        return LaunchOutcome::Opened;
    }

    log_unsuccessful_term(result.term);
    LaunchOutcome::Failed
}

fn log_unsuccessful_term(term: Term) {
    match term {
        Term::Exited(code) => debug_trace::logf(
            "core",
            format_args!("url opener unsuccessful term=exited exit_code={code}"),
        ),
        Term::Signal(sig) => debug_trace::logf(
            "core",
            format_args!("url opener unsuccessful term=signal signal={sig}"),
        ),
        Term::Stopped(sig) => debug_trace::logf(
            "core",
            format_args!("url opener unsuccessful term=stopped signal={sig}"),
        ),
        Term::Unknown(status) => debug_trace::logf(
            "core",
            format_args!("url opener unsuccessful term=unknown status={status}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static MOCK: RefCell<MockState> = RefCell::new(MockState::default());
    }

    struct MockState {
        argv_joined: Vec<u8>,
        result: Result<LaunchResult, LaunchError>,
    }

    impl Default for MockState {
        fn default() -> Self {
            MockState {
                argv_joined: Vec::new(),
                result: Ok(LaunchResult {
                    term: Term::Exited(0),
                }),
            }
        }
    }

    fn mock_launch(_: usize, argv: &[&[u8]]) -> Result<LaunchResult, LaunchError> {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            for (i, arg) in argv.iter().enumerate() {
                if i > 0 {
                    m.argv_joined.push(b' ');
                }
                m.argv_joined.extend_from_slice(arg);
            }
            m.result
        })
    }

    fn mock_launcher() -> Launcher {
        Launcher {
            ctx: 0,
            launch: mock_launch,
        }
    }

    fn argv() -> Vec<u8> {
        MOCK.with(|m| m.borrow().argv_joined.clone())
    }

    fn reset(result: Result<LaunchResult, LaunchError>) {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.argv_joined.clear();
            m.result = result;
        });
    }

    #[test]
    fn url_opener_selects_the_platform_launcher_argv() {
        reset(Ok(LaunchResult {
            term: Term::Exited(0),
        }));
        assert_eq!(
            LaunchOutcome::Opened,
            launch_url(b"http://localhost:3000", target::OsTag::Macos, mock_launcher())
        );
        assert_eq!(b"open http://localhost:3000".to_vec(), argv());

        reset(Ok(LaunchResult {
            term: Term::Exited(0),
        }));
        assert_eq!(
            LaunchOutcome::Opened,
            launch_url(b"http://localhost:3000", target::OsTag::Linux, mock_launcher())
        );
        assert_eq!(b"xdg-open http://localhost:3000".to_vec(), argv());
    }

    #[test]
    fn url_opener_reports_unsupported_platforms_without_launching() {
        reset(Ok(LaunchResult {
            term: Term::Exited(0),
        }));
        assert_eq!(
            LaunchOutcome::Unsupported,
            launch_url(b"http://x", target::OsTag::Windows, mock_launcher())
        );
        assert_eq!(Vec::<u8>::new(), argv());
    }

    #[test]
    fn url_opener_treats_nonzero_exit_bad_terms_and_launch_errors_as_failed() {
        reset(Ok(LaunchResult {
            term: Term::Exited(3),
        }));
        assert_eq!(
            LaunchOutcome::Failed,
            launch_url(b"http://x", target::OsTag::Macos, mock_launcher())
        );

        reset(Ok(LaunchResult {
            term: Term::Signal(2),
        }));
        assert_eq!(
            LaunchOutcome::Failed,
            launch_url(b"http://x", target::OsTag::Macos, mock_launcher())
        );

        reset(Err(LaunchError::SpawnFailed));
        assert_eq!(
            LaunchOutcome::Failed,
            launch_url(b"http://x", target::OsTag::Linux, mock_launcher())
        );
    }
}
