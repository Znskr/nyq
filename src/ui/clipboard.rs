//! Reading an image from the system clipboard with the platform's tools
//! (wl-paste, xclip, pngpaste, PowerShell), when they are installed.

use std::process::{Command, Stdio};

use crate::types::Image;

/// Run a command and return its stdout if it succeeded.
fn output(cmd: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
}

/// The best image MIME type among the clipboard's offered types.
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
pub fn pick_type(types: &str) -> Option<&'static str> {
    ["image/png", "image/jpeg", "image/webp", "image/gif"].into_iter().find(|t| types.lines().any(|l| l.trim() == *t))
}

/// Raw image bytes from the clipboard. Blocking; call off the UI thread.
/// Errors name the reason (no tool installed, no image).
pub fn read_image() -> Result<Image, String> {
    let bytes = read_bytes()?;
    let claimed = crate::tools::image::media_type(&bytes).ok_or("the clipboard doesn't hold a PNG, JPEG, GIF or WebP image")?;
    crate::tools::image::image_from_bytes(&bytes, claimed).map_err(|e| format!("{e:#}"))
}

#[cfg(target_os = "macos")]
fn read_bytes() -> Result<Vec<u8>, String> {
    if let Some(bytes) = output("pngpaste", &["-"]) {
        return Ok(bytes);
    }
    // Without pngpaste: AppleScript can hand the PNG data over as a temp file.
    let tmp = std::env::temp_dir().join(format!("nyq-clip-{}.png", uuid::Uuid::new_v4()));
    let script = format!(
        "set f to open for access POSIX file \"{}\" with write permission\ntry\nwrite (the clipboard as «class PNGf») to f\nend try\nclose access f",
        tmp.display()
    );
    let _ = output("osascript", &["-e", &script]);
    let bytes = std::fs::read(&tmp).ok().filter(|b| !b.is_empty());
    let _ = std::fs::remove_file(&tmp);
    bytes.ok_or_else(|| "no image found on clipboard".into())
}

#[cfg(windows)]
fn read_bytes() -> Result<Vec<u8>, String> {
    use base64::Engine;
    let script = "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; \
        $i = [Windows.Forms.Clipboard]::GetImage(); if ($i) { $m = New-Object IO.MemoryStream; \
        $i.Save($m, [Drawing.Imaging.ImageFormat]::Png); [Convert]::ToBase64String($m.ToArray()) }";
    let out = output("powershell", &["-NoProfile", "-STA", "-Command", script]).ok_or("no image found on clipboard")?;
    let text = String::from_utf8_lossy(&out);
    base64::engine::general_purpose::STANDARD.decode(text.trim()).map_err(|_| "no image found on clipboard".to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn read_bytes() -> Result<Vec<u8>, String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut tried = false;
    if wayland {
        if let Some(types) = output("wl-paste", &["--list-types"]) {
            tried = true;
            if let Some(t) = pick_type(&String::from_utf8_lossy(&types)) {
                if let Some(bytes) = output("wl-paste", &["--no-newline", "--type", t]) {
                    return Ok(bytes);
                }
            }
        }
    }
    if let Some(types) = output("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]) {
        tried = true;
        if let Some(t) = pick_type(&String::from_utf8_lossy(&types)) {
            if let Some(bytes) = output("xclip", &["-selection", "clipboard", "-t", t, "-o"]) {
                return Ok(bytes);
            }
        }
    }
    if tried {
        Err("no image found on clipboard".into())
    } else {
        Err(format!("reading the clipboard needs {}", if wayland { "wl-paste (wl-clipboard) or xclip" } else { "xclip" }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_an_image_type() {
        assert_eq!(pick_type("TARGETS\ntext/plain\nimage/jpeg\nimage/png\n"), Some("image/png"));
        assert_eq!(pick_type("text/plain\nimage/webp"), Some("image/webp"));
        assert_eq!(pick_type("text/plain\nimage/pngx"), None);
    }
}
