//! Scrub credentials from tool output before the model or the session file
//! sees it. Only distinctive token shapes are replaced, so ordinary source
//! code (`let token = next();`) reads unchanged.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

const MASK: &str = "[redacted]";

static PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        // PEM private key blocks.
        (
            r"-----BEGIN ([A-Z0-9 ]*)PRIVATE KEY-----[\s\S]*?(-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
            "-----BEGIN ${1}PRIVATE KEY-----\n[redacted]\n-----END ${1}PRIVATE KEY-----",
        ),
        // Credentials inside a URL (https://user:password@host/…).
        // Credentials inside a URL (https://user:password@host/…).
        // Credentials inside a URL (https://user:password@host/…).
        (r#"(?i)\b(https?://)[^\s/?#"'`<>@:]+:[^\s/?#"'`<>@]+@"#, "${1}[redacted]@"),
        // OpenAI/Anthropic-style and Stripe keys.
        (r"\b(sk|rk)-[A-Za-z0-9_-]{20,}", MASK),
        (r"\b(sk|pk|rk)_(live|test)_[A-Za-z0-9]{10,}", MASK),
        // GitHub, GitLab and Slack tokens.
        (r"\b(gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{20,})", MASK),
        (r"\bxox[abprs]-[A-Za-z0-9-]{10,}", MASK),
        // AWS access key ids, and secret keys in config/env form.
        (r"\b(AKIA|ASIA)[0-9A-Z]{16}\b", MASK),
        (r"(?i)\b(aws_secret_access_key\s*[=:]\s*)[A-Za-z0-9/+]{40}\b", "${1}[redacted]"),
        // Google API keys.
        (r"\bAIza[0-9A-Za-z_-]{35}\b", MASK),
        // Bearer tokens in headers and commands.
        (r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/-]{16,}=*", "${1}[redacted]"),
        // `NAME_TOKEN=value` lines as printed by `env` or found in `.env`.
        (
            r"(?m)^(\s*(?:\d+\t|[^\r\n]+?[:-]\d+[:-] )?\s*(?:export\s+)?[A-Z0-9_]*(?:API_KEY|APIKEY|TOKEN|SECRET|PASSWORD|PASSWD|PRIVATE_KEY|ACCESS_KEY)[A-Z0-9_]*\s*=\s*)[^\s#]{6,}",
            "${1}[redacted]",
        ),
    ]
    .into_iter()
    .map(|(p, with)| (Regex::new(p).expect("valid secret pattern"), with))
    .collect()
});

/// `text` with credentials replaced by `[redacted]`.
pub(crate) fn scrub(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    for (re, with) in PATTERNS.iter() {
        if let Cow::Owned(s) = re.replace_all(&out, *with) {
            out = Cow::Owned(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_assignments_in_numbered_reads_and_search_results() {
        for prefix in [
            "",
            "     1\t",
            "config.env:1: ",
            "config.env-2- ",
            "dir:with:colons/config.env:12: ",
        ] {
            let input = format!("{prefix}DATABASE_PASSWORD=super_secret_123\n");
            assert_eq!(
                scrub(&input),
                format!("{prefix}DATABASE_PASSWORD=[redacted]\n")
            );
        }
    }

    #[test]
    fn masks_known_token_shapes() {
        let cases = [
            "OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz0123",
            "token ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "github_pat_11ABCDEFG0123456789_abcdefghijklmnop",
            "aws AKIAIOSFODNN7EXAMPLE here",
            "curl -H 'Authorization: Bearer abcdefghijklmnop.qrstuvwx'",
            "xoxb-1234567890-abcdefghij",
            "stripe sk_live_abcdefghijklmnop",
        ];
        for case in cases {
            let out = scrub(case);
            assert!(out.contains("[redacted]"), "{case} -> {out}");
        }
        assert_eq!(
            scrub("export GITHUB_TOKEN=abcdef123456"),
            "export GITHUB_TOKEN=[redacted]"
        );
        assert_eq!(
            scrub("aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            "aws_secret_access_key = [redacted]"
        );
        let pem =
            "a\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nabc\n-----END RSA PRIVATE KEY-----\nb";
        let out = scrub(pem);
        assert!(
            !out.contains("MIIEow") && out.starts_with("a\n") && out.ends_with("\nb"),
            "{out}"
        );
    }

    #[test]
    fn masks_url_credentials_but_not_ports_or_hosts() {
        for case in [
            "git clone https://x-access-token:ghp_abcdefghijklmnop@github.com/o/r.git",
            "curl https://admin:hunter2@example.com/v1",
            "fetch http://user:pa%24ss@data.example.com/x",
        ] {
            let out = scrub(case);
            assert!(
                !out.contains("hunter2") && !out.contains("secretpw") && !out.contains("ghp_"),
                "{case} -> {out}"
            );
            assert!(out.contains("://[redacted]@"), "{case} -> {out}");
        }
        assert_eq!(
            scrub("https://admin:hunter2@example.com/v1"),
            "https://[redacted]@example.com/v1"
        );
        // Ports, plain hosts, IPv6 literals and user-only URLs are untouched.
        for plain in [
            "https://example.com:8443/v1/models",
            "http://[2001:db8::1]:8080/x",
            "https://user@example.com/x",
            "see https://docs.rs/tokio and http://93.184.216.34/",
        ] {
            assert!(
                matches!(scrub(plain), Cow::Borrowed(_)),
                "{plain} -> {}",
                scrub(plain)
            );
        }
    }

    #[test]
    fn leaves_ordinary_code_alone() {
        for text in [
            "let token = tokens.next();",
            "password: String,",
            "commit 4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            "fn bearer(token: &str) -> String",
            "API_KEY=",
            "task-runner sk-short",
        ] {
            assert!(
                matches!(scrub(text), Cow::Borrowed(_)),
                "{text} -> {}",
                scrub(text)
            );
        }
    }
}
