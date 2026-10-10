//! The status line, modelled on tmux's default one:
//!
//! ```text
//! [0] 0:vim*                                  "OMEN" 14:05 10-Oct-26
//! ```
//!
//! tmux builds it from format strings (`status-left`, `status-right`,
//! `window-status-format`) in status.c and format.c. The window name comes
//! from the pane's *foreground process* (`automatic-rename`), which
//! osdep-linux.c `osdep_get_name()` finds with tcgetpgrp() on the pty
//! master and /proc/<pgrp>/cmdline.

use std::os::fd::{AsRawFd, BorrowedFd};

use unicode_width::UnicodeWidthChar;

use crate::vt::{Cell, Color, Pen};

/// tmux's default `status-style`: black on green.
const STYLE: Pen = Pen { fg: Color::Indexed(0), bg: Color::Indexed(2), flags: 0 };

/// Name of the process group in the foreground of the pty, e.g. "vim".
pub fn foreground_name(master: BorrowedFd) -> Option<String> {
    let pgrp = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
    if pgrp <= 0 {
        return None;
    }
    let comm = std::fs::read_to_string(format!("/proc/{pgrp}/comm")).ok()?;
    Some(comm.trim_end().to_owned())
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Local time as tmux's default status-right shows it: `14:05 10-Oct-26`.
pub fn clock() -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        libc::localtime_r(&now, &mut tm);
    }
    format!(
        "{:02}:{:02} {:02}-{}-{:02}",
        tm.tm_hour,
        tm.tm_min,
        tm.tm_mday,
        MONTHS[tm.tm_mon.clamp(0, 11) as usize],
        tm.tm_year % 100
    )
}

/// Lays out `left` and `right` on a row `cols` wide. If they would
/// overlap, `right` is dropped, as in tmux.
pub fn render(left: &str, right: &str, cols: usize) -> Vec<Cell> {
    let mut row = vec![Cell { pen: STYLE, ..Cell::default() }; cols];
    let used = put(&mut row, 0, left);
    let right_width: usize = right.chars().map(|c| c.width().unwrap_or(0)).sum();
    if used + 1 + right_width <= cols {
        put(&mut row, cols - right_width, right);
    }
    row
}

/// Writes `s` at `col`, clipped to the row. Returns the column after it.
fn put(row: &mut [Cell], mut col: usize, s: &str) -> usize {
    for ch in s.chars() {
        let width = ch.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        if col + width > row.len() {
            break;
        }
        row[col] = Cell { ch, pen: STYLE, width: width as u8 };
        if width == 2 {
            row[col + 1] = Cell { ch: ' ', pen: STYLE, width: 0 };
        }
        col += width;
    }
    col
}

/// tmux's default status line for a single-window session.
pub fn line(session: &str, window: &str, title: &str, cols: usize) -> Vec<Cell> {
    // tmux trims the pane title to 21 columns: "#{=21:pane_title}".
    let title: String = title.chars().take(21).collect();
    render(&format!("[{session}] 0:{window}*"), &format!("\"{title}\" {}", clock()), cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(row: &[Cell]) -> String {
        row.iter().filter(|c| c.width > 0).map(|c| c.ch).collect()
    }

    #[test]
    fn left_and_right() {
        assert_eq!(text(&render("[0] 0:bash*", "\"host\" 12:00", 30)), "[0] 0:bash*       \"host\" 12:00");
    }

    #[test]
    fn right_dropped_when_too_narrow() {
        assert_eq!(text(&render("[0] 0:bash*", "\"host\" 12:00", 20)), "[0] 0:bash*         ");
    }

    #[test]
    fn clock_format() {
        let c = clock();
        assert_eq!(c.len(), "14:05 10-Oct-26".len(), "{c}");
    }
}
