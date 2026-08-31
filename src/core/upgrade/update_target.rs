//! Update-channel target parsing (ported 1:1 from
//! zig-reference/src/core/upgrade/update_target.zig).

use crate::core::mcp::json_value::{self, Value};
use std::cmp::Ordering;
use std::fmt;

pub const max_manifest_bytes: usize = 4096;
pub const max_version_bytes: usize = 32;
pub const max_revision_bytes: usize = 64;
const min_revision_bytes: usize = 7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Channel {
    Stable,
    Dev,
}

impl Channel {
    pub fn parse(raw: &[u8]) -> Option<Channel> {
        if raw.eq_ignore_ascii_case(b"stable") {
            Some(Channel::Stable)
        } else if raw.eq_ignore_ascii_case(b"dev") {
            Some(Channel::Dev)
        } else {
            None
        }
    }

    pub fn label(self) -> &'static [u8] {
        match self {
            Channel::Stable => b"stable",
            Channel::Dev => b"dev",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CurrentBuild<'a> {
    pub channel: Channel,
    pub version: &'a [u8],
    pub revision: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Stable {
        version: Vec<u8>,
        artifact_ref: Vec<u8>,
    },
    Dev {
        version: Vec<u8>,
        revision: Vec<u8>,
        artifact_ref: Vec<u8>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidVersion,
    ManifestTooLarge,
    InvalidManifest,
    NoSpaceLeft,
}

impl Target {
    pub fn init_stable(raw_version: &[u8]) -> Result<Target, Error> {
        let trimmed = trim_ascii(raw_version);
        let normalized_version = normalize_version(trimmed);
        if !valid_version(normalized_version) {
            return Err(Error::InvalidVersion);
        }
        Ok(Target::Stable {
            version: normalized_version.to_vec(),
            artifact_ref: trimmed.to_vec(),
        })
    }

    pub fn parse_dev_manifest(bytes: &[u8]) -> Result<Target, Error> {
        if bytes.len() > max_manifest_bytes {
            return Err(Error::ManifestTooLarge);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidManifest)?;
        let parsed = json_value::parse_json(text).map_err(|_| Error::InvalidManifest)?;
        let object = match parsed {
            Value::Object(object) => object,
            _ => return Err(Error::InvalidManifest),
        };
        let version_value = object.get("version").ok_or(Error::InvalidManifest)?;
        let revision_value = object.get("commit").ok_or(Error::InvalidManifest)?;
        let (version_string, revision_string) = match (version_value, revision_value) {
            (Value::String(version), Value::String(revision)) => (version.as_str(), revision.as_str()),
            _ => return Err(Error::InvalidManifest),
        };

        let normalized_version = normalize_version(version_string.as_bytes());
        let manifest_revision = revision_string.as_bytes();
        if !valid_version(normalized_version) || !valid_revision(manifest_revision) {
            return Err(Error::InvalidManifest);
        }

        let mut artifact_ref = Vec::with_capacity(4 + manifest_revision.len());
        artifact_ref.extend_from_slice(b"dev/");
        artifact_ref.extend_from_slice(manifest_revision);
        Ok(Target::Dev {
            version: normalized_version.to_vec(),
            revision: manifest_revision.to_vec(),
            artifact_ref,
        })
    }

    pub fn deinit(&mut self) {
        match self {
            Target::Stable {
                version,
                artifact_ref,
            } => {
                version.clear();
                artifact_ref.clear();
            }
            Target::Dev {
                version,
                revision,
                artifact_ref,
            } => {
                version.clear();
                revision.clear();
                artifact_ref.clear();
            }
        }
    }

    pub fn channel(&self) -> Channel {
        match self {
            Target::Stable { .. } => Channel::Stable,
            Target::Dev { .. } => Channel::Dev,
        }
    }

    pub fn version(&self) -> &[u8] {
        match self {
            Target::Stable { version, .. } | Target::Dev { version, .. } => version,
        }
    }

    pub fn revision(&self) -> Option<&[u8]> {
        match self {
            Target::Stable { .. } => None,
            Target::Dev { revision, .. } => Some(revision),
        }
    }

    pub fn artifact_ref(&self) -> &[u8] {
        match self {
            Target::Stable { artifact_ref, .. } | Target::Dev { artifact_ref, .. } => artifact_ref,
        }
    }

    pub fn should_install(&self, current: CurrentBuild<'_>) -> bool {
        if self.channel() != current.channel {
            return true;
        }
        match self {
            Target::Stable { version, .. } => {
                compare_versions(version, current.version) == Ordering::Greater
            }
            Target::Dev { revision, .. } => !revisions_equal(revision, current.revision),
        }
    }

    pub fn write_display_label<'a>(&self, out: &'a mut [u8]) -> Result<&'a [u8], Error> {
        match self {
            Target::Stable { version, .. } => {
                if version.len() > out.len() {
                    return Err(Error::NoSpaceLeft);
                }
                out[..version.len()].copy_from_slice(version);
                Ok(&out[..version.len()])
            }
            Target::Dev { revision, .. } => {
                let short = short_revision(revision);
                let needed = 4 + short.len();
                if needed > out.len() {
                    return Err(Error::NoSpaceLeft);
                }
                out[..4].copy_from_slice(b"dev ");
                out[4..needed].copy_from_slice(short);
                Ok(&out[..needed])
            }
        }
    }
}

pub fn normalize_version(raw: &[u8]) -> &[u8] {
    if !raw.is_empty() && raw[0] == b'v' {
        &raw[1..]
    } else {
        raw
    }
}

