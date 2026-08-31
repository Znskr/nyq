//! Bounded web-fetch content classification for the `web_fetch` tool.
//!
//! Conservative port of `tools/web/content.zig`. The three tiny items that
//! `content.zig` re-exports from `core/tooling/web_fetch_content_contract.zig`
//! (the conversion cap, `Kind`, and `Classification`) are inlined here so the
//! module remains usable before the tooling subsystem is ported.

use crate::core::shared::text_utils;

/// Maximum number of bytes accepted as converted textual content.
#[allow(non_upper_case_globals)]
pub const max_converted_content_bytes: usize = 10 * 1024 * 1024;

/// Class of fetched content after MIME and body inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Html,
    Binary,
}

/// Owned classification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub kind: Kind,
    pub mime_type: String,
    pub declared: bool,
}

/// Classify fetched content deterministically.
///
/// When a `Content-Type` header is declared, the normalized MIME type decides
/// the kind and the result is marked `declared`. Otherwise the body is sniffed
/// for model-safe text and falls back to binary.
pub fn classify(content_type: Option<&str>, body: &[u8]) -> Classification {
    if let Some(declared) = content_type {
        let normalized = normalized_mime(declared);
        return Classification {
            kind: declared_kind(&normalized),
            mime_type: normalized,
            declared: true,
        };
    }

    if text_utils::is_model_safe_text(body) {
        return Classification {
            kind: Kind::Text,
            mime_type: "text/plain".to_string(),
            declared: false,
        };
    }

    Classification {
        kind: Kind::Binary,
        mime_type: "application/octet-stream".to_string(),
        declared: false,
    }
}

fn normalized_mime(content_type: &str) -> String {
    let parameter_start = content_type.find(';').unwrap_or(content_type.len());
    let trimmed = content_type[..parameter_start]
        .trim_matches(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n'));
    if trimmed.is_empty() {
        return "application/octet-stream".to_string();
    }
    trimmed.to_ascii_lowercase()
}

fn declared_kind(mime: &str) -> Kind {
    if mime == "text/html" || mime == "application/xhtml+xml" {
        return Kind::Html;
    }
    if mime.starts_with("text/") {
        return Kind::Text;
    }
    if mime == "application/json"
        || mime == "application/xml"
        || mime == "application/javascript"
        || mime == "application/x-javascript"
    {
        return Kind::Text;
    }
    if mime.starts_with("application/") && mime.ends_with("+json") {
        return Kind::Text;
    }
    if mime.starts_with("application/") && mime.ends_with("+xml") {
        return Kind::Text;
    }
    Kind::Binary
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        content_type: Option<&'static str>,
        body: &'static [u8],
        kind: Kind,
        mime_type: &'static str,
        declared: bool,
    }

    #[test]
    fn web_fetch_classifies_declared_mime_and_missing_content_type_deterministically() {
        let cases = [
            Case {
                content_type: Some("Text/HTML; Charset=UTF-8"),
                body: b"hello",
                kind: Kind::Html,
                mime_type: "text/html",
                declared: true,
            },
            Case {
                content_type: Some("TEXT/PLAIN"),
                body: b"hello",
                kind: Kind::Text,
                mime_type: "text/plain",
                declared: true,
            },
            Case {
                content_type: Some("application/json; charset=utf-8"),
                body: b"hello",
                kind: Kind::Text,
                mime_type: "application/json",
                declared: true,
            },
            Case {
                content_type: Some("application/activity+json"),
                body: b"hello",
                kind: Kind::Text,
                mime_type: "application/activity+json",
                declared: true,
            },
            Case {
                content_type: Some("application/xml"),
                body: b"hello",
                kind: Kind::Text,
                mime_type: "application/xml",
                declared: true,
            },
            Case {
                content_type: Some("application/rss+xml"),
                body: b"hello",
                kind: Kind::Text,
                mime_type: "application/rss+xml",
                declared: true,
            },
            Case {
                content_type: Some("application/javascript"),
                body: b"hello",
                kind: Kind::Text,
                mime_type: "application/javascript",
                declared: true,
            },
            Case {
                content_type: Some("application/x-javascript"),
                body: b"hello",
                kind: Kind::Text,
                mime_type: "application/x-javascript",
                declared: true,
            },
            Case {
                content_type: Some("application/pdf"),
                body: b"hello",
                kind: Kind::Binary,
                mime_type: "application/pdf",
                declared: true,
            },
            Case {
                content_type: None,
                body: b"hello",
                kind: Kind::Text,
                mime_type: "text/plain",
                declared: false,
            },
            Case {
                content_type: None,
                body: b"bad\x00text",
                kind: Kind::Binary,
                mime_type: "application/octet-stream",
                declared: false,
            },
        ];

        for case in cases {
            let classification = classify(case.content_type, case.body);
            assert_eq!(case.kind, classification.kind);
            assert_eq!(case.mime_type, classification.mime_type);
            assert_eq!(case.declared, classification.declared);
        }
    }
}
