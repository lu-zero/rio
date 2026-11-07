// grid/resize.rs was originally taken from Alacritty
// https://github.com/alacritty/alacritty/blob/e35e5ad14fce8456afdd89f2b392b9924bb27471/alacritty_terminal/src/grid/resize.rs
// which is licensed under Apache 2.0 license.

use crate::ansi::kitty_virtual::PLACEHOLDER;
use crate::crosswords::grid::row::SemanticPrompt;
use crate::crosswords::grid::{Dimensions, Grid, GridSquare, ReflowRemap};
use crate::crosswords::pos::{Column, Line};
use crate::crosswords::square::{Square, Wide};
use crate::crosswords::Row;
use std::cmp::{min, Ordering};
use std::mem;

impl Grid<Square> {
    /// Resize the grid's width and/or height.
    ///
    /// `reflow` joins soft-wrapped rows, drops trailing spaces, and splits
    /// at the new width. `preserve_scrollback` makes a taller screen pad
    /// blank rows instead of pulling history into the viewport.
    pub fn resize(
        &mut self,
        reflow: bool,
        lines: usize,
        columns: usize,
        preserve_scrollback: bool,
    ) {
        let template = mem::take(&mut self.cursor.template);
        self.reflow_remap = None;

        let columns = columns.max(1);
        let lines = lines.max(1);

        if reflow && self.columns != columns {
            self.prune_blank_tail();
            self.rewrap_columns(columns, lines, preserve_scrollback);
        } else {
            match self.lines.cmp(&lines) {
                Ordering::Less => self.grow_lines(lines, preserve_scrollback),
                Ordering::Greater => self.shrink_lines(lines),
                Ordering::Equal => (),
            }
            if self.columns != columns {
                self.note_unmoved_remap();
                if self.columns < columns {
                    self.pad_columns(columns);
                } else {
                    self.clip_columns(columns);
                }
            }
        }

        let last_col = Column(self.columns - 1);
        self.saved_cursor.pos.col = min(self.saved_cursor.pos.col, last_col);
        if self.saved_cursor.pos.row.0 >= self.lines as i32 {
            self.saved_cursor.pos.row = Line(self.lines as i32 - 1);
        }
        self.cursor.template = template;
    }

    /// Add lines to the visible area.
    ///
    /// Rio keeps the cursor at the bottom of the terminal as long as there
    /// is scrollback available. Once scrollback is exhausted, new lines are
    /// simply added to the bottom of the screen.
    ///
    /// `preserve_scrollback` skips the pull: blanks land under the cursor
    /// and history stays where it is.
    fn grow_lines(&mut self, target: usize, preserve_scrollback: bool) {
        if preserve_scrollback {
            self.insert_blank_rows_at_bottom(target - self.lines);
            return;
        }

        let lines_added = target - self.lines;

        // Need to resize before updating buffer.
        self.raw.grow_visible_lines(target);
        self.lines = target;

        let history_size = self.history_size();
        let from_history = min(history_size, lines_added);

        // Move existing lines up for every line that couldn't be pulled from history.
        if from_history != lines_added {
            let delta = lines_added - from_history;
            self.scroll_up(&(Line(0)..Line(target as i32)), delta);
        }

        // Move cursor down for every line pulled from history.
        self.saved_cursor.pos.row += from_history;
        self.cursor.pos.row += from_history;

        // The viewport absorbed exactly the rows pulled out of history,
        // not the full growth (history may have had fewer). Clamp like
        // the reflow paths do: an offset past the remaining history makes
        // `visible_line_bounds` request rows the storage doesn't have.
        self.display_offset = self.display_offset.saturating_sub(from_history);
        self.decrease_scroll_limit(lines_added);
        self.display_offset = min(self.display_offset, self.history_size());
    }