pub fn compare_versions(a: &[u8], b: &[u8]) -> Ordering {
    let av = parse_version_parts(a);
    let bv = parse_version_parts(b);
    if av[0] != bv[0] {
        return av[0].cmp(&bv[0]);
    }
    if av[1] != bv[1] {
        return av[1].cmp(&bv[1]);
    }
    av[2].cmp(&bv[2])
}

fn valid_version(raw: &[u8]) -> bool {
    if raw.is_empty() || raw.len() > max_version_bytes {
        return false;
    }
    let mut count = 0usize;
    for part in raw.split(|&byte| byte == b'.') {
        if count == 3 || part.is_empty() {
            return false;
        }
        if !part.iter().all(u8::is_ascii_digit) {
            return false;
        }
        if parse_u32_decimal(part).is_none() {
            return false;
        }
        count += 1;
    }
    count == 3
}

fn valid_revision(raw: &[u8]) -> bool {
    if raw.len() < min_revision_bytes || raw.len() > max_revision_bytes {
        return false;
    }
    raw.iter().all(u8::is_ascii_hexdigit)
}

fn parse_version_parts(raw: &[u8]) -> [u32; 3] {
    let mut values = [0u32; 3];
    let mut parts = normalize_version(raw).split(|&byte| byte == b'.');
    for value in &mut values {
        let Some(part) = parts.next() else {
            break;
        };
        *value = parse_u32_decimal(part).unwrap_or(0);
    }
    values
}

fn parse_u32_decimal(part: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(part).ok()?;
    text.parse::<u32>().ok()
}

fn revisions_equal(full: &[u8], current: &[u8]) -> bool {
    if current == b"unknown" {
        return false;
    }
    let common_len = full.len().min(current.len());
    if common_len < min_revision_bytes {
        return false;
    }
    full[..common_len].eq_ignore_ascii_case(&current[..common_len])
}

fn short_revision(revision: &[u8]) -> &[u8] {
    &revision[..revision.len().min(12)]
}

fn trim_ascii(raw: &[u8]) -> &[u8] {
    let start = raw
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(raw.len());
    let end = raw
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .map_or(start, |index| index + 1);
    &raw[start..end]
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidVersion => write!(f, "InvalidVersion"),
            Error::ManifestTooLarge => write!(f, "ManifestTooLarge"),
            Error::InvalidManifest => write!(f, "InvalidManifest"),
            Error::NoSpaceLeft => write!(f, "NoSpaceLeft"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_parsing_accepts_only_stable_and_dev() {
        assert_eq!(Channel::parse(b"stable"), Some(Channel::Stable));
        assert_eq!(Channel::parse(b"DEV"), Some(Channel::Dev));
        assert_eq!(Channel::parse(b"nightly"), None);
    }

    #[test]
    fn dev_manifest_creates_a_bounded_immutable_target() {
        let mut target = Target::parse_dev_manifest(
            br#"{"version":"0.3.62","commit":"0123456789abcdef0123456789abcdef01234567"}"#,
        )
        .unwrap();

        assert_eq!(target.channel(), Channel::Dev);
        assert_eq!(target.version(), b"0.3.62");
        assert_eq!(
            target.artifact_ref(),
            b"dev/0123456789abcdef0123456789abcdef01234567"
        );
        let mut label_buf = [0u8; 32];
        assert_eq!(
            target.write_display_label(&mut label_buf).unwrap(),
            b"dev 0123456789ab"
        );
        target.deinit();
    }

    #[test]
    fn dev_manifest_rejects_malformed_and_oversized_external_data() {
        assert_eq!(
            Target::parse_dev_manifest(br#"{"version":"0.3.62","commit":"../escape"}"#),
            Err(Error::InvalidManifest)
        );
        assert_eq!(
            Target::parse_dev_manifest(br#"{"version":"0.3","commit":"0123456"}"#),
            Err(Error::InvalidManifest)
        );
        let oversized = vec![b' '; max_manifest_bytes + 1];
        assert_eq!(
            Target::parse_dev_manifest(&oversized),
            Err(Error::ManifestTooLarge)
        );
    }

    #[test]
    fn stable_release_ordering_rejects_older_targets_and_preserves_channel_switching() {
        let mut older = Target::init_stable(b"v0.0.1").unwrap();
        let newer_current = CurrentBuild {
            channel: Channel::Stable,
            version: b"0.0.2",
            revision: b"0123456789ab",
        };

        assert!(!older.should_install(newer_current));
        assert!(!older.should_install(CurrentBuild {
            channel: Channel::Stable,
            version: b"0.4.5",
            revision: b"0123456789ab",
        }));
        assert!(older.should_install(CurrentBuild {
            channel: Channel::Dev,
            version: b"0.0.2",
            revision: b"abcdef012345",
        }));
        older.deinit();
    }

    #[test]
    fn target_freshness_uses_version_for_stable_and_revision_for_dev() {
        let mut stable = Target::init_stable(b"v0.3.63").unwrap();
        let stable_current = CurrentBuild {
            channel: Channel::Stable,
            version: b"0.3.62",
            revision: b"0123456789ab",
        };
        assert!(stable.should_install(stable_current));

        let mut dev = Target::parse_dev_manifest(
            br#"{"version":"0.3.62","commit":"abcdef0123456789abcdef0123456789abcdef01"}"#,
        )
        .unwrap();
        assert!(dev.should_install(stable_current));
        assert!(!dev.should_install(CurrentBuild {
            channel: Channel::Dev,
            version: b"0.3.62",
            revision: b"abcdef012345",
        }));
        stable.deinit();
        dev.deinit();
    }
}
