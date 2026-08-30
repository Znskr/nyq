#![allow(non_snake_case)]

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Transient {
    pub row: u16,
    pub row_count: u16,
    pub gap_above_rows: u16,
    pub footer_clearance_rows: u16,
    pub tool_row: Option<u16>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityPlacement {
    None,
    TransientRow(Transient),
    OverlayEntry(u16),
}
impl ActivityPlacement {
    pub fn reservedFooterRows(self) -> u16 {
        match self {
            Self::TransientRow(x) => x.footer_clearance_rows,
            _ => 0,
        }
    }
    pub fn reservesFooterRow(self) -> bool {
        self.reservedFooterRows() > 0
    }
    pub fn reservedTopRow(self) -> Option<u16> {
        match self {
            Self::TransientRow(x) => {
                let first = x.tool_row.unwrap_or(x.row);
                Some(if first <= x.gap_above_rows {
                    1
                } else {
                    first - x.gap_above_rows
                })
            }
            _ => None,
        }
    }
    pub fn transcriptReservationRows(self) -> u16 {
        match self {
            Self::TransientRow(x) => x
                .gap_above_rows
                .saturating_add(x.row.saturating_sub(x.tool_row.unwrap_or(x.row)))
                .saturating_add(x.row_count)
                .saturating_add(x.footer_clearance_rows),
            _ => 0,
        }
    }
    pub fn requiredFooterTop(self) -> Option<u16> {
        match self {
            Self::TransientRow(x) => Some(
                x.row
                    .saturating_add(x.row_count)
                    .saturating_add(x.footer_clearance_rows),
            ),
            _ => None,
        }
    }
    pub fn row(self) -> Option<u16> {
        match self {
            Self::TransientRow(x) => Some(x.row),
            Self::OverlayEntry(r) => Some(r),
            Self::None => None,
        }
    }
    pub fn primaryRowCount(self) -> u16 {
        match self {
            Self::TransientRow(x) => x.row_count,
            Self::OverlayEntry(_) => 1,
            Self::None => 0,
        }
    }
    pub fn toolRow(self) -> Option<u16> {
        match self {
            Self::TransientRow(x) => x.tool_row,
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Input {
    pub active_label_present: bool,
    pub activity_rows: u16,
    pub tool_before_activity: bool,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub transient_gap_rows: u16,
    pub place_mid_line_active: bool,
    pub entry_bound_active: bool,
    pub stream_active: bool,
    pub footer_top_for_extra: u16,
    pub footer_clearance_rows: u16,
}
impl Input {
    pub fn new(cursor_row: u16, cursor_col: u16, footer_top_for_extra: u16) -> Self {
        Self {
            active_label_present: false,
            activity_rows: 1,
            tool_before_activity: false,
            cursor_row,
            cursor_col,
            transient_gap_rows: 0,
            place_mid_line_active: false,
            entry_bound_active: false,
            stream_active: false,
            footer_top_for_extra,
            footer_clearance_rows: 0,
        }
    }
}
pub fn resolve(input: Input) -> ActivityPlacement {
    let rows = input.activity_rows.max(1);
    if input.tool_before_activity {
        let tool = if input.cursor_col == 1 {
            input.cursor_row.saturating_add(input.transient_gap_rows)
        } else if input.place_mid_line_active {
            input
                .cursor_row
                .saturating_add(input.transient_gap_rows)
                .saturating_add(1)
        } else {
            return ActivityPlacement::None;
        };
        return ActivityPlacement::TransientRow(Transient {
            row: tool.saturating_add(2),
            row_count: rows,
            gap_above_rows: input.transient_gap_rows,
            footer_clearance_rows: input.footer_clearance_rows,
            tool_row: Some(tool),
        });
    }
    if input.active_label_present {
        let gap = input.transient_gap_rows;
        if input.cursor_col == 1 {
            return ActivityPlacement::TransientRow(Transient {
                row: input.cursor_row.saturating_add(gap),
                row_count: rows,
                gap_above_rows: gap,
                footer_clearance_rows: input.footer_clearance_rows,
                tool_row: None,
            });
        }
        if input.place_mid_line_active {
            return ActivityPlacement::TransientRow(Transient {
                row: input.cursor_row.saturating_add(gap).saturating_add(1),
                row_count: rows,
                gap_above_rows: gap,
                footer_clearance_rows: input.footer_clearance_rows,
                tool_row: None,
            });
        }
    }
    if !input.active_label_present && input.entry_bound_active && input.stream_active {
        let row = if input.cursor_col == 1 {
            input.cursor_row
        } else if input.footer_top_for_extra > 1 {
            input.footer_top_for_extra - 1
        } else {
            input.footer_top_for_extra
        };
        return ActivityPlacement::TransientRow(Transient {
            row,
            row_count: 1,
            gap_above_rows: 0,
            footer_clearance_rows: input.footer_clearance_rows,
            tool_row: None,
        });
    }
    ActivityPlacement::None
}
pub fn suppressEntryOverlayForBanner(activity: &mut ActivityPlacement, banner_active: bool) {
    if banner_active && matches!(activity, ActivityPlacement::OverlayEntry(_)) {
        *activity = ActivityPlacement::None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> Input {
        Input::new(5, 1, 9)
    }
    #[test]
    fn thinking_reserves_only_its_row() {
        let mut i = base();
        i.active_label_present = true;
        let a = resolve(i);
        assert_eq!(a.row(), Some(5));
        assert_eq!(a.transcriptReservationRows(), 1);
        assert!(!a.reservesFooterRow());
        i.cursor_col = 3;
        assert_eq!(resolve(i), ActivityPlacement::None)
    }
    #[test]
    fn leading_gap_is_separate() {
        let mut i = base();
        i.active_label_present = true;
        i.cursor_row = 3;
        i.transient_gap_rows = 1;
        i.footer_top_for_extra = 6;
        let a = resolve(i);
        assert_eq!(
            (a.row(), a.reservedTopRow(), a.transcriptReservationRows()),
            (Some(4), Some(3), 2)
        );
    }
    #[test]
    fn gap_is_not_clearance() {
        let mut i = base();
        i.active_label_present = true;
        i.cursor_row = 10;
        i.transient_gap_rows = 1;
        let a = resolve(i);
        assert_eq!(
            (a.row(), a.reservedFooterRows(), a.reservedTopRow()),
            (Some(11), 0, Some(10))
        );
    }
    #[test]
    fn midline() {
        let mut i = base();
        i.active_label_present = true;
        i.cursor_col = 12;
        i.transient_gap_rows = 1;
        i.place_mid_line_active = true;
        let a = resolve(i);
        assert_eq!(
            (a.row(), a.reservedTopRow(), a.transcriptReservationRows()),
            (Some(7), Some(6), 2)
        );
    }
    #[test]
    fn active_entry_is_transient() {
        let mut i = base();
        i.cursor_row = 9;
        i.entry_bound_active = true;
        i.stream_active = true;
        assert_eq!(resolve(i).row(), Some(9));
    }
    #[test]
    fn banner_suppression() {
        let mut a = ActivityPlacement::OverlayEntry(7);
        suppressEntryOverlayForBanner(&mut a, true);
        assert_eq!(a, ActivityPlacement::None)
    }
}
