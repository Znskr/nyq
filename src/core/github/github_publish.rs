//! GitHub PR/issue publishing through the `gh` CLI (ported 1:1 from
//! zig-reference/src/core/github/github_publish.zig).

use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Workflow {
    PullRequest,
    Issue,
}

#[derive(Debug)]
pub struct Draft {
    pub title: Vec<u8>,
    pub body: Vec<u8>,
}

pub struct PublishResult {
    pub ok: bool,
    pub text: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidGithubDraft,
}

struct CompletedProcess {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

pub fn parse_draft(text: &[u8]) -> Result<Draft, Error> {
    let trimmed = trim(text);
    if trimmed.is_empty() {
        return Err(Error::InvalidGithubDraft);
    }

    let Some(first_break) = trimmed.iter().position(|&b| b == b'\n') else {
        return Ok(Draft {
            title: trimmed.to_vec(),
            body: Vec::new(),
        });
    };

    let title_slice = trim(&trimmed[..first_break]);
    if title_slice.is_empty() {
        return Err(Error::InvalidGithubDraft);
    }

    let body_slice = trim_start(&trimmed[first_break + 1..]);
    Ok(Draft {
        title: title_slice.to_vec(),
        body: body_slice.to_vec(),
    })
}

pub fn publish(workflow: Workflow, draft: Draft) -> Result<PublishResult, std::io::Error> {
    let argv = build_publish_argv(workflow, &draft.title, &draft.body);
    match run_publish_command(&argv) {
        Ok(result) => Ok(publish_result_from_process(result)),
        Err(err) => publish_result_from_run_error(err),
    }
}

fn build_publish_argv(workflow: Workflow, title: &[u8], body: &[u8]) -> Vec<Vec<u8>> {
    let mut argv: Vec<Vec<u8>> = Vec::new();
    argv.push(b"gh".to_vec());
    argv.push(match workflow {
        Workflow::PullRequest => b"pr".to_vec(),
        Workflow::Issue => b"issue".to_vec(),
    });
    argv.push(b"create".to_vec());
    argv.push(b"--title".to_vec());
    argv.push(title.to_vec());
    argv.push(b"--body".to_vec());
    argv.push(body.to_vec());
    argv
}

fn run_publish_command(argv: &[Vec<u8>]) -> std::io::Result<CompletedProcess> {
    let output = run_argv(argv)?;
    Ok(CompletedProcess {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn publish_result_from_run_error(err: std::io::Error) -> Result<PublishResult, std::io::Error> {
    if err.kind() == std::io::ErrorKind::NotFound {
        Ok(PublishResult {
            ok: false,
            text: b"gh CLI not found in PATH".to_vec(),
        })
    } else {
        Err(err)
    }
}

fn publish_result_from_process(result: CompletedProcess) -> PublishResult {
    let CompletedProcess {
        exit_code,
        stdout,
        stderr,
    } = result;

    if exit_code != 0 {
        let stderr_trimmed = trim(&stderr);
        if !stderr_trimmed.is_empty() {
            return PublishResult {
                ok: false,
                text: stderr_trimmed.to_vec(),
            };
        }
        return PublishResult {
            ok: false,
            text: b"gh command failed".to_vec(),
        };
    }

    let stdout_trimmed = trim(&stdout);
    if stdout_trimmed.is_empty() {
        return PublishResult {
            ok: true,
            text: b"created successfully".to_vec(),
        };
    }
    if stdout_trimmed.len() == stdout.len() {
        return PublishResult {
            ok: true,
            text: stdout,
        };
    }
    PublishResult {
        ok: true,
        text: stdout_trimmed.to_vec(),
    }
}

fn run_argv(argv: &[Vec<u8>]) -> std::io::Result<std::process::Output> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut cmd = Command::new(std::ffi::OsStr::from_bytes(&argv[0]));
        for arg in &argv[1..] {
            cmd.arg(std::ffi::OsStr::from_bytes(arg));
        }
        return cmd.output();
    }
    #[cfg(not(unix))]
    {
        let mut cmd = Command::new(std::ffi::OsString::from(
            String::from_utf8_lossy(&argv[0]).into_owned(),
        ));
        for arg in &argv[1..] {
            cmd.arg(std::ffi::OsString::from(
                String::from_utf8_lossy(arg).into_owned(),
            ));
        }
        return cmd.output();
    }
}

fn trim(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n')) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn trim_start(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n')) {
        bytes = &bytes[1..];
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_draft_extracts_title_and_body() {
        let d = parse_draft(b"Title\n\n## Summary\nhello").unwrap();
        assert_eq!(d.title, b"Title");
        assert_eq!(d.body, b"## Summary\nhello");
    }

    #[test]
    fn parse_draft_rejects_whitespace_only_input() {
        assert_eq!(
            parse_draft(b" \n\t ").unwrap_err(),
            Error::InvalidGithubDraft
        );
    }

    #[test]
    fn parse_draft_accepts_title_only_input() {
        let d = parse_draft(b"Title only").unwrap();
        assert_eq!(d.title, b"Title only");
        assert_eq!(d.body, b"");
    }

    #[test]
    fn parse_draft_trims_outer_whitespace_and_preserves_body_markdown() {
        let d = parse_draft(b" \r\n  Title with space  \r\n\r\n  - one\r\n  - two\r\n\n").unwrap();
        assert_eq!(d.title, b"Title with space");
        assert_eq!(d.body, b"- one\r\n  - two");
    }

    #[test]
    fn publish_argv_maps_pull_request_workflow() {
        let argv = build_publish_argv(Workflow::PullRequest, b"A title", b"Body text");
        assert_eq!(argv.len(), 7);
        assert_eq!(argv[0], b"gh");
        assert_eq!(argv[1], b"pr");
        assert_eq!(argv[2], b"create");
        assert_eq!(argv[3], b"--title");
        assert_eq!(argv[4], b"A title");
        assert_eq!(argv[5], b"--body");
        assert_eq!(argv[6], b"Body text");
    }

    #[test]
    fn publish_argv_maps_issue_workflow() {
        let argv = build_publish_argv(Workflow::Issue, b"A title", b"Body text");
        assert_eq!(argv.len(), 7);
        assert_eq!(argv[0], b"gh");
        assert_eq!(argv[1], b"issue");
        assert_eq!(argv[2], b"create");
        assert_eq!(argv[3], b"--title");
        assert_eq!(argv[4], b"A title");
        assert_eq!(argv[5], b"--body");
        assert_eq!(argv[6], b"Body text");
    }

    #[test]
    fn publish_maps_missing_gh_executable_to_handled_failure() {
        let result = publish_result_from_run_error(std::io::Error::from(
            std::io::ErrorKind::NotFound,
        ))
        .unwrap();
        assert!(!result.ok);
        assert_eq!(result.text, b"gh CLI not found in PATH");
    }

    #[test]
    fn publish_returns_trimmed_stderr_for_non_zero_exit() {
        let result = publish_result_from_process(CompletedProcess {
            exit_code: 1,
            stdout: b"ignored".to_vec(),
            stderr: b" \nfailed to create\n ".to_vec(),
        });
        assert!(!result.ok);
        assert_eq!(result.text, b"failed to create");
    }

    #[test]
    fn publish_returns_fallback_for_non_zero_exit_without_stderr() {
        let result = publish_result_from_process(CompletedProcess {
            exit_code: 1,
            stdout: b"ignored".to_vec(),
            stderr: b" \n\t ".to_vec(),
        });
        assert!(!result.ok);
        assert_eq!(result.text, b"gh command failed");
    }

    #[test]
    fn publish_returns_success_fallback_for_empty_stdout() {
        let result = publish_result_from_process(CompletedProcess {
            exit_code: 0,
            stdout: b" \n\t ".to_vec(),
            stderr: Vec::new(),
        });
        assert!(result.ok);
        assert_eq!(result.text, b"created successfully");
    }

    #[test]
    fn publish_trims_padded_stdout() {
        let result = publish_result_from_process(CompletedProcess {
            exit_code: 0,
            stdout: b" \nhttps://github.com/vercel-labs/fx/pull/1\n ".to_vec(),
            stderr: Vec::new(),
        });
        assert!(result.ok);
        assert_eq!(result.text, b"https://github.com/vercel-labs/fx/pull/1");
    }

    #[test]
    fn publish_transfers_already_trimmed_stdout_ownership() {
        let stdout = b"https://github.com/vercel-labs/fx/issues/2".to_vec();
        let ptr = stdout.as_ptr();
        let result = publish_result_from_process(CompletedProcess {
            exit_code: 0,
            stdout,
            stderr: Vec::new(),
        });
        assert!(result.ok);
        assert_eq!(result.text.as_ptr(), ptr);
        assert_eq!(result.text, b"https://github.com/vercel-labs/fx/issues/2");
    }

}