    /// Remove lines from the visible area.
    ///
    /// The behavior in Terminal.app and iTerm.app is to keep the cursor at the
    /// bottom of the screen. This is achieved by pushing history "out the top"
    /// of the terminal window.
    ///
    /// Rio takes the same approach.
    fn shrink_lines(&mut self, target: usize) {
        // Scroll up to keep content inside the window.
        let required_scrolling =
            (self.cursor.pos.row.0 as usize + 1).saturating_sub(target);
        if required_scrolling > 0 {
            self.scroll_up(&(Line(0)..Line(self.lines as i32)), required_scrolling);

            // Clamp cursors to the new viewport size.
            self.cursor.pos.row = min(self.cursor.pos.row, Line(target as i32 - 1));
        }

        // Clamp saved cursor, since only primary cursor is scrolled into viewport.
        self.saved_cursor.pos.row =
            min(self.saved_cursor.pos.row, Line(target as i32 - 1));

        self.raw.rotate((self.lines - target) as isize);
        self.raw.shrink_visible_lines(target);
        self.lines = target;
        // Rows moved into history above; same defensive clamp as
        // `grow_lines` and the column reflows.
        self.display_offset = min(self.display_offset, self.history_size());
    }

    /// Drop the whitespace rows under the cursor so a later shrink cannot
    /// push them into scrollback. Interior blanks stay. The cursor row stays.
    fn prune_blank_tail(&mut self) {
        let cursor_line = (self.cursor.pos.row.0.max(0) as usize).min(self.lines - 1);
        let below = self.lines - 1 - cursor_line;
        if below == 0 {
            return;
        }

        let mut rows = self.raw.take_all();
        let mut drop_count = 0;
        while drop_count < below && row_is_whitespace(&rows[drop_count]) {
            drop_count += 1;
        }
        if drop_count == 0 {
            self.raw.replace_inner(rows);
            return;
        }

        rows.drain(0..drop_count);
        self.lines -= drop_count;
        self.raw.replace_inner(rows);
        self.raw.set_visible_lines(self.lines);
        if self.saved_cursor.pos.row.0 >= self.lines as i32 {
            self.saved_cursor.pos.row = Line(self.lines as i32 - 1);
        }
    }

    /// Join soft-wrapped rows and split them at `columns`, then fit the
    /// viewport to `target` lines.
    fn rewrap_columns(
        &mut self,
        columns: usize,
        target: usize,
        preserve_scrollback: bool,
    ) {
        let old_columns = self.columns;
        let shrinking = columns < old_columns;
        let cursor_col = self.cursor.pos.col.0;
        let cursor_line = (self.cursor.pos.row.0.max(0) as usize).min(self.lines - 1);

        let mut rows = self.raw.take_all();
        let old_len = rows.len();
        let base_abs = self.lines_evicted();
        rows.reverse();

        let hist_count = old_len.saturating_sub(self.lines);
        let cursor_oldest = hist_count + cursor_line;
        let mut remap = self.track_reflow_remap.then(|| vec![-1i64; old_len]);

        let mut out: Vec<Row<Square>> = Vec::with_capacity(old_len);
        let mut view_rows = 0usize;
        let mut logical = LogicalLine::default();
        let mut cursor_at: Option<CursorLanding> = None;

        for (old_idx, row) in rows.into_iter().enumerate() {
            let wrapped = row.last().is_some_and(|cell| cell.wrapline());
            let cells = cells_for_join(&row, wrapped);
            if logical.rows.is_empty() {
                logical.prompt = row.semantic_prompt;
            }
            let start = logical.cells.len();
            if old_idx == cursor_oldest {
                logical.cursor = Some(start + cursor_col);
            }
            logical.rows.push((old_idx, start));
            logical.touches_view |= old_idx >= hist_count;
            logical.cells.extend(cells);
            if wrapped {
                continue;
            }
            flush_logical(
                &mut logical,
                columns,
                shrinking,
                cursor_col,
                &mut out,
                &mut view_rows,
                &mut remap,
                &mut cursor_at,
            );
        }
        if !logical.rows.is_empty() {
            flush_logical(
                &mut logical,
                columns,
                shrinking,
                cursor_col,
                &mut out,
                &mut view_rows,
                &mut remap,
                &mut cursor_at,
            );
        }

        let cap = self.max_scroll_limit + target;
        trim_to_cap(
            &mut out,
            &mut view_rows,
            cap,
            &mut remap,
            &mut cursor_at,
            &mut self.total_lines_scrolled,
        );
        fit_viewport(&mut out, view_rows, target, columns, preserve_scrollback);

        let cursor_index = cursor_at
            .map(|landing| landing.row.min(out.len().saturating_sub(1)))
            .unwrap_or(0);
        let viewport_start = out.len().saturating_sub(target);
        let view_row = cursor_index.saturating_sub(viewport_start).min(target - 1);
        let (cursor_col, cursor_wrap) = match cursor_at {
            Some(landing) => (landing.col, landing.should_wrap),
            None => (0, false),
        };

        out.reverse();
        self.columns = columns;
        self.lines = target;
        self.raw.replace_inner(out);
        self.raw.set_visible_lines(self.lines);
        self.cursor.pos.row = Line(view_row as i32);
        self.cursor.pos.col = Column(cursor_col);
        self.cursor.should_wrap = cursor_wrap;
        self.display_offset = min(self.display_offset, self.history_size());
        self.reflow_remap = remap.map(|new_pos| ReflowRemap { base_abs, new_pos });
    }

