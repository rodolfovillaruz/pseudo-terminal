//! Drawing to a client's real terminal.
//!
//! The server keeps, per client, a copy of what that terminal currently
//! shows (`front`). To update it, it diffs the next `Frame` against that
//! copy and sends only the difference. tmux does the same job in tty.c,
//! but it is driven by commands instead of diffs: each `screen_write_*`
//! operation becomes a `tty_cmd_*` (insert line, clear line, ...) replayed
//! on every client, and a full redraw happens only when needed
//! (screen-redraw.c). screen does the equivalent in display.c.

use std::collections::BTreeSet;
use std::fmt::Write;

use crate::vt::{self, Cell, Color, Pen, Screen};

/// Everything a client's terminal should show at one moment.
pub struct Frame {
    pub cells: Vec<Vec<Cell>>,
    /// Rows `0..pane_rows` hold the pane and may be scrolled as a block.
    pub pane_rows: usize,
    pub cursor: (usize, usize),
    pub cursor_visible: bool,
    pub modes: BTreeSet<u16>,
    pub app_keypad: bool,
    pub title: String,
}

impl Frame {
    /// The pane in the top-left corner of a `rows` x `cols` terminal, with
    /// `status` as the last row. A pane bigger than the terminal (another
    /// client set its size) is clipped.
    pub fn compose(screen: &Screen, rows: usize, cols: usize, status: Vec<Cell>) -> Frame {
        let pane_rows = rows.saturating_sub(1).max(1).min(rows);
        let mut cells = vec![vec![Cell::default(); cols]; rows];
        for (r, line) in cells.iter_mut().enumerate().take(pane_rows.min(screen.rows)) {
            let src = screen.row(r);
            let n = cols.min(screen.cols);
            line[..n].copy_from_slice(&src[..n]);
            if n > 0 && line[n - 1].width == 2 {
                line[n - 1] = Cell::default();
            }
        }
        if rows > 1 {
            let mut status = status;
            status.resize(cols, Cell::default());
            cells[rows - 1] = status;
        }
        Frame {
            cells,
            pane_rows,
            cursor: (screen.cur.row.min(pane_rows - 1), screen.cur.col.min(cols.max(1) - 1)),
            cursor_visible: screen.cursor_visible,
            modes: screen.private_modes.clone(),
            app_keypad: screen.app_keypad,
            title: screen.title.clone(),
        }
    }
}

pub struct Tty {
    pub rows: usize,
    pub cols: usize,
    front: Vec<Vec<Cell>>,
    pen: Pen,
    cursor: Option<(usize, usize)>,
    cursor_visible: bool,
    modes: BTreeSet<u16>,
    app_keypad: bool,
    title: String,
    must_clear: bool,
}

