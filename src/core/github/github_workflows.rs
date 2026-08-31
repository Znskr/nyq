//! Prompt builders for GitHub PR/issue drafting workflows (ported 1:1 from
//! zig-reference/src/core/github/github_workflows.zig).

use crate::core::github::git_context;
use crate::core::shared::types::ConversationLanguage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Workflow {
    PullRequest,
    Issue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    NotGitRepository,
}

struct PromptSnapshot<'a> {
    in_git_repo: bool,
    text: &'a [u8],
}

pub fn build_prompt(
    workflow: Workflow,
    language: ConversationLanguage,
    context: &[u8],
) -> Result<Vec<u8>, Error> {
    let snapshot = git_context::snapshot();
    build_prompt_from_snapshot(
        workflow,
        language,
        context,
        PromptSnapshot {
            in_git_repo: snapshot.in_git_repo,
            text: &snapshot.text,
        },
    )
}

fn build_prompt_from_snapshot(
    workflow: Workflow,
    _language: ConversationLanguage,
    context: &[u8],
    snapshot: PromptSnapshot<'_>,
) -> Result<Vec<u8>, Error> {
    let trimmed = trim(context);
    if workflow == Workflow::PullRequest && !snapshot.in_git_repo {
        return Err(Error::NotGitRepository);
    }
    Ok(match workflow {
        Workflow::PullRequest => build_pull_request_prompt(trimmed, snapshot.text),
        Workflow::Issue => build_issue_prompt(trimmed, snapshot.text),
    })
}

fn build_pull_request_prompt(context: &[u8], snapshot_text: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(
        b"Draft a GitHub pull request for the current branch. Reply in the same natural language as the current session.",
    );
    if !context.is_empty() {
        out.extend_from_slice(b" Additional context: ");
        out.extend_from_slice(context);
        out.push(b'.');
    }
    out.extend_from_slice(
        b" Use this prepared git snapshot first and avoid shell commands unless they are truly necessary:\n\n",
    );
    out.extend_from_slice(snapshot_text);
    out.extend_from_slice(
        b"\n\nIf you need more context, read relevant files. Return only: Title, blank line, then a GitHub-flavored Markdown body with sections '## Summary' and '## Testing'. Do not create the PR with gh or publish anything unless I explicitly ask you to.",
    );
    out
}

fn build_issue_prompt(context: &[u8], snapshot_text: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(
        b"Draft a GitHub issue from the current context. Reply in the same natural language as the current session.",
    );
    if !context.is_empty() {
        out.extend_from_slice(b" Additional context: ");
        out.extend_from_slice(context);
        out.push(b'.');
    }
    out.extend_from_slice(
        b" Use this prepared git snapshot first and avoid shell commands unless they are truly necessary:\n\n",
    );
    out.extend_from_slice(snapshot_text);
    out.extend_from_slice(
        b"\n\nIf you need more context, inspect relevant files, errors, or logs. Return only: Title, blank line, then a GitHub-flavored Markdown body with sections '## Summary', '## Steps to Reproduce', '## Expected', and '## Actual'. Do not create the issue with gh or publish anything unless I explicitly ask you to.",
    );
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_contains(haystack: &[u8], needle: &[u8]) {
        assert!(
            haystack.windows(needle.len()).any(|w| w == needle),
            "expected {:?} to contain {:?}",
            String::from_utf8_lossy(haystack),
            String::from_utf8_lossy(needle),
        );
    }

    fn expect_not_contains(haystack: &[u8], needle: &[u8]) {
        assert!(
            !haystack.windows(needle.len()).any(|w| w == needle),
            "expected {:?} to not contain {:?}",
            String::from_utf8_lossy(haystack),
            String::from_utf8_lossy(needle),
        );
    }

    #[test]
    fn build_pull_request_prompt_includes_additional_context() {
        let prompt = match build_prompt(Workflow::PullRequest, ConversationLanguage::default(), b"ready for review") {
            Ok(p) => p,
            Err(Error::NotGitRepository) => return,
        };
        expect_contains(&prompt, b"ready for review");
        expect_contains(&prompt, b"## Summary");
    }

    #[test]
    fn build_issue_prompt_defaults_cleanly_without_context() {
        let prompt = build_prompt(Workflow::Issue, ConversationLanguage::default(), b"").unwrap();
        expect_contains(&prompt, b"GitHub issue");
        expect_contains(&prompt, b"## Steps to Reproduce");
    }

    #[test]
    fn pull_request_prompt_preserves_active_context_and_section_contract() {
        let prompt = build_prompt_from_snapshot(
            Workflow::PullRequest,
            ConversationLanguage::literal(b"es"),
            b" ready for review \n",
            PromptSnapshot {
                in_git_repo: true,
                text: b"Git snapshot\nBranch: feature\n",
            },
        )
        .unwrap();

        expect_contains(&prompt, b"Additional context: ready for review.");
        expect_contains(&prompt, b"Git snapshot\nBranch: feature\n");
        expect_contains(&prompt, b"## Summary");
        expect_contains(&prompt, b"## Testing");
        expect_not_contains(&prompt, b"## Steps to Reproduce");
        expect_contains(
            &prompt,
            b"Do not create the PR with gh or publish anything unless I explicitly ask you to.",
        );
    }

    #[test]
    fn pull_request_prompt_requires_a_git_repository() {
        assert_eq!(
            build_prompt_from_snapshot(
                Workflow::PullRequest,
                ConversationLanguage::default(),
                b"",
                PromptSnapshot {
                    in_git_repo: false,
                    text: b"Git snapshot\nBranch: unavailable\n",
                },
            )
            .unwrap_err(),
            Error::NotGitRepository,
        );
    }

    #[test]
    fn issue_prompt_works_outside_git_and_omits_empty_context_clause() {
        let prompt = build_prompt_from_snapshot(
            Workflow::Issue,
            ConversationLanguage::default(),
            b" \t\r\n",
            PromptSnapshot {
                in_git_repo: false,
                text: b"Git snapshot\nBranch: unavailable\n",
            },
        )
        .unwrap();

        expect_contains(&prompt, b"Draft a GitHub issue from the current context.");
        expect_contains(&prompt, b"Git snapshot\nBranch: unavailable\n");
        expect_contains(&prompt, b"## Summary");
        expect_contains(&prompt, b"## Steps to Reproduce");
        expect_contains(&prompt, b"## Expected");
        expect_contains(&prompt, b"## Actual");
        expect_not_contains(&prompt, b"Additional context:");
        expect_contains(
            &prompt,
            b"Do not create the issue with gh or publish anything unless I explicitly ask you to.",
        );
    }
}
