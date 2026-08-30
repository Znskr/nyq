pub const default_max_picker_rows: u16 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub start: usize,
    pub end: usize,
}

pub fn centered(count: usize, selected: usize, max_rows: u16) -> Window {
    if count == 0 || max_rows == 0 {
        return Window { start: 0, end: 0 };
    }
    let rows = max_rows as usize;
    if count <= rows {
        return Window { start: 0, end: count };
    }

    let mut start = if selected >= rows / 2 {
        selected - rows / 2
    } else {
        0
    };
    if start + rows > count {
        start = count - rows;
    }
    Window { start, end: start + rows }
}

pub fn edge_from_selection(count: usize, selected: usize, max_rows: u16) -> Window {
    if count == 0 || max_rows == 0 {
        return Window { start: 0, end: 0 };
    }
    let rows = max_rows as usize;
    if count <= rows {
        return Window { start: 0, end: count };
    }

    let start = if selected >= rows { selected - rows + 1 } else { 0 };
    Window { start, end: (start + rows).min(count) }
}

pub fn edge_from_start(count: usize, start: usize, max_rows: u16) -> Window {
    if count == 0 || max_rows == 0 {
        return Window { start: 0, end: 0 };
    }
    let rows = max_rows as usize;
    if count <= rows {
        return Window { start: 0, end: count };
    }

    let clamped_start = start.min(count - rows);
    Window { start: clamped_start, end: clamped_start + rows }
}

pub fn update_edge_start(current_start: usize, count: usize, selected: usize, max_rows: u16) -> usize {
    if count == 0 || max_rows == 0 {
        return 0;
    }
    let rows = max_rows as usize;
    if count <= rows {
        return 0;
    }

    let clamped_selected = selected.min(count - 1);
    let mut start = current_start.min(count - rows);
    if clamped_selected < start {
        start = clamped_selected;
    } else if clamped_selected >= start + rows {
        start = clamped_selected - rows + 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_window_start_lets_selection_move_back_up_before_reverse_scrolling() {
        let rows: u16 = 6;

        let mut start = update_edge_start(0, 11, 5, rows);
        assert_eq!(0usize, start);
        assert_eq!(5usize, 5 - start);

        start = update_edge_start(start, 11, 6, rows);
        assert_eq!(1usize, start);
        assert_eq!(5usize, 6 - start);

        start = update_edge_start(start, 11, 5, rows);
        assert_eq!(1usize, start);
        assert_eq!(4usize, 5 - start);
    }
}
