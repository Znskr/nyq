#![allow(non_snake_case)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameDocumentAppendClear {
    Remainder,
    Rows(u16),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameDocumentAppend<'a> {
    pub bytes: &'a [u8],
    pub start_row: u16,
    pub start_col: u16,
    pub clear: FrameDocumentAppendClear,
    pub reset_replay: bool,
}
impl Default for FrameDocumentAppend<'_> {
    fn default() -> Self {
        Self {
            bytes: &[],
            start_row: 1,
            start_col: 1,
            clear: FrameDocumentAppendClear::Remainder,
            reset_replay: false,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidFrameScrollPlan;
impl FrameDocumentAppend<'_> {
    pub fn isEmpty(self) -> bool {
        self.bytes.is_empty()
    }
    pub fn validatedStartCol(
        self,
        target_cols: u16,
        target_rows: u16,
    ) -> Result<u16, InvalidFrameScrollPlan> {
        if target_cols == 0
            || target_rows == 0
            || self.start_row == 0
            || self.start_row > target_rows
        {
            Err(InvalidFrameScrollPlan)
        } else {
            Ok(self.start_col.max(1).min(target_cols))
        }
    }
    pub fn validatedClearBottom(
        self,
        target_rows: u16,
    ) -> Result<Option<u16>, InvalidFrameScrollPlan> {
        if target_rows == 0 || self.start_row == 0 || self.start_row > target_rows {
            return Err(InvalidFrameScrollPlan);
        }
        match self.clear {
            FrameDocumentAppendClear::Remainder => Ok(None),
            FrameDocumentAppendClear::Rows(0) => Err(InvalidFrameScrollPlan),
            FrameDocumentAppendClear::Rows(n) => Ok(Some(
                self.start_row + n.min(target_rows - self.start_row + 1) - 1,
            )),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcceptedScrollRows {
    pub preserved_release_rows: u16,
    pub inline_advance_rows: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameScrollCommit {
    pub physical_terminal_scroll_rows: u16,
    pub planned_terminal_scroll_rows: u16,
    pub expected_terminal_scroll_rows: u16,
    pub accepted_terminal_scroll_rows: u16,
    pub unplanned_terminal_scroll_rows: u16,
    pub accepted_rows: AcceptedScrollRows,
}
impl FrameScrollCommit {
    pub fn complete(self) -> bool {
        self.physical_terminal_scroll_rows >= self.expected_terminal_scroll_rows
            && self.unplanned_terminal_scroll_rows == 0
    }
    pub fn matchesPlan(self, p: FrameScrollPlan) -> bool {
        self == p.commitWithExpectedRows(
            self.physical_terminal_scroll_rows,
            self.expected_terminal_scroll_rows,
        )
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalMovementRows {
    pub planned_scroll_rows: u16,
    pub materialized_scroll_rows: u16,
    pub alignment_scroll_rows: u16,
    pub expected_scroll_rows: u16,
}
impl TerminalMovementRows {
    pub fn emittedScrollRows(self) -> u16 {
        self.materialized_scroll_rows + self.alignment_scroll_rows
    }
    pub fn resolve(p: u16, m: u16, reset: bool) -> Result<Self, InvalidFrameScrollPlan> {
        if reset {
            return Ok(Self {
                planned_scroll_rows: p,
                materialized_scroll_rows: m,
                alignment_scroll_rows: 0,
                expected_scroll_rows: m,
            });
        }
        if m > p {
            return Err(InvalidFrameScrollPlan);
        }
        Ok(Self {
            planned_scroll_rows: p,
            materialized_scroll_rows: m,
            alignment_scroll_rows: p - m,
            expected_scroll_rows: p,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameScrollPlan {
    pub prior_owned_top: u16,
    pub requested_release_rows: u16,
    pub requested_inline_advance_rows: u32,
    pub preserved_release_rows: u16,
    pub terminal_scroll_rows: u16,
    pub remaining_inline_advance_rows: u32,
    pub post_scroll_owned_top: u16,
}
impl FrameScrollPlan {
    pub fn none(rows: u16, top: u16) -> Self {
        merge(rows, top, 0, 0)
    }
    pub fn validate(self, rows: u16) -> Result<(), InvalidFrameScrollPlan> {
        if self
            == merge(
                rows,
                self.prior_owned_top,
                self.requested_release_rows,
                self.requested_inline_advance_rows,
            )
        {
            Ok(())
        } else {
            Err(InvalidFrameScrollPlan)
        }
    }
    pub fn acceptedPreservedRows(self, n: u16) -> u16 {
        self.acceptedRows(n).preserved_release_rows
    }
    pub fn acceptedInlineRows(self, n: u16) -> u32 {
        self.acceptedRows(n).inline_advance_rows
    }
    pub fn acceptedRows(self, n: u16) -> AcceptedScrollRows {
        self.commit(n).accepted_rows
    }
    pub fn commit(self, n: u16) -> FrameScrollCommit {
        self.commitWithExpectedRows(n, self.terminal_scroll_rows)
    }
    pub fn commitWithExpectedRows(self, n: u16, expected: u16) -> FrameScrollCommit {
        assert!(expected >= self.terminal_scroll_rows);
        let accepted = n.min(self.terminal_scroll_rows);
        FrameScrollCommit {
            physical_terminal_scroll_rows: n,
            planned_terminal_scroll_rows: self.terminal_scroll_rows,
            expected_terminal_scroll_rows: expected,
            accepted_terminal_scroll_rows: accepted,
            unplanned_terminal_scroll_rows: n.saturating_sub(expected),
            accepted_rows: split(
                self.preserved_release_rows,
                self.requested_inline_advance_rows,
                accepted,
            ),
        }
    }
}
pub fn merge(rows: u16, top: u16, release: u16, inline: u32) -> FrameScrollPlan {
    let prior = normalizeOwnedTop(top, rows);
    let capacity = prior.saturating_sub(1);
    let requested = release.min(capacity);
    let terminal = if rows == 0 {
        0
    } else {
        (requested as u32 + inline).min(u16::MAX as u32) as u16
    };
    let accepted = split(capacity, inline, terminal);
    FrameScrollPlan {
        prior_owned_top: prior,
        requested_release_rows: requested,
        requested_inline_advance_rows: inline,
        preserved_release_rows: accepted.preserved_release_rows,
        terminal_scroll_rows: terminal,
        remaining_inline_advance_rows: inline.saturating_sub(accepted.inline_advance_rows),
        post_scroll_owned_top: prior.saturating_sub(accepted.preserved_release_rows),
    }
}
pub fn normalizeOwnedTop(top: u16, rows: u16) -> u16 {
    if rows == 0 {
        0
    } else if top == 0 {
        1
    } else {
        top.min(rows)
    }
}
fn split(cap: u16, inline: u32, terminal: u16) -> AcceptedScrollRows {
    let preserved = cap.min(terminal);
    AcceptedScrollRows {
        preserved_release_rows: preserved,
        inline_advance_rows: inline.min(terminal.saturating_sub(preserved) as u32),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merge_cases() {
        let p = merge(10, 5, 3, 1);
        assert_eq!(
            (
                p.prior_owned_top,
                p.requested_release_rows,
                p.preserved_release_rows,
                p.terminal_scroll_rows,
                p.remaining_inline_advance_rows,
                p.post_scroll_owned_top
            ),
            (5, 3, 4, 4, 1, 1)
        );
        p.validate(10).unwrap();
        assert_eq!(FrameScrollPlan::none(10, 99), merge(10, 99, 0, 0));
        let p = merge(4, 1, 0, 7);
        assert_eq!(
            (p.terminal_scroll_rows, p.remaining_inline_advance_rows),
            (7, 0)
        );
        let p = merge(0, 5, 0, 7);
        assert_eq!(
            (
                p.prior_owned_top,
                p.terminal_scroll_rows,
                p.remaining_inline_advance_rows,
                p.post_scroll_owned_top
            ),
            (0, 0, 7, 0)
        );
    }
    #[test]
    fn append_validation() {
        let a = FrameDocumentAppend {
            bytes: b"row\n",
            start_row: 4,
            start_col: 99,
            ..Default::default()
        };
        assert_eq!(a.validatedStartCol(8, 4), Ok(8));
        assert!(a.validatedStartCol(0, 4).is_err());
        let a = FrameDocumentAppend {
            start_row: 3,
            clear: FrameDocumentAppendClear::Rows(3),
            ..Default::default()
        };
        assert_eq!(a.validatedClearBottom(4), Ok(Some(4)));
    }
    #[test]
    fn chunks() {
        let p = merge(24, 1, 0, 100_000);
        assert_eq!(p.terminal_scroll_rows, u16::MAX);
        assert_eq!(p.remaining_inline_advance_rows, 100_000 - u16::MAX as u32)
    }
    #[test]
    fn commits() {
        let p = merge(10, 5, 3, 4);
        let c = p.commit(99);
        assert_eq!(
            (
                c.accepted_rows.preserved_release_rows,
                c.accepted_rows.inline_advance_rows,
                c.unplanned_terminal_scroll_rows
            ),
            (4, 3, 92)
        );
        assert!(!c.complete());
        let c = merge(10, 1, 0, 0).commitWithExpectedRows(3, 3);
        assert!(c.complete())
    }
    #[test]
    fn movement() {
        let n = TerminalMovementRows::resolve(5, 2, false).unwrap();
        assert_eq!(
            (
                n.alignment_scroll_rows,
                n.expected_scroll_rows,
                n.emittedScrollRows()
            ),
            (3, 5, 5)
        );
        assert!(TerminalMovementRows::resolve(1, 2, false).is_err());
        assert_eq!(
            TerminalMovementRows::resolve(0, 4, true)
                .unwrap()
                .expected_scroll_rows,
            4
        )
    }
    #[test]
    fn rejects_noncanonical() {
        let mut p = merge(10, 5, 2, 3);
        p.terminal_scroll_rows = 4;
        assert!(p.validate(10).is_err());
        p = merge(10, 5, 2, 3);
        p.preserved_release_rows = 3;
        assert!(p.validate(10).is_err());
    }
    #[test]
    fn none_canonical() {
        assert_eq!(merge(10, 99, 0, 0), FrameScrollPlan::none(10, 99));
        assert_eq!(merge(0, 5, 0, 0), FrameScrollPlan::none(0, 5));
    }
    #[test]
    fn equal_preserved_and_inline() {
        let p = merge(10, 5, 2, 2);
        assert_eq!(
            (
                p.preserved_release_rows,
                p.terminal_scroll_rows,
                p.remaining_inline_advance_rows,
                p.post_scroll_owned_top
            ),
            (4, 4, 2, 1)
        );
    }
    #[test]
    fn adds_preserved_and_inline() {
        let p = merge(10, 5, 1, 3);
        assert_eq!(
            (
                p.requested_release_rows,
                p.preserved_release_rows,
                p.terminal_scroll_rows,
                p.remaining_inline_advance_rows
            ),
            (1, 4, 4, 3)
        );
    }
    #[test]
    fn advances_after_row_one() {
        let p = merge(10, 1, 0, 3);
        assert_eq!(
            (
                p.preserved_release_rows,
                p.terminal_scroll_rows,
                p.post_scroll_owned_top
            ),
            (0, 3, 1)
        );
    }
    #[test]
    fn document_movement_not_height_clamped() {
        assert_eq!(merge(4, 1, 0, 7).terminal_scroll_rows, 7);
    }
    #[test]
    fn preserves_zero_height_debt() {
        let p = merge(0, 5, 0, 7);
        assert_eq!(
            (
                p.prior_owned_top,
                p.terminal_scroll_rows,
                p.remaining_inline_advance_rows,
                p.post_scroll_owned_top
            ),
            (0, 0, 7, 0)
        );
    }
    #[test]
    fn ownership_boundaries_saturate() {
        let z = merge(10, 0, 99, 0);
        assert_eq!(
            (
                z.prior_owned_top,
                z.requested_release_rows,
                z.post_scroll_owned_top
            ),
            (1, 0, 1)
        );
        let h = merge(10, 99, 99, 0);
        assert_eq!(
            (
                h.prior_owned_top,
                h.requested_release_rows,
                h.post_scroll_owned_top
            ),
            (10, 9, 1)
        );
    }
    #[test]
    fn accepted_release_precedes_inline() {
        let p = merge(10, 5, 3, 4);
        assert_eq!(
            p.acceptedRows(2),
            AcceptedScrollRows {
                preserved_release_rows: 2,
                inline_advance_rows: 0
            }
        );
        assert_eq!(
            p.acceptedRows(5),
            AcceptedScrollRows {
                preserved_release_rows: 4,
                inline_advance_rows: 1
            }
        );
        assert_eq!(
            p.acceptedRows(99),
            AcceptedScrollRows {
                preserved_release_rows: 4,
                inline_advance_rows: 3
            }
        );
    }
    #[test]
    fn accepted_caps_at_plan() {
        let c = merge(10, 5, 0, 0).commit(4);
        assert_eq!(
            (
                c.accepted_terminal_scroll_rows,
                c.unplanned_terminal_scroll_rows
            ),
            (0, 4)
        );
        assert!(!c.complete());
    }
    #[test]
    fn unplanned_does_not_complete() {
        let p = merge(10, 5, 3, 4);
        assert!(!p.commit(p.terminal_scroll_rows + 1).complete());
    }
    #[test]
    fn expected_movement_completes() {
        assert!(merge(10, 1, 0, 0).commitWithExpectedRows(3, 3).complete());
    }
    #[test]
    fn inline_consumes_preserved_prefix() {
        let p = merge(10, 6, 0, 3);
        assert_eq!(
            (
                p.terminal_scroll_rows,
                p.preserved_release_rows,
                p.remaining_inline_advance_rows,
                p.post_scroll_owned_top
            ),
            (3, 3, 3, 3)
        );
        assert_eq!(p.acceptedRows(2).inline_advance_rows, 0);
    }
    #[test]
    fn rejects_release_above_prefix() {
        let p = FrameScrollPlan {
            prior_owned_top: 2,
            requested_release_rows: 2,
            requested_inline_advance_rows: 0,
            preserved_release_rows: 1,
            terminal_scroll_rows: 1,
            remaining_inline_advance_rows: 0,
            post_scroll_owned_top: 1,
        };
        assert!(p.validate(10).is_err());
    }
    #[test]
    fn rejects_prior_above_height() {
        let p = FrameScrollPlan {
            prior_owned_top: 11,
            requested_release_rows: 1,
            requested_inline_advance_rows: 0,
            preserved_release_rows: 1,
            terminal_scroll_rows: 1,
            remaining_inline_advance_rows: 0,
            post_scroll_owned_top: 10,
        };
        assert!(p.validate(10).is_err());
    }
    #[test]
    fn rejects_remaining_debt() {
        let mut p = merge(10, 5, 2, 3);
        p.remaining_inline_advance_rows = 1;
        assert!(p.validate(10).is_err());
    }
    #[test]
    fn rejects_post_scroll_ownership() {
        let mut p = merge(10, 5, 2, 3);
        p.post_scroll_owned_top = 2;
        assert!(p.validate(10).is_err());
    }
    #[test]
    fn canonical_plan_validates() {
        merge(10, 5, 2, 3).validate(10).unwrap();
    }
}
