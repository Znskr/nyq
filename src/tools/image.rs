//! Images: the `view_image` tool and `@path` attachments in user messages.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

use super::{Registry, Tool, ToolCtx, ToolOutput};
use crate::types::{Image, Item, ToolSpec};

const MAX_BYTES: u64 = 20 * 1024 * 1024;
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

pub fn register(r: &mut Registry) {
    r.add(Box::new(ViewImage));
}

/// Media type from the file's magic bytes.
fn media_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

pub fn load(path: &Path) -> Result<Image> {
    let bytes = super::safeio::read_capped(path, MAX_BYTES).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!("{} is {} MB; images are limited to 20 MB", path.display(), (bytes.len() as u64) / (1024 * 1024));
    }
    let Some(media_type) = media_type(&bytes) else {
        bail!("{} is not a PNG, JPEG, GIF or WebP image", path.display());
    };
    Ok(Image { media_type: media_type.into(), data: base64::engine::general_purpose::STANDARD.encode(&bytes) })
}

fn resolve(cwd: &Path, raw: &str) -> PathBuf {
    let raw = raw.strip_prefix("file://").unwrap_or(raw);
    let expanded = match raw.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(raw),
    };
    cwd.join(expanded)
}

/// Build a user message, attaching images referenced as `@path` or as a bare
/// path to an existing image file (as terminals paste on drag-and-drop).
/// Returns the message and the attached paths, or an error for an `@path`
/// image that can't be loaded.
pub fn user_message(text: &str, cwd: &Path) -> Result<(Item, Vec<String>)> {
    let mut images = Vec::new();
    let mut attached = Vec::new();
    for token in text.split_whitespace() {
        let explicit = token.starts_with('@');
        let raw = token.trim_start_matches('@').trim_matches(|c| c == '\'' || c == '"');
        let is_image = Path::new(raw)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if !is_image || attached.iter().any(|a| a == raw) {
            continue;
        }
        let path = resolve(cwd, raw);
        if !explicit && !path.is_file() {
            continue;
        }
        images.push(load(&path)?);
        attached.push(raw.to_string());
    }
    Ok((Item::User { text: text.to_string(), images }, attached))
}

/// Like [`user_message`], but a failed `@image` keeps the text. The third
/// value is the load error, so the caller can warn and still send the message.
pub fn user_turn(text: &str, cwd: &Path) -> (Item, Vec<String>, Option<String>) {
    match user_message(text, cwd) {
        Ok((item, attached)) => (item, attached, None),
        Err(e) => (Item::user(text), Vec::new(), Some(format!("{e:#}"))),
    }
}

struct ViewImage;

#[async_trait::async_trait]
impl Tool for ViewImage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "view_image".into(),
            description: "Look at an image file (PNG, JPEG, GIF, WebP) from the workspace, e.g. a screenshot or diagram.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "Image file path"}},
                "required": ["path"]
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        args["path"].as_str().unwrap_or("").to_string()
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(raw) = args["path"].as_str() else {
            return ToolOutput::err("view_image: missing `path`");
        };
        match load(&resolve(&ctx.cwd, raw)) {
            Ok(image) => ToolOutput { images: vec![image], ..ToolOutput::ok(format!("Loaded {raw}; the image follows.")) },
            Err(e) => ToolOutput::err(format!("view_image: {e:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];

    #[test]
    fn attaches_explicit_and_existing_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), PNG).unwrap();
        let (item, attached) = user_message("what is in @shot.png and shot.png? not missing.png", dir.path()).unwrap();
        assert_eq!(attached, vec!["shot.png"]);
        let Item::User { images, text } = item else { panic!() };
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        assert!(text.contains("@shot.png"));
        assert!(images[0].data_url().starts_with("data:image/png;base64,iVBOR"));
        let (_, attached, warning) = user_turn("see @shot.png", dir.path());
        assert!(warning.is_none() && attached == vec!["shot.png"]);
    }

    #[test]
    fn explicit_missing_or_non_image_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(user_message("see @nope.png", dir.path()).is_err());
        std::fs::write(dir.path().join("fake.jpg"), b"text").unwrap();
        assert!(user_message("see @fake.jpg", dir.path()).is_err());
        assert!(user_message("plain text only", dir.path()).unwrap().1.is_empty());
        let (item, attached, warning) = user_turn("long note @nope.png", dir.path());
        let Item::User { text, images } = item else { panic!() };
        assert_eq!(text, "long note @nope.png");
        assert!(images.is_empty() && attached.is_empty());
        assert!(warning.unwrap().contains("nope.png"));
        #[cfg(unix)]
        if Path::new("/dev/zero").exists() {
            let err = load(Path::new("/dev/zero")).unwrap_err();
            assert!(format!("{err:#}").contains("regular"), "{err:#}");
        }
    }

    #[tokio::test]
    async fn view_image_returns_image() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), PNG).unwrap();
        let ctx = ToolCtx { cwd: dir.path().into(), ..Default::default() };
        let out = ViewImage.run(json!({"path": "a.png"}), &ctx).await;
        assert!(!out.is_error);
        assert_eq!(out.images.len(), 1);
        assert!(ViewImage.run(json!({"path": "b.png"}), &ctx).await.is_error);
    }
}
