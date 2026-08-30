use std::io::{self, Write};

/// Rewrites `json` without insignificant whitespace and with raw control
/// bytes inside string literals escaped, so an NDJSON frame stays on one line.
/// All other bytes pass through unchanged, preserving valid input tokens.
pub fn write_compact<W: Write>(writer: &mut W, json: &[u8]) -> io::Result<()> {
    let mut in_string = false;
    let mut escape_pending = false;
    for &byte in json {
        if !in_string {
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' => {}
                _ => {
                    if byte == b'"' {
                        in_string = true;
                    }
                    writer.write_all(&[byte])?;
                }
            }
            continue;
        }
        if byte < 0x20 {
            // Raw control bytes are invalid JSON inside a string; escaping
            // them keeps malformed model output inside its NDJSON frame.
            write!(writer, "\\u{:04x}", byte)?;
            escape_pending = false;
            continue;
        }
        writer.write_all(&[byte])?;
        if escape_pending {
            escape_pending = false;
        } else {
            match byte {
                b'\\' => escape_pending = true,
                b'"' => in_string = false,
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_compact;

    #[test]
    fn compact_json_escapes_raw_control_bytes_inside_strings() {
        let mut out = Vec::new();
        write_compact(&mut out, br#"{"k": "a
b"}"#)
            .unwrap();
        assert_eq!(br#"{"k":"a\u000ab"}"#, out.as_slice());
    }

    #[test]
    fn compact_json_escapes_a_raw_control_byte_after_a_backslash() {
        let mut out = Vec::new();
        write_compact(&mut out, b"{\"k\":\"a\\\nb\"}").unwrap();
        assert!(!out.contains(&b'\n'));
        assert_eq!(b"{\"k\":\"a\\\\u000ab\"}", out.as_slice());
    }

    #[test]
    fn compact_json_preserves_escaped_quotes_and_backslashes() {
        let mut out = Vec::new();
        write_compact(
            &mut out,
            br#"{"k": "say \"hi now\"", "p": "x\\", "q": 1}"#,
        )
        .unwrap();
        assert_eq!(
            br#"{"k":"say \"hi now\"","p":"x\\","q":1}"#,
            out.as_slice()
        );
    }
}