impl Tty {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            front: vec![vec![Cell::default(); cols]; rows],
            pen: Pen::default(),
            cursor: None,
            cursor_visible: true,
            modes: BTreeSet::new(),
            app_keypad: false,
            title: String::new(),
            must_clear: false,
        }
    }

    /// Takes over the terminal (tmux: `tty_start_tty`): switch to the
    /// alternate screen so the user's shell history survives, and turn off
    /// autowrap. Every cell is addressed explicitly, so writing the
    /// bottom-right corner must never scroll.
    pub fn start(&mut self) -> Vec<u8> {
        self.front = vec![vec![Cell::default(); self.cols]; self.rows];
        self.pen = Pen::default();
        b"\x1b[?1049h\x1b[?7l\x1b[0m\x1b[H\x1b[2J".to_vec()
    }

    /// Gives the terminal back (tmux: `tty_stop_tty`).
    pub fn stop(&self) -> Vec<u8> {
        let mut o = String::from("\x1b[0m");
        for m in &self.modes {
            let _ = write!(o, "\x1b[?{m}l");
        }
        if self.app_keypad {
            o.push_str("\x1b>");
        }
        o.push_str("\x1b[?25h\x1b[?7h\x1b[?1049l");
        o.into_bytes()
    }

    /// The terminal changed size: its contents are unknown, so redraw everything.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;
        self.front = vec![vec![Cell::default(); cols]; rows];
        self.cursor = None;
        self.must_clear = true;
    }

    /// Bytes that turn what the terminal shows now into `f`. Empty if
    /// nothing changed.
    pub fn draw(&mut self, f: &Frame) -> Vec<u8> {
        debug_assert_eq!((f.cells.len(), f.cells[0].len()), (self.rows, self.cols));
        let mut body = String::new();
        if self.must_clear {
            body.push_str("\x1b[0m\x1b[H\x1b[2J");
            self.pen = Pen::default();
            self.must_clear = false;
        }
        self.scroll(&mut body, f);
        for r in 0..self.rows {
            self.draw_row(&mut body, r, &f.cells[r]);
        }

        let mut o = String::new();
        let drew = !body.is_empty();
        // Hide the cursor while drawing so it doesn't flicker across the screen.
        if drew && self.cursor_visible {
            o.push_str("\x1b[?25l");
            self.cursor_visible = false;
        }
        o.push_str(&body);
        for m in self.modes.difference(&f.modes) {
            let _ = write!(o, "\x1b[?{m}l");
        }
        for m in f.modes.difference(&self.modes) {
            let _ = write!(o, "\x1b[?{m}h");
        }
        self.modes.clone_from(&f.modes);
        if f.app_keypad != self.app_keypad {
            o.push_str(if f.app_keypad { "\x1b=" } else { "\x1b>" });
            self.app_keypad = f.app_keypad;
        }
        if f.title != self.title {
            let _ = write!(o, "\x1b]2;{}\x07", f.title);
            self.title.clone_from(&f.title);
        }
        if drew || self.cursor != Some(f.cursor) {
            let _ = write!(o, "\x1b[{};{}H", f.cursor.0 + 1, f.cursor.1 + 1);
            self.cursor = Some(f.cursor);
        }
        if f.cursor_visible != self.cursor_visible {
            o.push_str(if f.cursor_visible { "\x1b[?25h" } else { "\x1b[?25l" });
            self.cursor_visible = f.cursor_visible;
        }
        o.into_bytes()
    }

    /// If the pane's contents moved up by k lines (output scrolling), let the
    /// terminal move them: set a scroll region over the pane so the status
    /// line stays put, then SU. tmux does this in `tty_cmd_linefeed` /
    /// `tty_cmd_scrollup`.
    fn scroll(&mut self, o: &mut String, f: &Frame) {
        let n = f.pane_rows.min(self.rows);
        if n < 2 || self.front[0] == f.cells[0] {
            return;
        }
        // Rows that would already be right after scrolling by k. Blank rows
        // don't count: they are cheap to draw anyway.
        let blank = |line: &[Cell]| line.iter().all(|c| *c == Cell::default());
        let kept = |k: usize| (k..n).filter(|&r| self.front[r] == f.cells[r - k] && !blank(&self.front[r])).count();
        let best = (1..n)
            .filter(|&k| self.front[k] == f.cells[0])
            .map(|k| (kept(k), std::cmp::Reverse(k)))
            .max();
        // Scrolling costs about as much as drawing one short row.
        let Some((count, std::cmp::Reverse(k))) = best else { return };
        if count <= kept(0) + 1 {
            return;
        }
        // New lines are filled with the current background, so reset it first.
        if self.pen != Pen::default() {
            o.push_str("\x1b[0m");
            self.pen = Pen::default();
        }
        let _ = write!(o, "\x1b[1;{n}r\x1b[{k}S\x1b[r");
        self.front[..n].rotate_left(k);
        for line in &mut self.front[n - k..n] {
            line.fill(Cell::default());
        }
    }

    fn draw_row(&mut self, o: &mut String, r: usize, new: &[Cell]) {
        let old = &self.front[r];
        let Some(mut first) = (0..self.cols).find(|&c| old[c] != new[c]) else {
            return;
        };
        let mut last = (0..self.cols).rfind(|&c| old[c] != new[c]).unwrap();
        // Always write whole wide characters.
        while first > 0 && (new[first].width == 0 || old[first].width == 0) {
            first -= 1;
        }
        if last + 1 < self.cols && (new[last].width == 2 || old[last].width == 2) {
            last += 1;
        }
        // A run of blanks up to the end of the line is one EL, not N spaces.
        let tail = blank_tail(new);
        let use_el = tail <= last && last + 1 - tail.max(first) > 3;
        let stop = if use_el { tail.max(first) } else { last + 1 };

        let _ = write!(o, "\x1b[{};{}H", r + 1, first + 1);
        for cell in &new[first..stop] {
            if cell.width == 0 {
                continue;
            }
            self.set_pen(o, cell.pen);
            o.push(cell.ch);
        }
        if use_el {
            self.set_pen(o, new[self.cols - 1].pen);
            o.push_str("\x1b[K");
        }
        self.front[r][first..=last].copy_from_slice(&new[first..=last]);
    }

    fn set_pen(&mut self, o: &mut String, pen: Pen) {
        if pen != self.pen {
            o.push_str(&sgr(&pen));
            self.pen = pen;
        }
    }
}