    /// Append `extra` blank rows under the viewport. History line numbers
    /// stay put: the blanks are inserted at the bottom of the newest-first
    /// ring, not grown in through `grow_visible_lines`.
    fn insert_blank_rows_at_bottom(&mut self, extra: usize) {
        if extra == 0 {
            return;
        }
        let mut rows = self.raw.take_all();
        let mut with_blanks = Vec::with_capacity(rows.len() + extra);
        with_blanks.resize_with(extra, || Row::new(self.columns));
        with_blanks.append(&mut rows);
        self.lines += extra;
        self.raw.replace_inner(with_blanks);
        self.raw.set_visible_lines(self.lines);
        self.display_offset = min(self.display_offset, self.history_size());
    }

    fn pad_columns(&mut self, columns: usize) {
        self.columns = columns;
        self.raw.grow_all_rows(columns);
    }

    fn clip_columns(&mut self, columns: usize) {
        self.columns = columns;
        self.raw.shrink_all_rows(columns);
        let last = Column(columns - 1);
        self.cursor.pos.col = min(self.cursor.pos.col, last);
    }

    /// Rows did not move. Image anchors keep their oldest-first index.
    fn note_unmoved_remap(&mut self) {
        if !self.track_reflow_remap {
            return;
        }
        let len = self.raw.len();
        self.reflow_remap = Some(ReflowRemap {
            base_abs: self.lines_evicted(),
            new_pos: (0..len as i64).collect(),
        });
    }
}

#[derive(Clone, Copy)]
struct CursorLanding {
    row: usize,
    col: usize,
    should_wrap: bool,
}

#[derive(Default)]
struct LogicalLine {
    cells: Vec<Square>,
    /// `(old oldest-first index, first-cell offset in `cells`)`.
    rows: Vec<(usize, usize)>,
    prompt: SemanticPrompt,
    touches_view: bool,
    /// Cursor index into `cells` before the trailing-space trim.
    cursor: Option<usize>,
}

fn flush_logical(
    line: &mut LogicalLine,
    columns: usize,
    shrinking: bool,
    original_cursor_col: usize,
    out: &mut Vec<Row<Square>>,
    view_rows: &mut usize,
    remap: &mut Option<Vec<i64>>,
    cursor_at: &mut Option<CursorLanding>,
) {
    let untrimmed = mem::take(&mut line.cells);
    let pending = mem::take(&mut line.rows);
    let prompt = line.prompt;
    let touches_view = line.touches_view;
    let cursor_index = line.cursor.take();
    line.prompt = SemanticPrompt::None;
    line.touches_view = false;

    let mut trimmed = untrimmed;
    if trimmed.len() > columns {
        if let Some(end) = trimmed.iter().rposition(|cell| !is_pad(*cell)) {
            trimmed.truncate(end + 1);
        } else {
            trimmed.clear();
        }
    }

    let first_out = out.len();
    let (frags, places) = if trimmed.is_empty() {
        (vec![Vec::new()], Vec::new())
    } else {
        split_cells(&trimmed, columns)
    };
    let produced = frags.len().max(1);
    push_fragments(out, frags, columns, prompt);
    if touches_view {
        *view_rows += produced;
    }

    if let Some(map) = remap.as_mut() {
        for (old_idx, offset) in pending {
            map[old_idx] = if trimmed.is_empty() {
                first_out as i64
            } else if offset >= trimmed.len() {
                -1
            } else {
                (first_out + places[offset].0) as i64
            };
        }
    }

    if let Some(index) = cursor_index {
        *cursor_at = Some(land_cursor(
            index,
            columns,
            shrinking,
            original_cursor_col,
            first_out,
            produced,
        ));
    }
}

