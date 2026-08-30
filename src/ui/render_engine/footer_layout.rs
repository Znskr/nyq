#![allow(non_snake_case)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FooterRows {
    pub top: u16,
    pub top_divider: u16,
    pub banner: u16,
    pub banner_active: bool,
    pub banner_rows: u16,
    pub input_base: u16,
    pub picker_divider: u16,
    pub picker_start: u16,
    pub bottom_divider: u16,
    pub hint: u16,
    pub total_rows: u16,
    pub composer_top_chrome_rows: u16,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Input {
    pub footer_top_for_extra: u16,
    pub terminal_rows: u16,
    pub activity_offset: u16,
    pub extra_input_rows: u16,
    pub input_extra: u16,
    pub input_visible: bool,
    pub composer_top_chrome_rows: u16,
    pub picker_rows: u16,
    pub banner_active: bool,
    pub banner_rows: u16,
    pub allocated_rows: Option<u16>,
}
impl Input {
    pub fn new(footer_top_for_extra: u16, terminal_rows: u16) -> Self {
        Self {
            footer_top_for_extra,
            terminal_rows,
            activity_offset: 0,
            extra_input_rows: 0,
            input_extra: 0,
            input_visible: true,
            composer_top_chrome_rows: 1,
            picker_rows: 0,
            banner_active: false,
            banner_rows: 0,
            allocated_rows: None,
        }
    }
}
pub fn reservedBaseRows(input_visible: bool, composer_top_chrome_rows: u16) -> u16 {
    if input_visible {
        2u16.saturating_add(composer_top_chrome_rows)
    } else {
        3
    }
}
pub fn resolve(i: Input) -> FooterRows {
    if let Some(n) = i.allocated_rows {
        return resolveAllocated(i, n);
    }
    let chrome = if i.input_visible {
        i.composer_top_chrome_rows
    } else {
        1
    };
    let reserved = reservedBaseRows(i.input_visible, chrome) as u32 + i.extra_input_rows as u32;
    let max_top = if i.terminal_rows as u32 > reserved {
        (i.terminal_rows as u32 - reserved) as u16
    } else {
        1
    };
    let top = i
        .footer_top_for_extra
        .saturating_add(i.activity_offset)
        .min(max_top);
    let banner_rows = if i.banner_rows > 0 {
        i.banner_rows
    } else if i.banner_active {
        1
    } else {
        0
    };
    let td = top.saturating_add(banner_rows);
    if !i.input_visible {
        let ps = td.saturating_add(1);
        let bd = ps.saturating_add(i.picker_rows);
        let hint = bd.saturating_add(1);
        return FooterRows {
            top,
            top_divider: td,
            banner: top,
            banner_active: banner_rows > 0,
            banner_rows,
            input_base: td,
            picker_divider: td,
            picker_start: ps,
            bottom_divider: bd,
            hint,
            total_rows: hint - top + 1,
            composer_top_chrome_rows: 1,
        };
    }
    let ib = td.saturating_add(chrome).saturating_add(i.input_extra);
    let pd = ib.saturating_add(1);
    let bd = if i.picker_rows > 0 {
        pd.saturating_add(1).saturating_add(i.picker_rows)
    } else {
        pd
    };
    let hint = bd.saturating_add(1);
    FooterRows {
        top,
        top_divider: td,
        banner: top,
        banner_active: banner_rows > 0,
        banner_rows,
        input_base: ib,
        picker_divider: pd,
        picker_start: pd.saturating_add(1),
        bottom_divider: bd,
        hint,
        total_rows: hint - top + 1,
        composer_top_chrome_rows: chrome,
    }
}
fn resolveAllocated(i: Input, requested: u16) -> FooterRows {
    let top = i.footer_top_for_extra.min(i.terminal_rows).max(1);
    let available = i.terminal_rows - top + 1;
    let allocated = requested.min(available).max(1);
    let bottom = top + allocated - 1;
    let requested_banner = if i.banner_rows > 0 {
        i.banner_rows
    } else if i.banner_active {
        1
    } else {
        0
    };
    let banner_rows = requested_banner.min(allocated.saturating_sub(1));
    let td = top + banner_rows;
    let approval = allocated - banner_rows;
    let ps = if approval > 1 { td + 1 } else { td };
    let bd = if approval > 1 { bottom - 1 } else { bottom };
    FooterRows {
        top,
        top_divider: td,
        banner: top,
        banner_active: banner_rows > 0,
        banner_rows,
        input_base: td,
        picker_divider: td,
        picker_start: ps,
        bottom_divider: bd,
        hint: bottom,
        total_rows: allocated,
        composer_top_chrome_rows: 1,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn r(top: u16) -> Input {
        Input::new(top, 24)
    }
    #[test]
    fn anchor() {
        let x = resolve(r(8));
        assert_eq!(
            (
                x.top,
                x.top_divider,
                x.input_base,
                x.picker_divider,
                x.bottom_divider,
                x.hint
            ),
            (8, 8, 9, 10, 10, 11)
        );
    }
    #[test]
    fn clamp() {
        let mut i = r(19);
        i.extra_input_rows = 2;
        i.input_extra = 2;
        let x = resolve(i);
        assert_eq!((x.top, x.input_base, x.hint), (19, 22, 24));
    }
    #[test]
    fn picker() {
        let mut i = r(17);
        i.extra_input_rows = 4;
        i.picker_rows = 3;
        let x = resolve(i);
        assert_eq!(
            (
                x.top,
                x.input_base,
                x.picker_start,
                x.bottom_divider,
                x.hint
            ),
            (17, 18, 20, 23, 24)
        );
    }
    #[test]
    fn tint() {
        let mut i = r(20);
        i.composer_top_chrome_rows = 0;
        let x = resolve(i);
        assert_eq!(
            (
                x.top,
                x.input_base,
                x.bottom_divider,
                x.hint,
                x.total_rows,
                x.composer_top_chrome_rows
            ),
            (20, 20, 21, 22, 3, 0)
        );
    }
    #[test]
    fn tint_multiline() {
        let mut i = r(18);
        i.extra_input_rows = 5;
        i.input_extra = 1;
        i.composer_top_chrome_rows = 0;
        i.picker_rows = 3;
        let x = resolve(i);
        assert_eq!(
            (
                x.top,
                x.input_base,
                x.picker_start,
                x.bottom_divider,
                x.hint
            ),
            (17, 18, 20, 23, 24)
        );
    }
    #[test]
    fn tint_banner() {
        let mut i = r(20);
        i.extra_input_rows = 1;
        i.composer_top_chrome_rows = 0;
        i.banner_active = true;
        i.banner_rows = 1;
        let x = resolve(i);
        assert_eq!(
            (
                x.top,
                x.banner,
                x.top_divider,
                x.input_base,
                x.bottom_divider,
                x.hint
            ),
            (20, 20, 21, 21, 22, 23)
        );
    }
    #[test]
    fn picker_replaces_input() {
        let mut i = r(17);
        i.extra_input_rows = 7;
        i.input_visible = false;
        i.composer_top_chrome_rows = 0;
        i.picker_rows = 7;
        let x = resolve(i);
        assert_eq!(
            (
                x.top,
                x.input_base,
                x.picker_start,
                x.bottom_divider,
                x.hint
            ),
            (14, 14, 15, 22, 23)
        );
    }
    #[test]
    fn banner() {
        let mut i = r(8);
        i.extra_input_rows = 1;
        i.banner_active = true;
        let x = resolve(i);
        assert_eq!(
            (
                x.top,
                x.banner,
                x.top_divider,
                x.input_base,
                x.bottom_divider,
                x.hint
            ),
            (8, 8, 9, 10, 11, 12)
        );
        assert!(x.banner_active)
    }
    #[test]
    fn multi_banner() {
        let mut i = r(8);
        i.extra_input_rows = 6;
        i.banner_active = true;
        i.banner_rows = 6;
        let x = resolve(i);
        assert_eq!(
            (
                x.top,
                x.banner_rows,
                x.top_divider,
                x.input_base,
                x.bottom_divider,
                x.hint
            ),
            (8, 6, 14, 15, 16, 17)
        );
    }
}