/// Where the trailing run of erasable blanks starts: spaces whose pen has
/// at most a background colour, which is exactly what EL produces.
fn blank_tail(line: &[Cell]) -> usize {
    let last = line[line.len() - 1];
    let erasable = last.ch == ' ' && last.width == 1 && last.pen.flags == 0 && last.pen.fg == Color::Default;
    if !erasable {
        return line.len();
    }
    line.iter().rposition(|c| *c != last).map_or(0, |i| i + 1)
}

fn sgr(pen: &Pen) -> String {
    let mut o = String::from("\x1b[0");
    for (flag, code) in [
        (vt::BOLD, 1),
        (vt::DIM, 2),
        (vt::ITALIC, 3),
        (vt::UNDERLINE, 4),
        (vt::BLINK, 5),
        (vt::REVERSE, 7),
        (vt::HIDDEN, 8),
        (vt::STRIKE, 9),
    ] {
        if pen.flags & flag != 0 {
            let _ = write!(o, ";{code}");
        }
    }
    color(&mut o, pen.fg, 30, 90, 38);
    color(&mut o, pen.bg, 40, 100, 48);
    o.push('m');
    o
}

fn color(o: &mut String, c: Color, base: u8, bright: u8, extended: u8) {
    let _ = match c {
        Color::Default => Ok(()),
        Color::Indexed(n @ 0..8) => write!(o, ";{}", base + n),
        Color::Indexed(n @ 8..16) => write!(o, ";{}", bright + n - 8),
        Color::Indexed(n) => write!(o, ";{extended};5;{n}"),
        Color::Rgb(r, g, b) => write!(o, ";{extended};2;{r};{g};{b}"),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::Parser;

    fn status(text: &str) -> Vec<Cell> {
        let pen = Pen { fg: Color::Indexed(0), bg: Color::Indexed(2), flags: 0 };
        text.chars().map(|ch| Cell { ch, pen, width: 1 }).collect()
    }

    /// Feeds `chunks` to a pane one at a time, drawing a frame after each,
    /// and checks that a terminal receiving only the draw output (modelled
    /// by our own `Screen`) shows exactly each frame. Returns what was sent
    /// for each frame.
    fn simulate(rows: usize, cols: usize, chunks: &[&[u8]]) -> Vec<String> {
        let mut pane = Screen::new(rows - 1, cols);
        let mut pane_parser = Parser::default();
        let mut term = Screen::new(rows, cols);
        let mut term_parser = Parser::default();
        let mut tty = Tty::new(rows, cols);
        term_parser.advance(&mut term, &tty.start());

        let mut outs = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            pane_parser.advance(&mut pane, chunk);
            let frame = Frame::compose(&pane, rows, cols, status(&format!("[t] frame {i}")));
            let out = tty.draw(&frame);
            outs.push(String::from_utf8(out.clone()).unwrap());
            term_parser.advance(&mut term, &out);
            for r in 0..rows {
                assert_eq!(term.row(r), &frame.cells[r][..], "row {r} after chunk {i} ({chunk:?})");
            }
            assert_eq!((term.cur.row, term.cur.col), frame.cursor, "cursor after chunk {i}");
            assert_eq!(term.cursor_visible, frame.cursor_visible);
            assert_eq!(term.private_modes, frame.modes);
        }
        outs
    }

    #[test]
    fn frames_reach_the_terminal_exactly() {
        simulate(6, 20, &[
            b"$ ls\r\nfoo  bar\r\n$ ",
            b"\x1b[1;31mred\x1b[0m \x1b[44mblue bg\x1b[K",
            "wide 中文 text that wraps around".as_bytes(),
            b"\x1b[?1049h\x1b[H\x1b[2Jfull screen\x1b[?25l\x1b[?2004h",
            b"\x1b[3;5Hx\x1b[?1000h",
            b"\x1b[?1049l\x1b[?25h\x1b[?1000l",
            b"\x1b[H\x1b[2J",
            b"\x1b(0lqqqk\x1b(B",
        ]);
    }

    #[test]
    fn scrolling_uses_su() {
        let lines: Vec<Vec<u8>> = (1..=30).map(|i| format!("line number {i}\r\n").into_bytes()).collect();
        let chunks: Vec<&[u8]> = lines.iter().map(|l| &l[..]).collect();
        let outs = simulate(10, 30, &chunks);
        // Once the pane is full, each new line is one scroll of the pane
        // region plus one short row, not a redraw of all nine rows.
        let full_redraw = (1..=9).map(|r| format!("\x1b[{r};1Hline number 99").len()).sum::<usize>();
        for out in &outs[20..] {
            assert!(out.contains("\x1b[1;9r\x1b[1S\x1b[r"), "{out:?}");
            assert!(out.len() < full_redraw / 2, "{} bytes: {out:?}", out.len());
        }
    }

    #[test]
    fn unchanged_frame_sends_nothing() {
        let mut pane = Screen::new(3, 10);
        Parser::default().advance(&mut pane, b"hello");
        let mut tty = Tty::new(4, 10);
        tty.start();
        let frame = Frame::compose(&pane, 4, 10, status("s"));
        assert!(!tty.draw(&frame).is_empty());
        assert!(tty.draw(&frame).is_empty());
    }

    #[test]
    fn resize_redraws_everything() {
        let mut pane = Screen::new(3, 10);
        Parser::default().advance(&mut pane, b"hello");
        let mut tty = Tty::new(4, 10);
        tty.start();
        tty.draw(&Frame::compose(&pane, 4, 10, status("s")));
        tty.resize(5, 12);
        pane.resize(4, 12);
        let out = String::from_utf8(tty.draw(&Frame::compose(&pane, 5, 12, status("s")))).unwrap();
        assert!(out.contains("\x1b[2J") && out.contains("hello"), "{out:?}");
    }

    #[test]
    fn stop_undoes_modes() {
        let mut pane = Screen::new(3, 10);
        Parser::default().advance(&mut pane, b"\x1b[?1000;2004h\x1b=");
        let mut tty = Tty::new(4, 10);
        tty.start();
        tty.draw(&Frame::compose(&pane, 4, 10, status("s")));
        let out = String::from_utf8(tty.stop()).unwrap();
        for seq in ["\x1b[?1000l", "\x1b[?2004l", "\x1b>", "\x1b[?7h", "\x1b[?1049l"] {
            assert!(out.contains(seq), "missing {seq:?} in {out:?}");
        }
    }
}