/// `index / columns`, `index % columns`, then the column-0 adjustment.
/// A landing past the rows actually emitted clamps onto the last one.
fn land_cursor(
    index: usize,
    columns: usize,
    shrinking: bool,
    original_cursor_col: usize,
    first_out: usize,
    produced: usize,
) -> CursorLanding {
    let mut row = first_out + index / columns;
    let mut col = index % columns;
    if col == 0 && row > 0 {
        col = if shrinking {
            original_cursor_col
        } else {
            columns
        };
        row -= 1;
    }
    let last = first_out + produced - 1;
    if row > last {
        row = last;
    }
    let (col, should_wrap) = if col >= columns {
        (columns - 1, true)
    } else {
        (col, false)
    };
    CursorLanding {
        row,
        col,
        should_wrap,
    }
}

fn push_fragments(
    out: &mut Vec<Row<Square>>,
    frags: Vec<Vec<Square>>,
    columns: usize,
    prompt: SemanticPrompt,
) {
    let produced = frags.len();
    for (i, mut cells) in frags.into_iter().enumerate() {
        let last = i + 1 == produced;
        if !last {
            debug_assert_eq!(cells.len(), columns);
            if let Some(cell) = cells.last_mut() {
                cell.set_wrapline(true);
            }
        }
        let occ = cells.len().min(columns);
        if cells.len() < columns {
            cells.resize(columns, Square::default());
        }
        let mut row = Row::from_vec(cells, occ);
        row.has_extras = row.inner.iter().any(|cell| cell.has_extras());
        row.has_styles = row.inner.iter().any(GridSquare::carries_style);
        row.kitty_virtual_placeholder = row
            .inner
            .iter()
            .any(|cell| !cell.is_bg_only() && cell.c() == PLACEHOLDER);
        row.semantic_prompt = if i == 0 {
            prompt
        } else if prompt != SemanticPrompt::None {
            SemanticPrompt::PromptContinuation
        } else {
            SemanticPrompt::None
        };
        out.push(row);
    }
}

/// Split `cells` into rows of `columns`. A wide square is never left as
/// the last column: that column becomes a leading spacer and the wide
/// square opens the next row. A one-column grid cannot hold the pair,
/// so the wide square becomes a blank narrow cell and its spacer is dropped.
fn split_cells(
    cells: &[Square],
    columns: usize,
) -> (Vec<Vec<Square>>, Vec<(usize, usize)>) {
    let mut frags: Vec<Vec<Square>> = Vec::new();
    let mut places = Vec::with_capacity(cells.len());
    let mut i = 0;
    while i < cells.len() {
        if frags.last().is_none_or(|frag| frag.len() == columns) {
            frags.push(Vec::new());
        }
        let cell = cells[i];
        if matches!(cell.wide(), Wide::Wide) {
            let room = columns - frags.last().unwrap().len();
            if columns < 2 {
                let frag_idx = frags.len() - 1;
                frags.last_mut().unwrap().push(Square::default());
                let col = frags.last().unwrap().len() - 1;
                places.push((frag_idx, col));
                i += 1;
                if i < cells.len() && matches!(cells[i].wide(), Wide::Spacer) {
                    places.push((frag_idx, col));
                    i += 1;
                }
                continue;
            }
            if room < 2 {
                if room == 1 {
                    let mut spacer = Square::default();
                    spacer.set_wide(Wide::LeadingSpacer);
                    frags.last_mut().unwrap().push(spacer);
                }
                frags.push(Vec::new());
                frags.last_mut().unwrap().push(cell);
                places.push((frags.len() - 1, 0));
                i += 1;
                continue;
            }
        }
        let frag_idx = frags.len() - 1;
        let col = frags.last().unwrap().len();
        frags.last_mut().unwrap().push(cell);
        places.push((frag_idx, col));
        i += 1;
    }
    if frags.len() > 1 {
        for frag in &frags[..frags.len() - 1] {
            debug_assert_eq!(frag.len(), columns);
        }
    }
    (frags, places)
}

