#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Toggle,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    InlineMode,
    Review,
    Full,
}

impl Depth {
    pub fn transition(self, event: Event) -> Depth {
        match event {
            Event::Toggle => {
                if self == Depth::InlineMode {
                    Depth::Review
                } else {
                    Depth::InlineMode
                }
            }
            Event::Left => {
                if self.active() {
                    Depth::Review
                } else {
                    Depth::InlineMode
                }
            }
            Event::Right => {
                if self.active() {
                    Depth::Full
                } else {
                    Depth::InlineMode
                }
            }
        }
    }

    pub fn active(self) -> bool {
        self != Depth::InlineMode
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemRow {
    pub entry_id: u32,
    pub row: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub depth: Depth,
    pub scroll_rows: u32,
    pub follow_tail: bool,
    pub anchor_entry_id: Option<u32>,
    pub anchor_pending: bool,
    pub bookmark_entry_id: Option<u32>,
    pub bookmark_intra_row: u32,
    pub bookmark_pending: bool,
    pub projection_cols: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OffsetSelection {
    pub state: State,
    pub offset: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub depth: Depth,
    pub scroll_rows: u32,
    pub follow_tail: bool,
    pub anchor_entry_id: Option<u32>,
    pub anchor_pending: bool,
    pub bookmark_entry_id: Option<u32>,
    pub bookmark_intra_row: u32,
    pub bookmark_pending: bool,
    pub projection_cols: Option<u16>,
}

impl Default for State {
    fn default() -> Self {
        State {
            depth: Depth::InlineMode,
            scroll_rows: 0,
            follow_tail: true,
            anchor_entry_id: None,
            anchor_pending: false,
            bookmark_entry_id: None,
            bookmark_intra_row: 0,
            bookmark_pending: false,
            projection_cols: None,
        }
    }
}

impl State {
    pub fn with_depth(self, requested: Depth) -> State {
        if self.depth == requested {
            return self;
        }

        let mut next = self;
        if self.depth.active() && requested.active() {
            next = next.queue_bookmark();
        }
        next = match requested {
            Depth::InlineMode => next.closed(),
            Depth::Review => {
                if self.depth == Depth::InlineMode {
                    next.open_review()
                } else {
                    next.depth = Depth::Review;
                    next
                }
            }
            Depth::Full => {
                if self.depth == Depth::InlineMode {
                    next = next.open_review();
                }
                next.depth = Depth::Full;
                next
            }
        };
        next
    }

    pub fn closed(self) -> State {
        let mut next = self.reset_viewport().clear_anchor();
        next.depth = Depth::InlineMode;
        next
    }

    pub fn clear_anchor(self) -> State {
        let mut next = self;
        next.anchor_entry_id = None;
        next.anchor_pending = false;
        next.bookmark_entry_id = None;
        next.bookmark_intra_row = 0;
        next.bookmark_pending = false;
        next.projection_cols = None;
        next
    }

    pub fn capture_anchor(self, entry_id: Option<u32>) -> State {
        let mut next = self;
        next.anchor_entry_id = entry_id;
        next.anchor_pending = entry_id.is_some();
        next
    }

    pub fn retarget_anchor(self, entry_id: u32) -> State {
        let mut next = self;
        next.anchor_entry_id = Some(entry_id);
        next
    }

    pub fn prepare_projection(self, cols: u16) -> State {
        let mut next = self;
        if let Some(previous_cols) = next.projection_cols {
            if previous_cols != cols {
                next = next.queue_bookmark();
            }
        }
        next.projection_cols = Some(cols);
        next
    }

    pub fn scroll(self, direction: ScrollDirection, rows: u32) -> State {
        let mut next = self;
        next.follow_tail = false;
        next.bookmark_pending = false;
        match direction {
            ScrollDirection::Up => next.scroll_rows = next.scroll_rows.saturating_sub(rows),
            ScrollDirection::Down => next.scroll_rows = next.scroll_rows.saturating_add(rows),
        }
        next
    }

    pub fn select_visual_offset(
        self,
        total_rows: u32,
        visible_rows: u16,
        item_rows: &[ItemRow],
    ) -> OffsetSelection {
        let mut next = self;
        let max_offset = total_rows.saturating_sub(visible_rows as u32);
        let anchor_rows = resolve_bookmark_row(item_rows, next.anchor_entry_id);
        let offset = if next.bookmark_pending {
            next.bookmark_pending = false;
            next.follow_tail = false;
            match resolve_bookmark_row(item_rows, next.bookmark_entry_id) {
                Some(item_row) => {
                    let item_end = next_bookmark_item_row(item_rows, item_row).unwrap_or(total_rows);
                    let max_intra_row = item_end.saturating_sub(item_row).saturating_sub(1);
                    item_row
                        .saturating_add(next.bookmark_intra_row.min(max_intra_row))
                        .min(max_offset)
                }
                None => next.scroll_rows.min(max_offset),
            }
        } else if next.anchor_pending && anchor_rows.is_some() {
            next.anchor_pending = false;
            next.follow_tail = false;
            anchor_rows.unwrap().min(max_offset)
        } else if next.follow_tail {
            max_offset
        } else {
            next.scroll_rows.min(max_offset)
        };

        next.scroll_rows = offset;
        next.follow_tail = offset == max_offset;
        next = next.refresh_bookmark(item_rows, offset);
        OffsetSelection {
            state: next,
            offset,
        }
    }

    pub fn snapshot(self) -> Snapshot {
        Snapshot {
            depth: self.depth,
            scroll_rows: self.scroll_rows,
            follow_tail: self.follow_tail,
            anchor_entry_id: self.anchor_entry_id,
            anchor_pending: self.anchor_pending,
            bookmark_entry_id: self.bookmark_entry_id,
            bookmark_intra_row: self.bookmark_intra_row,
            bookmark_pending: self.bookmark_pending,
            projection_cols: self.projection_cols,
        }
    }

    pub fn from_snapshot(saved: Snapshot) -> State {
        State {
            depth: saved.depth,
            scroll_rows: saved.scroll_rows,
            follow_tail: saved.follow_tail,
            anchor_entry_id: saved.anchor_entry_id,
            anchor_pending: saved.anchor_pending,
            bookmark_entry_id: saved.bookmark_entry_id,
            bookmark_intra_row: saved.bookmark_intra_row,
            bookmark_pending: saved.bookmark_pending,
            projection_cols: saved.projection_cols,
        }
    }

    fn open_review(self) -> State {
        let mut next = self.reset_viewport();
        next.depth = Depth::Review;
        // The identity remains available for retention retargeting, but a
        // normal open starts at the tail instead of consuming the old anchor.
        next.anchor_pending = false;
        next
    }

    fn reset_viewport(self) -> State {
        let mut next = self;
        next.scroll_rows = 0;
        next.follow_tail = true;
        next
    }

    fn queue_bookmark(self) -> State {
        let mut next = self;
        next.bookmark_pending = !next.follow_tail && next.bookmark_entry_id.is_some();
        next
    }

    fn refresh_bookmark(self, item_rows: &[ItemRow], offset: u32) -> State {
        let mut selected: Option<ItemRow> = None;
        for &item in item_rows {
            if item.row > offset {
                break;
            }
            selected = Some(item);
        }
        let item = match selected {
            Some(item) => item,
            None => {
                if !item_rows.is_empty() {
                    item_rows[0]
                } else {
                    return self;
                }
            }
        };
        let mut next = self;
        next.bookmark_entry_id = Some(item.entry_id);
        next.bookmark_intra_row = offset.saturating_sub(item.row);
        next
    }
}

fn resolve_bookmark_row(item_rows: &[ItemRow], entry_id: Option<u32>) -> Option<u32> {
    let target = entry_id?;
    let mut preceding: Option<ItemRow> = None;
    let mut following: Option<ItemRow> = None;
    for &item in item_rows {
        if item.entry_id == target {
            return Some(item.row);
        }
        if item.entry_id < target {
            if preceding.map_or(true, |p| item.entry_id > p.entry_id) {
                preceding = Some(item);
            }
        } else if following.map_or(true, |f| item.entry_id < f.entry_id) {
            following = Some(item);
        }
    }
    if let Some(item) = preceding {
        Some(item.row)
    } else if let Some(item) = following {
        Some(item.row)
    } else {
        None
    }
}

fn next_bookmark_item_row(item_rows: &[ItemRow], current_row: u32) -> Option<u32> {
    for &item in item_rows {
        if item.row > current_row {
            return Some(item.row);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_presentation_depth_preserves_viewer_transitions() {
        let cases = [
            (Depth::InlineMode, Event::Toggle, Depth::Review),
            (Depth::Review, Event::Toggle, Depth::InlineMode),
            (Depth::Full, Event::Toggle, Depth::InlineMode),
            (Depth::InlineMode, Event::Left, Depth::InlineMode),
            (Depth::Review, Event::Left, Depth::Review),
            (Depth::Full, Event::Left, Depth::Review),
            (Depth::InlineMode, Event::Right, Depth::InlineMode),
            (Depth::Review, Event::Right, Depth::Full),
            (Depth::Full, Event::Right, Depth::Full),
        ];

        for (from, input, expected) in cases {
            assert_eq!(expected, from.transition(input));
        }
    }

    #[test]
    fn transcript_presentation_scroll_saturates_and_leaves_follow_tail() {
        let initial = State {
            scroll_rows: 1,
            ..Default::default()
        };
        let at_start = initial.scroll(ScrollDirection::Up, 3);
        assert_eq!(0, at_start.scroll_rows);
        assert!(!at_start.follow_tail);

        let near_end = State {
            scroll_rows: u32::MAX - 1,
            ..Default::default()
        };
        let at_end = near_end.scroll(ScrollDirection::Down, 3);
        assert_eq!(u32::MAX, at_end.scroll_rows);
    }

    #[test]
    fn transcript_presentation_depth_changes_preserve_viewport_intent() {
        let item_rows = [
            ItemRow {
                entry_id: 10,
                row: 0,
            },
            ItemRow {
                entry_id: 20,
                row: 4,
            },
            ItemRow {
                entry_id: 30,
                row: 10,
            },
        ];

        for (from, to) in [
            (Depth::Review, Depth::Full),
            (Depth::Full, Depth::Review),
        ] {
            let tail = State {
                depth: from,
                scroll_rows: 4,
                follow_tail: true,
                bookmark_entry_id: Some(20),
                bookmark_intra_row: 2,
                ..Default::default()
            }
            .with_depth(to);
            assert!(!tail.bookmark_pending);

            let tail_selected = tail.select_visual_offset(20, 5, &item_rows);
            assert_eq!(15, tail_selected.offset);
            assert!(tail_selected.state.follow_tail);

            let history = State {
                depth: from,
                scroll_rows: 6,
                follow_tail: false,
                bookmark_entry_id: Some(20),
                bookmark_intra_row: 2,
                ..Default::default()
            }
            .with_depth(to);
            assert!(history.bookmark_pending);

            let history_selected = history.select_visual_offset(20, 5, &item_rows);
            assert_eq!(6, history_selected.offset);
            assert_eq!(Some(20), history_selected.state.bookmark_entry_id);
            assert_eq!(2, history_selected.state.bookmark_intra_row);
            assert!(!history_selected.state.bookmark_pending);
        }

        let no_bookmark = State {
            depth: Depth::Review,
            scroll_rows: 99,
            follow_tail: false,
            ..Default::default()
        }
        .with_depth(Depth::Full);
        assert!(!no_bookmark.bookmark_pending);
        let clamped = no_bookmark.select_visual_offset(20, 5, &item_rows);
        assert_eq!(15, clamped.offset);
    }

    #[test]
    fn transcript_presentation_clamps_bookmarks_and_selects_retained_neighbor() {
        let item_rows = [
            ItemRow {
                entry_id: 10,
                row: 1,
            },
            ItemRow {
                entry_id: 30,
                row: 8,
            },
        ];
        let clamped = State {
            scroll_rows: 4,
            follow_tail: false,
            bookmark_entry_id: Some(10),
            bookmark_intra_row: 99,
            bookmark_pending: true,
            ..Default::default()
        }
        .select_visual_offset(14, 4, &item_rows);
        assert_eq!(7, clamped.offset);

        let neighbor = State {
            scroll_rows: 4,
            follow_tail: false,
            bookmark_entry_id: Some(20),
            bookmark_pending: true,
            ..Default::default()
        }
        .select_visual_offset(14, 4, &item_rows);
        assert_eq!(1, neighbor.offset);
        assert_eq!(Some(10), neighbor.state.bookmark_entry_id);

        let following = State {
            scroll_rows: 4,
            follow_tail: false,
            bookmark_entry_id: Some(5),
            bookmark_intra_row: 1,
            bookmark_pending: true,
            ..Default::default()
        }
        .select_visual_offset(14, 4, &item_rows);
        assert_eq!(2, following.offset);
        assert_eq!(Some(10), following.state.bookmark_entry_id);
    }

    #[test]
    fn transcript_presentation_snapshot_round_trips_all_state() {
        let state = State {
            depth: Depth::Full,
            scroll_rows: 7,
            follow_tail: false,
            anchor_entry_id: Some(11),
            anchor_pending: true,
            bookmark_entry_id: Some(12),
            bookmark_intra_row: 2,
            bookmark_pending: true,
            projection_cols: Some(40),
        };
        assert_eq!(state, State::from_snapshot(state.snapshot()));
    }
}
