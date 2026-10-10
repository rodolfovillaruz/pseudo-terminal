//! The virtual screen: a grid of cells plus the terminal state that the
//! parser's actions change.
//!
//! tmux splits this across grid.c (cell storage, with history),
//! screen.c (cursor, modes, title) and screen-write.c (the editing
//! operations, which also queue redraws). screen keeps it all in ansi.c on
//! `struct win` and its `mline` rows.

use std::collections::BTreeSet;

use unicode_width::UnicodeWidthChar;

use super::parser::Perform;

pub const BOLD: u16 = 1 << 0;
pub const DIM: u16 = 1 << 1;
pub const ITALIC: u16 = 1 << 2;
pub const UNDERLINE: u16 = 1 << 3;
pub const BLINK: u16 = 1 << 4;
pub const REVERSE: u16 = 1 << 5;
pub const HIDDEN: u16 = 1 << 6;
pub const STRIKE: u16 = 1 << 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Graphic rendition: what SGR (`CSI ... m`) sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub flags: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub pen: Pen,
    /// 1 for normal cells, 2 for the left half of a wide character, and 0
    /// for the right half that the wide character covers.
    pub width: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Self { ch: ' ', pen: Pen::default(), width: 1 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Charset {
    #[default]
    Ascii,
    /// DEC Special Graphics, used for line drawing (`ESC ( 0`).
    DecSpecial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub pen: Pen,
    /// Set after printing in the last column. The wrap only happens when
    /// the next character arrives, so a full-width line followed by CR LF
    /// does not leave an empty line behind.
    pub wrap_pending: bool,
    /// DECOM: cursor addressing is relative to the scroll region.
    pub origin: bool,
    pub charsets: [Charset; 2],
    /// Which of G0/G1 is active (SI/SO).
    pub shift: usize,
}

/// DEC private modes the screen does not act on but must replay to a newly
/// attached terminal, because they change the bytes that terminal sends:
/// application cursor keys, mouse reporting, focus events, bracketed paste.
pub const REPLAYED_MODES: [u16; 9] = [1, 1000, 1002, 1003, 1004, 1005, 1006, 1015, 2004];

pub struct Screen {
    pub rows: usize,
    pub cols: usize,
    grid: Vec<Vec<Cell>>,
    /// The inactive buffer: the primary screen while `alt` is set, else the alternate one.
    other: Vec<Vec<Cell>>,
    pub alt: bool,
    pub cur: Cursor,
    /// DECSC slots, one per buffer.
    saved: [Option<Cursor>; 2],
    /// Scroll region, inclusive.
    pub top: usize,
    pub bottom: usize,
    pub autowrap: bool,
    pub insert: bool,
    pub cursor_visible: bool,
    pub app_keypad: bool,
    pub private_modes: BTreeSet<u16>,
    pub title: String,
    tabs: Vec<bool>,
    last_char: char,
}

impl Screen {
    pub fn new(rows: usize, cols: usize) -> Self {
        let (rows, cols) = (rows.max(1), cols.max(1));
        Self {
            rows,
            cols,
            grid: vec![vec![Cell::default(); cols]; rows],
            other: vec![vec![Cell::default(); cols]; rows],
            alt: false,
            cur: Cursor::default(),
            saved: [None, None],
            top: 0,
            bottom: rows - 1,
            autowrap: true,
            insert: false,
            cursor_visible: true,
            app_keypad: false,
            private_modes: BTreeSet::new(),
            title: String::new(),
            tabs: (0..cols).map(|c| c % 8 == 0).collect(),
            last_char: ' ',
        }
    }

    pub fn row(&self, r: usize) -> &[Cell] {
        &self.grid[r]
    }

    /// The visible text, one line per row, with trailing blanks trimmed:
    /// what `ptmux capture` prints (tmux: `capture-pane -p`).
    pub fn text(&self) -> String {
        let mut out = String::new();
        for line in &self.grid {
            let s: String = line.iter().filter(|c| c.width > 0).map(|c| c.ch).collect();
            out.push_str(s.trim_end());
            out.push('\n');
        }
        out
    }

    /// Resizes both buffers without reflowing text. When the screen gets
    /// shorter, lines are dropped from the top as needed to keep the cursor
    /// line visible, which is what xterm does.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if (rows, cols) == (self.rows, self.cols) {
            return;
        }
        let dropped = resize_grid(&mut self.grid, rows, cols, self.cur.row);
        resize_grid(&mut self.other, rows, cols, 0);
        self.rows = rows;
        self.cols = cols;
        self.cur.row = self.cur.row.saturating_sub(dropped).min(rows - 1);
        self.cur.col = self.cur.col.min(cols - 1);
        self.cur.wrap_pending = false;
        for saved in self.saved.iter_mut().flatten() {
            saved.row = saved.row.min(rows - 1);
            saved.col = saved.col.min(cols - 1);
        }
        self.top = 0;
        self.bottom = rows - 1;
        let old = self.tabs.len();
        self.tabs.resize(cols, false);
        for c in old..cols {
            self.tabs[c] = c % 8 == 0;
        }
    }

    /// An erased cell. Erasing fills with the current background colour (the
    /// "bce" terminfo capability, which xterm-256color advertises).
    fn blank(&self) -> Cell {
        Cell { pen: Pen { bg: self.cur.pen.bg, ..Pen::default() }, ..Cell::default() }
    }

    fn blank_line(&self) -> Vec<Cell> {
        vec![self.blank(); self.cols]
    }

    // --- cursor movement ---

    fn goto(&mut self, row: usize, col: usize) {
        self.cur.row = row.min(self.rows - 1);
        self.cur.col = col.min(self.cols - 1);
        self.cur.wrap_pending = false;
    }

    /// CUP/HVP/VPA addressing, which is relative to the scroll region in origin mode.
    fn goto_origin(&mut self, row: usize, col: usize) {
        let (lo, hi) = if self.cur.origin { (self.top, self.bottom) } else { (0, self.rows - 1) };
        self.goto((lo + row).min(hi), col);
    }

    fn linefeed(&mut self) {
        self.cur.wrap_pending = false;
        if self.cur.row == self.bottom {
            self.scroll_up(1);
        } else if self.cur.row + 1 < self.rows {
            self.cur.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.cur.wrap_pending = false;
        if self.cur.row == self.top {
            self.scroll_down(1);
        } else if self.cur.row > 0 {
            self.cur.row -= 1;
        }
    }

    fn tab(&mut self, n: usize) {
        for _ in 0..n {
            let next = (self.cur.col + 1..self.cols).find(|&c| self.tabs[c]);
            self.cur.col = next.unwrap_or(self.cols - 1);
        }
        self.cur.wrap_pending = false;
    }

    fn save_cursor(&mut self) {
        self.saved[self.alt as usize] = Some(self.cur);
    }

    fn restore_cursor(&mut self) {
        let saved = self.saved[self.alt as usize].unwrap_or_default();
        self.cur = saved;
        self.goto(saved.row, saved.col);
        self.cur.wrap_pending = saved.wrap_pending;
    }

    // --- editing ---

    fn scroll_up(&mut self, n: usize) {
        let (t, b) = (self.top, self.bottom);
        let n = n.min(b - t + 1);
        self.grid[t..=b].rotate_left(n);
        for r in b + 1 - n..=b {
            self.grid[r] = self.blank_line();
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let (t, b) = (self.top, self.bottom);
        let n = n.min(b - t + 1);
        self.grid[t..=b].rotate_right(n);
        for r in t..t + n {
            self.grid[r] = self.blank_line();
        }
    }

    fn insert_lines(&mut self, n: usize) {
        let (row, b) = (self.cur.row, self.bottom);
        if row < self.top || row > b {
            return;
        }
        let n = n.min(b - row + 1);
        self.grid[row..=b].rotate_right(n);
        for r in row..row + n {
            self.grid[r] = self.blank_line();
        }
        self.goto(row, 0);
    }

    fn delete_lines(&mut self, n: usize) {
        let (row, b) = (self.cur.row, self.bottom);
        if row < self.top || row > b {
            return;
        }
        let n = n.min(b - row + 1);
        self.grid[row..=b].rotate_left(n);
        for r in b + 1 - n..=b {
            self.grid[r] = self.blank_line();
        }
        self.goto(row, 0);
    }

    fn insert_chars(&mut self, n: usize) {
        let (row, col, blank) = (self.cur.row, self.cur.col, self.blank());
        let n = n.min(self.cols - col);
        let line = &mut self.grid[row];
        line.truncate(self.cols - n);
        line.splice(col..col, std::iter::repeat_n(blank, n));
        self.cur.wrap_pending = false;
    }

    fn delete_chars(&mut self, n: usize) {
        let (row, col, blank) = (self.cur.row, self.cur.col, self.blank());
        let n = n.min(self.cols - col);
        let line = &mut self.grid[row];
        line.drain(col..col + n);
        line.extend(std::iter::repeat_n(blank, n));
        self.cur.wrap_pending = false;
    }

    /// Blanks columns `from..to` of `row`.
    fn erase(&mut self, row: usize, from: usize, to: usize) {
        let blank = self.blank();
        self.grid[row][from..to.min(self.cols)].fill(blank);
    }

    fn erase_display(&mut self, mode: usize) {
        let (row, col, rows, cols) = (self.cur.row, self.cur.col, self.rows, self.cols);
        match mode {
            0 => {
                self.erase(row, col, cols);
                (row + 1..rows).for_each(|r| self.erase(r, 0, cols));
            }
            1 => {
                (0..row).for_each(|r| self.erase(r, 0, cols));
                self.erase(row, 0, col + 1);
            }
            2 => (0..rows).for_each(|r| self.erase(r, 0, cols)),
            _ => {} // 3 clears scrollback, which we don't keep yet.
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let (row, col, cols) = (self.cur.row, self.cur.col, self.cols);
        match mode {
            0 => self.erase(row, col, cols),
            1 => self.erase(row, 0, col + 1),
            2 => self.erase(row, 0, cols),
            _ => {}
        }
    }

    /// Before overwriting `col`, blanks the other half of any wide character
    /// that is cut in two.
    fn break_wide(&mut self, row: usize, col: usize) {
        let blank = Cell::default();
        let line = &mut self.grid[row];
        match line[col].width {
            0 if col > 0 => line[col - 1] = blank,
            2 if col + 1 < line.len() => line[col + 1] = blank,
            _ => {}
        }
    }

    fn switch_buffer(&mut self, alt: bool) {
        if self.alt != alt {
            std::mem::swap(&mut self.grid, &mut self.other);
            self.alt = alt;
        }
    }

    fn clear_buffer(&mut self) {
        let line = self.blank_line();
        self.grid.iter_mut().for_each(|l| l.clone_from(&line));
    }

    fn set_private_mode(&mut self, mode: u16, on: bool) {
        match mode {
            6 => {
                self.cur.origin = on;
                self.goto_origin(0, 0);
            }
            7 => self.autowrap = on,
            25 => self.cursor_visible = on,
            47 => self.switch_buffer(on),
            1047 => {
                if !on && self.alt {
                    self.clear_buffer();
                }
                self.switch_buffer(on);
            }
            1048 if on => self.save_cursor(),
            1048 => self.restore_cursor(),
            1049 if on => {
                self.save_cursor();
                self.switch_buffer(true);
                self.clear_buffer();
            }
            1049 => {
                self.switch_buffer(false);
                self.restore_cursor();
            }
            m if REPLAYED_MODES.contains(&m) => {
                if on {
                    self.private_modes.insert(m);
                } else {
                    self.private_modes.remove(&m);
                }
            }
            _ => {}
        }
    }

    fn sgr(&mut self, params: &[Vec<u16>]) {
        let pen = &mut self.cur.pen;
        let mut i = 0;
        while i < params.len() {
            let group = &params[i];
            match group[0] {
                0 => *pen = Pen::default(),
                1 => pen.flags |= BOLD,
                2 => pen.flags |= DIM,
                3 => pen.flags |= ITALIC,
                4 if group.get(1) == Some(&0) => pen.flags &= !UNDERLINE,
                4 | 21 => pen.flags |= UNDERLINE,
                5 | 6 => pen.flags |= BLINK,
                7 => pen.flags |= REVERSE,
                8 => pen.flags |= HIDDEN,
                9 => pen.flags |= STRIKE,
                22 => pen.flags &= !(BOLD | DIM),
                23 => pen.flags &= !ITALIC,
                24 => pen.flags &= !UNDERLINE,
                25 => pen.flags &= !BLINK,
                27 => pen.flags &= !REVERSE,
                28 => pen.flags &= !HIDDEN,
                29 => pen.flags &= !STRIKE,
                n @ 30..=37 => pen.fg = Color::Indexed(n as u8 - 30),
                n @ 90..=97 => pen.fg = Color::Indexed(n as u8 - 90 + 8),
                n @ 40..=47 => pen.bg = Color::Indexed(n as u8 - 40),
                n @ 100..=107 => pen.bg = Color::Indexed(n as u8 - 100 + 8),
                39 => pen.fg = Color::Default,
                49 => pen.bg = Color::Default,
                n @ (38 | 48) => {
                    let (color, used) = extended_color(params, i);
                    if let Some(c) = color {
                        if n == 38 { pen.fg = c } else { pen.bg = c }
                    }
                    i += used;
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn reset(&mut self) {
        let title = std::mem::take(&mut self.title);
        *self = Screen::new(self.rows, self.cols);
        self.title = title;
    }
}

/// Parses the colour after SGR 38/48, in either the `38;5;n` / `38;2;r;g;b`
/// form or the ITU `38:5:n` / `38:2:[cs]:r:g:b` form. Returns the colour
/// and how many extra `;` parameters it used.
fn extended_color(params: &[Vec<u16>], i: usize) -> (Option<Color>, usize) {
    let sub = &params[i][1..];
    let byte = |v: u16| v.min(255) as u8;
    if !sub.is_empty() {
        let color = match sub {
            [5, n, ..] => Some(Color::Indexed(byte(*n))),
            [2, _, r, g, b, ..] => Some(Color::Rgb(byte(*r), byte(*g), byte(*b))),
            [2, r, g, b] => Some(Color::Rgb(byte(*r), byte(*g), byte(*b))),
            _ => None,
        };
        return (color, 0);
    }
    let next = |k: usize| params.get(i + k).map(|g| g[0]);
    match next(1) {
        Some(5) => (next(2).map(|n| Color::Indexed(byte(n))), 2),
        Some(2) => match (next(2), next(3), next(4)) {
            (Some(r), Some(g), Some(b)) => (Some(Color::Rgb(byte(r), byte(g), byte(b))), 4),
            _ => (None, params.len()),
        },
        _ => (None, 0),
    }
}

fn resize_grid(grid: &mut Vec<Vec<Cell>>, rows: usize, cols: usize, keep_row: usize) -> usize {
    let dropped = if keep_row >= rows { keep_row + 1 - rows } else { 0 };
    grid.drain(..dropped.min(grid.len()));
    grid.truncate(rows);
    for line in grid.iter_mut() {
        line.resize(cols, Cell::default());
        if line[cols - 1].width == 2 {
            line[cols - 1] = Cell::default();
        }
    }
    grid.resize(rows, vec![Cell::default(); cols]);
    dropped
}

/// The DEC Special Graphics set: ASCII 0x5f-0x7e drawn as line-drawing glyphs.
fn dec_special(c: char) -> char {
    match c {
        '_' => ' ',
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        c => c,
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        let c = match self.cur.charsets[self.cur.shift] {
            Charset::DecSpecial => dec_special(c),
            Charset::Ascii => c,
        };
        // Zero-width characters (combining marks) are dropped for now.
        let width = c.width().unwrap_or(0);
        if width == 0 || width > self.cols {
            return;
        }
        self.last_char = c;

        if self.cur.wrap_pending && self.autowrap {
            self.cur.col = 0;
            self.linefeed();
        }
        self.cur.wrap_pending = false;
        if self.cur.col + width > self.cols {
            // A wide character that doesn't fit in the last column.
            if self.autowrap {
                self.erase(self.cur.row, self.cur.col, self.cols);
                self.cur.col = 0;
                self.linefeed();
            } else {
                self.cur.col = self.cols - width;
            }
        }
        if self.insert {
            self.insert_chars(width);
        }

        let (row, col, pen) = (self.cur.row, self.cur.col, self.cur.pen);
        self.break_wide(row, col);
        if width == 2 {
            self.break_wide(row, col + 1);
            self.grid[row][col + 1] = Cell { ch: ' ', pen, width: 0 };
        }
        self.grid[row][col] = Cell { ch: c, pen, width: width as u8 };

        if col + width >= self.cols {
            // Without autowrap the cursor just sticks in the last column.
            self.cur.col = self.cols - 1;
            self.cur.wrap_pending = self.autowrap;
        } else {
            self.cur.col = col + width;
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                self.cur.col = self.cur.col.saturating_sub(1);
                self.cur.wrap_pending = false;
            }
            0x09 => self.tab(1),
            0x0a..=0x0c => self.linefeed(),
            0x0d => {
                self.cur.col = 0;
                self.cur.wrap_pending = false;
            }
            0x0e => self.cur.shift = 1,
            0x0f => self.cur.shift = 0,
            _ => {} // BEL and the rest: nothing to draw.
        }
    }

    fn csi_dispatch(&mut self, params: &[Vec<u16>], inter: &[u8], action: char) {
        // Parameter `i`, where 0 or missing means `default`.
        let p = |i: usize, default: usize| {
            params.get(i).map(|g| g[0] as usize).filter(|&v| v != 0).unwrap_or(default)
        };
        let (row, col) = (self.cur.row, self.cur.col);
        match (inter, action) {
            ([], '@') => self.insert_chars(p(0, 1)),
            ([], 'A') => {
                let floor = if row >= self.top { self.top } else { 0 };
                self.goto(row.saturating_sub(p(0, 1)).max(floor), col);
            }
            ([], 'B' | 'e') => {
                let ceil = if row <= self.bottom { self.bottom } else { self.rows - 1 };
                self.goto((row + p(0, 1)).min(ceil), col);
            }
            ([], 'C' | 'a') => self.goto(row, col + p(0, 1)),
            ([], 'D') => self.goto(row, col.saturating_sub(p(0, 1))),
            ([], 'E') => self.goto((row + p(0, 1)).min(self.bottom.max(row)), 0),
            ([], 'F') => self.goto(row.saturating_sub(p(0, 1)), 0),
            ([], 'G' | '`') => self.goto(row, p(0, 1) - 1),
            ([], 'H' | 'f') => self.goto_origin(p(0, 1) - 1, p(1, 1) - 1),
            ([], 'I') => self.tab(p(0, 1)),
            ([], 'J') => self.erase_display(p(0, 0)),
            ([], 'K') => self.erase_line(p(0, 0)),
            ([], 'L') => self.insert_lines(p(0, 1)),
            ([], 'M') => self.delete_lines(p(0, 1)),
            ([], 'P') => self.delete_chars(p(0, 1)),
            ([], 'S') => self.scroll_up(p(0, 1)),
            ([], 'T') => self.scroll_down(p(0, 1)),
            ([], 'X') => {
                self.erase(row, col, col + p(0, 1));
                self.cur.wrap_pending = false;
            }
            ([], 'b') => {
                let c = self.last_char;
                for _ in 0..p(0, 1).min(self.rows * self.cols) {
                    self.print(c);
                }
            }
            ([], 'd') => self.goto_origin(p(0, 1) - 1, col),
            ([], 'g') => match p(0, 0) {
                0 => self.tabs[col] = false,
                3 => self.tabs.fill(false),
                _ => {}
            },
            ([], 'h' | 'l') => {
                if params.iter().any(|g| g[0] == 4) {
                    self.insert = action == 'h';
                }
            }
            ([b'?'], 'h' | 'l') => {
                for g in params {
                    self.set_private_mode(g[0], action == 'h');
                }
            }
            ([], 'm') => self.sgr(params),
            ([], 'r') => {
                let (top, bottom) = (p(0, 1) - 1, p(1, self.rows).min(self.rows) - 1);
                if top < bottom {
                    self.top = top;
                    self.bottom = bottom;
                    self.goto_origin(0, 0);
                }
            }
            ([], 's') => self.save_cursor(),
            ([], 'u') => self.restore_cursor(),
            // Queries (DSR, DA, ...) are answered by the real terminal for
            // now, since pane output still reaches it unchanged.
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, inter: &[u8], byte: u8) {
        let charset = |b: u8| if b == b'0' { Charset::DecSpecial } else { Charset::Ascii };
        match (inter, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => self.linefeed(),
            ([], b'E') => {
                self.cur.col = 0;
                self.linefeed();
            }
            ([], b'H') => self.tabs[self.cur.col] = true,
            ([], b'M') => self.reverse_index(),
            ([], b'c') => self.reset(),
            ([], b'=') => self.app_keypad = true,
            ([], b'>') => self.app_keypad = false,
            ([b'('], b) => self.cur.charsets[0] = charset(b),
            ([b')'], b) => self.cur.charsets[1] = charset(b),
            ([b'#'], b'8') => {
                // DECALN: fill the screen with 'E' (used by vttest).
                let e = Cell { ch: 'E', ..Cell::default() };
                self.grid.iter_mut().for_each(|l| l.fill(e));
                self.top = 0;
                self.bottom = self.rows - 1;
                self.goto(0, 0);
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]]) {
        if let [b"0" | b"2", rest @ ..] = params {
            self.title = String::from_utf8_lossy(&rest.join(&b';')).into_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::Parser;

    fn screen(rows: usize, cols: usize, input: &[u8]) -> Screen {
        let mut s = Screen::new(rows, cols);
        Parser::default().advance(&mut s, input);
        s
    }

    fn lines(s: &Screen) -> Vec<String> {
        s.text().lines().map(str::to_owned).collect()
    }

    #[test]
    fn text_and_newlines() {
        let s = screen(3, 10, b"hello\r\nworld");
        assert_eq!(lines(&s), ["hello", "world", ""]);
        assert_eq!((s.cur.row, s.cur.col), (1, 5));
    }

    #[test]
    fn wrap_is_deferred() {
        let s = screen(3, 5, b"abcde");
        assert_eq!((s.cur.row, s.cur.col, s.cur.wrap_pending), (0, 4, true));
        let s = screen(3, 5, b"abcde\r\nx");
        assert_eq!(lines(&s), ["abcde", "x", ""]);
        let s = screen(3, 5, b"abcdefg");
        assert_eq!(lines(&s), ["abcde", "fg", ""]);
    }

    #[test]
    fn scrolls_at_bottom() {
        let s = screen(2, 5, b"1\r\n2\r\n3");
        assert_eq!(lines(&s), ["2", "3"]);
    }

    #[test]
    fn cursor_position_and_erase() {
        let s = screen(3, 5, b"aaaaa\r\nbbbbb\r\nccccc\x1b[2;3H\x1b[J");
        assert_eq!(lines(&s), ["aaaaa", "bb", ""]);
        let s = screen(1, 5, b"abcde\x1b[3G\x1b[1K");
        assert_eq!(lines(&s), ["   de"]);
    }

    #[test]
    fn scroll_region() {
        let s = screen(4, 3, b"a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[3;1H\nx");
        assert_eq!(lines(&s), ["a", "c", "x", "d"]);
    }

    #[test]
    fn insert_and_delete() {
        let s = screen(1, 6, b"abcdef\x1b[1;2H\x1b[2P");
        assert_eq!(lines(&s), ["adef"]);
        let s = screen(1, 6, b"abcdef\x1b[1;2H\x1b[2@");
        assert_eq!(lines(&s), ["a  bcd"]);
        let s = screen(3, 1, b"a\r\nb\r\nc\x1b[1;1H\x1b[L");
        assert_eq!(lines(&s), ["", "a", "b"]);
    }

    #[test]
    fn sgr_colors() {
        let s = screen(1, 5, b"\x1b[1;31;48;5;200ma\x1b[0;38:2::1:2:3mb\x1b[38;2;4;5;6mc");
        let row = s.row(0);
        assert_eq!(row[0].pen, Pen { fg: Color::Indexed(1), bg: Color::Indexed(200), flags: BOLD });
        assert_eq!(row[1].pen.fg, Color::Rgb(1, 2, 3));
        assert_eq!(row[2].pen.fg, Color::Rgb(4, 5, 6));
    }

    #[test]
    fn alternate_screen_preserves_primary() {
        let mut s = screen(2, 5, b"main");
        Parser::default().advance(&mut s, b"\x1b[?1049h\x1b[Halt");
        assert_eq!(lines(&s), ["alt", ""]);
        Parser::default().advance(&mut s, b"\x1b[?1049l");
        assert_eq!(lines(&s), ["main", ""]);
        assert_eq!((s.cur.row, s.cur.col), (0, 4));
    }

    #[test]
    fn wide_characters() {
        let s = screen(1, 4, "a中b".as_bytes());
        assert_eq!(lines(&s), ["a中b"]);
        assert_eq!(s.row(0)[2].width, 0);
        // Overwriting the right half breaks the wide character.
        let s = screen(1, 4, "a中\x1b[3Gx".as_bytes());
        assert_eq!(lines(&s), ["a x"]);
    }

    #[test]
    fn line_drawing() {
        let s = screen(1, 5, b"\x1b(0lqk\x1b(Bq");
        assert_eq!(lines(&s), ["┌─┐q"]);
    }

    #[test]
    fn resize_keeps_cursor_line() {
        let mut s = screen(4, 5, b"1\r\n2\r\n3\r\n4");
        s.resize(2, 3);
        assert_eq!(lines(&s), ["3", "4"]);
        assert_eq!((s.cur.row, s.cur.col), (1, 1));
    }

    #[test]
    fn title_and_modes() {
        let s = screen(1, 5, b"\x1b]2;vim\x07\x1b[?2004h\x1b[?1;1000h\x1b[?1000l\x1b[?25l");
        assert_eq!(s.title, "vim");
        assert_eq!(s.private_modes, BTreeSet::from([1, 2004]));
        assert!(!s.cursor_visible);
    }
}