/// Make `out`'s last `target` rows the viewport.
///
/// Without `preserve`, a taller screen swallows history rows that are
/// already in `out` and pads only the deficit. With `preserve`, history
/// stays above the viewport and the whole growth is blank rows.
fn fit_viewport(
    out: &mut Vec<Row<Square>>,
    view_rows: usize,
    target: usize,
    columns: usize,
    preserve_scrollback: bool,
) {
    let view_rows = view_rows.min(out.len());
    let hist = out.len() - view_rows;
    if target > view_rows {
        let pull = if preserve_scrollback {
            0
        } else {
            min(hist, target - view_rows)
        };
        let pad = target - view_rows - pull;
        for _ in 0..pad {
            out.push(Row::new(columns));
        }
    }
    while out.len() < target {
        out.push(Row::new(columns));
    }
}

fn trim_to_cap(
    out: &mut Vec<Row<Square>>,
    view_rows: &mut usize,
    cap: usize,
    remap: &mut Option<Vec<i64>>,
    cursor_at: &mut Option<CursorLanding>,
    total_lines_scrolled: &mut u64,
) {
    while out.len() > cap {
        let last = out.len() - 1;
        let cursor_here = cursor_at.is_some_and(|landing| landing.row == last);
        if cursor_here || !row_is_whitespace(out.last().unwrap()) {
            break;
        }
        out.pop();
        if *view_rows > 0 {
            *view_rows -= 1;
        }
        invalidate_remap_row(remap, last);
    }

    let overflow = out.len().saturating_sub(cap);
    if overflow == 0 {
        return;
    }
    let hist = out.len().saturating_sub(*view_rows);
    let drained_view = overflow.saturating_sub(hist);
    *view_rows = view_rows.saturating_sub(drained_view);
    out.drain(0..overflow);
    *total_lines_scrolled += overflow as u64;
    if let Some(map) = remap.as_mut() {
        for pos in map.iter_mut() {
            if *pos >= 0 && (*pos as usize) < overflow {
                *pos = -1;
            }
        }
    }
    if let Some(landing) = cursor_at.as_mut() {
        landing.row = landing.row.saturating_sub(overflow);
    }
}

fn invalidate_remap_row(remap: &mut Option<Vec<i64>>, row: usize) {
    if let Some(map) = remap.as_mut() {
        for pos in map.iter_mut() {
            if *pos == row as i64 {
                *pos = -1;
            }
        }
    }
}

fn cells_for_join(row: &Row<Square>, wrapped: bool) -> Vec<Square> {
    // Trim while WRAPLINE is still set. An explicit empty cell that only
    // continues the line is not padding, and `is_empty` treats that flag
    // as occupied. Clearing the flag first would drop it.
    let mut cells: Vec<Square> = row.inner.iter().copied().collect();
    if let Some(end) = cells.iter().rposition(|cell| !cell.is_empty()) {
        cells.truncate(end + 1);
    } else {
        cells.clear();
    }
    if wrapped
        && matches!(
            cells.last().map(|cell| cell.wide()),
            Some(Wide::LeadingSpacer)
        )
    {
        cells.pop();
    }
    for cell in &mut cells {
        cell.set_wrapline(false);
    }
    cells
}

fn is_pad(cell: Square) -> bool {
    matches!(cell.c(), ' ' | '\0' | '\t')
        && matches!(cell.wide(), Wide::Narrow)
        && !cell.has_extras()
        && !cell.wrapline()
        && !cell.carries_style()
}

fn row_is_whitespace(row: &Row<Square>) -> bool {
    row.semantic_prompt == SemanticPrompt::None
        && row.inner.iter().copied().all(cell_is_whitespace)
}

fn cell_is_whitespace(cell: Square) -> bool {
    if cell.has_extras() || cell.carries_style() {
        return false;
    }
    match cell.wide() {
        Wide::Wide => false,
        Wide::Narrow | Wide::Spacer | Wide::LeadingSpacer => {
            matches!(cell.c(), '\0' | '\t' | ' ')
        }
    }
}
