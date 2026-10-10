//! Turning a `Screen` back into bytes for a real terminal.
//!
//! For now this means a full repaint when a client attaches, and a reset
//! when it leaves. tmux does both in tty.c: `tty_draw_line` /
//! `screen_redraw_screen` paint, and `tty_start_tty` / `tty_stop_tty` set
//! and undo terminal modes. screen does it in display.c (`Activate`,
//! `FinitTerm`).

use std::fmt::Write;

use crate::vt::{self, Cell, Charset, Color, Pen, Screen};

/// Bytes that make a terminal show exactly what `s` holds and put it in
/// the same state (modes, cursor, pen), so that live output from the pane
/// can continue on top of it.
pub fn repaint(s: &Screen) -> Vec<u8> {
    let mut o = String::new();
    // Known starting point: ASCII charset, no margins, absolute addressing,
    // no autowrap while drawing (so the last column never scrolls), no
    // insert mode, then clear.
    o.push_str("\x1b(B\x1b)B\x0f");
    if s.alt {
        o.push_str("\x1b[?1049h");
    }
    o.push_str("\x1b[0m\x1b[r\x1b[?6l\x1b[?7l\x1b[4l\x1b[H\x1b[2J");

    let mut pen = Pen::default();
    for r in 0..s.rows {
        let row = s.row(r);
        let Some(last) = row.iter().rposition(|c| *c != Cell::default()) else {
            continue;
        };
        let _ = write!(o, "\x1b[{};1H", r + 1);
        for cell in &row[..=last] {
            if cell.width == 0 {
                continue;
            }
            if cell.pen != pen {
                o.push_str(&sgr(&cell.pen));
                pen = cell.pen;
            }
            o.push(cell.ch);
        }
    }

    if s.autowrap {
        o.push_str("\x1b[?7h");
    }
    if (s.top, s.bottom) != (0, s.rows - 1) {
        let _ = write!(o, "\x1b[{};{}r", s.top + 1, s.bottom + 1);
    }
    if s.cur.origin {
        o.push_str("\x1b[?6h");
    }
    let row_offset = if s.cur.origin { s.top } else { 0 };

    if s.cur.wrap_pending && s.autowrap {
        // Pending wrap can't be set with CUP. Redraw the last cell instead,
        // which leaves the terminal in the same state.
        let line = s.row(s.cur.row);
        let col = if line[s.cols - 1].width == 0 { s.cols - 2 } else { s.cols - 1 };
        let cell = line[col];
        let _ = write!(o, "\x1b[{};{}H{}{}", s.cur.row - row_offset + 1, col + 1, sgr(&cell.pen), cell.ch);
    } else {
        let _ = write!(o, "\x1b[{};{}H", s.cur.row - row_offset + 1, s.cur.col + 1);
    }
    o.push_str(&sgr(&s.cur.pen));

    if s.cur.charsets[0] == Charset::DecSpecial {
        o.push_str("\x1b(0");
    }
    if s.cur.charsets[1] == Charset::DecSpecial {
        o.push_str("\x1b)0");
    }
    if s.cur.shift == 1 {
        o.push('\x0e');
    }
    if s.insert {
        o.push_str("\x1b[4h");
    }
    if s.app_keypad {
        o.push_str("\x1b=");
    }
    for m in &s.private_modes {
        let _ = write!(o, "\x1b[?{m}h");
    }
    o.push_str(if s.cursor_visible { "\x1b[?25h" } else { "\x1b[?25l" });
    if !s.title.is_empty() {
        let _ = write!(o, "\x1b]2;{}\x07", s.title);
    }
    o.into_bytes()
}

/// Bytes that undo what the pane did to the terminal, sent when a client
/// detaches or the session ends: leave the alternate screen, and turn off
/// mouse reporting, bracketed paste, application keys and so on.
pub fn reset(s: &Screen) -> Vec<u8> {
    let mut o = String::from("\x1b[0m\x1b(B\x1b)B\x0f\x1b[4l\x1b[?7h\x1b[?25h");
    for m in &s.private_modes {
        let _ = write!(o, "\x1b[?{m}l");
    }
    if s.app_keypad {
        o.push_str("\x1b>");
    }
    if s.cur.origin {
        o.push_str("\x1b[?6l");
    }
    if (s.top, s.bottom) != (0, s.rows - 1) {
        // DECSTBM homes the cursor; keep it where it is.
        o.push_str("\x1b7\x1b[r\x1b8");
    }
    if s.alt {
        o.push_str("\x1b[?1049l");
    }
    o.into_bytes()
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

    fn screen(rows: usize, cols: usize, input: &[u8]) -> Screen {
        let mut s = Screen::new(rows, cols);
        Parser::default().advance(&mut s, input);
        s
    }

    /// Repainting a screen onto a blank terminal must reproduce it: same
    /// cells, cursor and modes. A fresh `Screen` stands in for that terminal.
    fn assert_roundtrip(rows: usize, cols: usize, input: &[u8]) {
        let a = screen(rows, cols, input);
        let b = screen(rows, cols, &repaint(&a));
        for r in 0..rows {
            assert_eq!(a.row(r), b.row(r), "row {r} differs for input {input:?}");
        }
        assert_eq!(a.cur, b.cur, "cursor differs for input {input:?}");
        assert_eq!(
            (a.alt, a.top, a.bottom, a.autowrap, a.insert, a.cursor_visible, a.app_keypad),
            (b.alt, b.top, b.bottom, b.autowrap, b.insert, b.cursor_visible, b.app_keypad),
        );
        assert_eq!(a.private_modes, b.private_modes);
        assert_eq!(a.title, b.title);
    }

    #[test]
    fn roundtrips() {
        assert_roundtrip(3, 10, b"$ ls\r\nfoo bar\r\n$ ");
        assert_roundtrip(3, 5, b"abcde");
        assert_roundtrip(3, 6, "ab中\x1b[1;5H中".as_bytes());
        assert_roundtrip(4, 10, b"\x1b[1;31mred\x1b[0;44m\x1b[K\x1b[38;2;1;2;3;4mx\x1b[2;3H");
        assert_roundtrip(4, 10, b"main\x1b[?1049h\x1b[2;3r\x1b[?6h\x1b[2;2Halt\x1b[?2004h\x1b[?1h\x1b=");
        assert_roundtrip(2, 8, b"\x1b(0lqqk\x1b[?25l\x1b[4h\x1b]0;title\x07");
        assert_roundtrip(2, 4, b"\x1b[?7labcdef");
    }

    #[test]
    fn reset_undoes_modes() {
        let s = screen(4, 10, b"\x1b[?1049h\x1b[?1000;2004h\x1b=\x1b[2;3r");
        let out = String::from_utf8(reset(&s)).unwrap();
        for seq in ["\x1b[?1000l", "\x1b[?2004l", "\x1b>", "\x1b[r", "\x1b[?1049l"] {
            assert!(out.contains(seq), "missing {seq:?} in {out:?}");
        }
        let plain = String::from_utf8(reset(&screen(4, 10, b"hi"))).unwrap();
        assert!(!plain.contains("1049"));
    }
}
