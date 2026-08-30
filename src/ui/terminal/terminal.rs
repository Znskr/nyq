#![allow(non_snake_case)]

use crate::core::shared::types::Layout;
use std::fmt::Write as _;

pub const interactive_mode_enable_sequence: &str = "\x1b[>4;2m\x1b[>1u\x1b[?2004h\x1b[?7l";
const TMUX_INTERACTIVE_MODE_ENABLE_SEQUENCE: &str = "\x1b[>4;2m\x1b[?2004h\x1b[?7l";
pub const theme_notification_enable_sequence: &str = "\x1b[?2031h";
pub const theme_notification_disable_sequence: &str = "\x1b[?2031l";
pub const theme_color_scheme_query: &str = "\x1b[?996n";
pub const theme_background_query: &str = "\x1b]11;?\x1b\\";
pub const theme_response_fence_query: &str = "\x1b[c";
pub const theme_background_query_with_fence: &str = "\x1b]11;?\x1b\\\x1b[c";
pub const alternate_screen_leave_sequence: &str = "\x1b[?1049l";
pub const alternate_mouse_tracking_leave_sequence: &str = "\x1b[?1000l\x1b[?1006l";

pub fn alternateScreenLeaveSequence(mouse_tracking_active: bool) -> &'static str {
    if mouse_tracking_active {
        "\x1b[?1000l\x1b[?1006l\x1b[?1049l"
    } else {
        alternate_screen_leave_sequence
    }
}

pub fn alternateScreenFrameRestoreSequence(mouse_tracking_active: bool) -> &'static str {
    if mouse_tracking_active {
        "\x1b[?1000l\x1b[?1006l\x1b[?1049l\x1b[?25l"
    } else {
        "\x1b[?1049l\x1b[?25l"
    }
}

pub fn interactiveModeEnableSequence(tmux: Option<&[u8]>) -> &'static str {
    if tmux.is_none() {
        interactive_mode_enable_sequence
    } else {
        TMUX_INTERACTIVE_MODE_ENABLE_SEQUENCE
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutError {
    UnableToReadTerminalSize,
    TerminalTooSmall,
}

pub fn layoutFromSize(rows: u16, cols: u16, footer_rows: u16) -> Result<Layout, LayoutError> {
    if rows <= footer_rows {
        return Err(LayoutError::TerminalTooSmall);
    }
    Ok(Layout {
        rows,
        cols,
        content_bottom: rows - footer_rows,
        divider_top_row: rows - 3,
        input_row: rows - 2,
        divider_bottom_row: rows - 1,
        hint_row: rows,
    })
}

#[cfg(unix)]
pub fn queryLayout(fd: std::os::fd::RawFd, footer_rows: u16) -> Result<Layout, LayoutError> {
    #[repr(C)]
    struct Winsize {
        row: u16,
        col: u16,
        xpixel: u16,
        ypixel: u16,
    }
    unsafe extern "C" {
        fn ioctl(
            fd: std::os::raw::c_int,
            request: std::os::raw::c_ulong,
            ...
        ) -> std::os::raw::c_int;
    }
    let mut ws = Winsize {
        row: 0,
        col: 0,
        xpixel: 0,
        ypixel: 0,
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const TIOCGWINSZ: std::os::raw::c_ulong = 0x5413;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    const TIOCGWINSZ: std::os::raw::c_ulong = 0x40087468;
    // SAFETY: `ws` is a writable winsize with the ABI expected by TIOCGWINSZ.
    let rc = unsafe { ioctl(fd, TIOCGWINSZ, &mut ws) };
    if rc == -1 || ws.row == 0 || ws.col == 0 {
        return Err(LayoutError::UnableToReadTerminalSize);
    }
    layoutFromSize(ws.row, ws.col, footer_rows)
}

#[cfg(not(unix))]
pub fn queryLayout(_: i32, _: u16) -> Result<Layout, LayoutError> {
    Err(LayoutError::UnableToReadTerminalSize)
}

pub fn isInvalidLayoutError(err: LayoutError) -> bool {
    matches!(
        err,
        LayoutError::UnableToReadTerminalSize | LayoutError::TerminalTooSmall
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveCursorError {
    BufferTooSmall,
}
pub fn moveCursorSequence(buf: &mut [u8], row: u16, col: u16) -> Result<&[u8], MoveCursorError> {
    let mut text = String::new();
    write!(&mut text, "\x1b[{row};{col}H").unwrap();
    if text.len() > buf.len() {
        return Err(MoveCursorError::BufferTooSmall);
    }
    buf[..text.len()].copy_from_slice(text.as_bytes());
    Ok(&buf[..text.len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn move_cursor_formats_correctly() {
        let mut b = [0; 32];
        assert_eq!(moveCursorSequence(&mut b, 10, 5).unwrap(), b"\x1b[10;5H");
    }
    #[test]
    fn move_cursor_row_one_col_one() {
        let mut b = [0; 32];
        assert_eq!(moveCursorSequence(&mut b, 1, 1).unwrap(), b"\x1b[1;1H");
    }
    #[test]
    fn layout_geometry() {
        let l = layoutFromSize(24, 80, 4).unwrap();
        assert_eq!(
            l,
            Layout {
                rows: 24,
                cols: 80,
                content_bottom: 20,
                divider_top_row: 21,
                input_row: 22,
                divider_bottom_row: 23,
                hint_row: 24
            }
        );
        assert_eq!(layoutFromSize(4, 80, 4), Err(LayoutError::TerminalTooSmall));
    }
    #[test]
    fn alternate_leave() {
        assert_eq!(
            alternateScreenLeaveSequence(false),
            alternate_screen_leave_sequence
        );
        assert_eq!(
            alternateScreenLeaveSequence(true),
            "\x1b[?1000l\x1b[?1006l\x1b[?1049l"
        );
    }
    #[test]
    fn restore_hides_cursor() {
        assert_eq!(
            alternateScreenFrameRestoreSequence(false),
            "\x1b[?1049l\x1b[?25l"
        );
        assert_eq!(
            alternateScreenFrameRestoreSequence(true),
            "\x1b[?1000l\x1b[?1006l\x1b[?1049l\x1b[?25l"
        );
    }
    #[test]
    fn direct_keyboard_protocols() {
        assert_eq!(
            interactiveModeEnableSequence(None),
            interactive_mode_enable_sequence
        );
    }
    #[test]
    fn tmux_negotiates_kitty() {
        let s = interactiveModeEnableSequence(Some(b""));
        assert_eq!(s, "\x1b[>4;2m\x1b[?2004h\x1b[?7l");
        assert!(!s.contains("\x1b[>1u"));
    }
    #[test]
    fn scrollback_stays_enabled() {
        for tmux in [None, Some(b"".as_slice())] {
            let s = interactiveModeEnableSequence(tmux);
            for mouse in ["\x1b[?1000h", "\x1b[?1002h", "\x1b[?1006h"] {
                assert!(!s.contains(mouse));
            }
        }
    }
}
