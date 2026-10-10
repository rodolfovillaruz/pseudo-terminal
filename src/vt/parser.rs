//! A VT500-series escape sequence parser.
//!
//! This follows Paul Williams' state diagram (https://vt100.net/emu/dec_ansi_parser),
//! simplified: DCS, SOS, PM and APC strings are recognised and skipped,
//! and bytes >= 0x80 are treated as UTF-8 instead of C1 controls, as modern
//! terminals do. tmux's equivalent is input.c: the same states appear there as
//! `input_state_*` tables. screen's is the `DoESC`/`DoCSI` switch in ansi.c.

/// Receives the parsed actions. `Screen` implements this.
pub trait Perform {
    fn print(&mut self, c: char);
    fn execute(&mut self, byte: u8);
    /// `params` holds one group per `;`-separated parameter, and each group
    /// holds its `:`-separated sub-parameters. A missing parameter reads as 0.
    fn csi_dispatch(&mut self, params: &[Vec<u16>], intermediates: &[u8], action: char);
    fn esc_dispatch(&mut self, intermediates: &[u8], byte: u8);
    fn osc_dispatch(&mut self, params: &[&[u8]]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    OscString,
    /// DCS/SOS/PM/APC: swallowed until the string terminator.
    IgnoredString,
}

const MAX_PARAMS: usize = 32;
const MAX_OSC: usize = 4096;

pub struct Parser {
    state: State,
    intermediates: Vec<u8>,
    params: Vec<Vec<u16>>,
    osc: Vec<u8>,
    utf8: Vec<u8>,
    utf8_len: usize,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            state: State::Ground,
            intermediates: Vec::new(),
            params: vec![vec![0]],
            osc: Vec::new(),
            utf8: Vec::new(),
            utf8_len: 0,
        }
    }
}

impl Parser {
    pub fn advance(&mut self, p: &mut impl Perform, bytes: &[u8]) {
        for &b in bytes {
            self.byte(p, b);
        }
    }

    fn byte(&mut self, p: &mut impl Perform, b: u8) {
        if self.utf8_len > 0 {
            if (0x80..=0xbf).contains(&b) {
                self.utf8.push(b);
                if self.utf8.len() == self.utf8_len {
                    let c = std::str::from_utf8(&self.utf8).ok().and_then(|s| s.chars().next());
                    p.print(c.unwrap_or('\u{fffd}'));
                    self.utf8_len = 0;
                }
                return;
            }
            // Truncated sequence: report it, then handle `b` normally.
            p.print('\u{fffd}');
            self.utf8_len = 0;
        }

        // Transitions from any state.
        match b {
            0x18 | 0x1a => {
                p.execute(b);
                self.state = State::Ground;
                return;
            }
            0x1b => {
                // ESC ends an OSC string (as the first half of ST, ESC \).
                if self.state == State::OscString {
                    self.dispatch_osc(p);
                }
                self.intermediates.clear();
                self.state = State::Escape;
                return;
            }
            _ => {}
        }

        match self.state {
            State::Ground => match b {
                0x00..=0x1f => p.execute(b),
                0x20..=0x7e => p.print(b as char),
                0x7f => {}
                0xc2..=0xdf => self.start_utf8(b, 2),
                0xe0..=0xef => self.start_utf8(b, 3),
                0xf0..=0xf4 => self.start_utf8(b, 4),
                _ => p.print('\u{fffd}'),
            },
            State::Escape => match b {
                0x00..=0x1f => p.execute(b),
                0x20..=0x2f => {
                    self.intermediates.push(b);
                    self.state = State::EscapeIntermediate;
                }
                b'[' => {
                    self.params = vec![vec![0]];
                    self.state = State::CsiEntry;
                }
                b']' => {
                    self.osc.clear();
                    self.state = State::OscString;
                }
                b'P' | b'X' | b'^' | b'_' => self.state = State::IgnoredString,
                0x30..=0x7e => {
                    p.esc_dispatch(&self.intermediates, b);
                    self.state = State::Ground;
                }
                _ => {}
            },
            State::EscapeIntermediate => match b {
                0x00..=0x1f => p.execute(b),
                0x20..=0x2f => self.intermediates.push(b),
                0x30..=0x7e => {
                    p.esc_dispatch(&self.intermediates, b);
                    self.state = State::Ground;
                }
                _ => {}
            },
            State::CsiEntry | State::CsiParam | State::CsiIntermediate => self.csi_byte(p, b),
            State::CsiIgnore => match b {
                0x00..=0x1f => p.execute(b),
                0x40..=0x7e => self.state = State::Ground,
                _ => {}
            },
            State::OscString => match b {
                0x07 => {
                    self.dispatch_osc(p);
                    self.state = State::Ground;
                }
                0x00..=0x1f => {}
                _ if self.osc.len() < MAX_OSC => self.osc.push(b),
                _ => {}
            },
            State::IgnoredString => {}
        }
    }

    fn csi_byte(&mut self, p: &mut impl Perform, b: u8) {
        let in_params = self.state != State::CsiIntermediate;
        match b {
            0x00..=0x1f => p.execute(b),
            b'0'..=b'9' if in_params => {
                let v = self.params.last_mut().unwrap().last_mut().unwrap();
                *v = v.saturating_mul(10).saturating_add(u16::from(b - b'0'));
                self.state = State::CsiParam;
            }
            b';' if in_params => {
                if self.params.len() < MAX_PARAMS {
                    self.params.push(vec![0]);
                }
                self.state = State::CsiParam;
            }
            b':' if in_params => {
                self.params.last_mut().unwrap().push(0);
                self.state = State::CsiParam;
            }
            // Private markers such as '?' are only valid right after CSI.
            b'<'..=b'?' if self.state == State::CsiEntry => {
                self.intermediates.push(b);
                self.state = State::CsiParam;
            }
            0x20..=0x2f => {
                self.intermediates.push(b);
                self.state = State::CsiIntermediate;
            }
            0x40..=0x7e => {
                p.csi_dispatch(&self.params, &self.intermediates, b as char);
                self.state = State::Ground;
            }
            _ => self.state = State::CsiIgnore,
        }
    }

    fn start_utf8(&mut self, b: u8, len: usize) {
        self.utf8.clear();
        self.utf8.push(b);
        self.utf8_len = len;
    }

    fn dispatch_osc(&mut self, p: &mut impl Perform) {
        let parts: Vec<&[u8]> = self.osc.split(|&b| b == b';').collect();
        p.osc_dispatch(&parts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log(Vec<String>);

    impl Perform for Log {
        fn print(&mut self, c: char) {
            self.0.push(format!("print {c}"));
        }
        fn execute(&mut self, b: u8) {
            self.0.push(format!("exec {b:#04x}"));
        }
        fn csi_dispatch(&mut self, params: &[Vec<u16>], inter: &[u8], action: char) {
            let inter = String::from_utf8_lossy(inter);
            self.0.push(format!("csi {inter}{params:?} {action}"));
        }
        fn esc_dispatch(&mut self, inter: &[u8], b: u8) {
            let inter = String::from_utf8_lossy(inter);
            self.0.push(format!("esc {inter}{}", b as char));
        }
        fn osc_dispatch(&mut self, params: &[&[u8]]) {
            let params: Vec<_> = params.iter().map(|p| String::from_utf8_lossy(p)).collect();
            self.0.push(format!("osc {}", params.join("|")));
        }
    }

    fn parse(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = Parser::default();
        let mut log = Log::default();
        for c in chunks {
            parser.advance(&mut log, c);
        }
        log.0
    }

    #[test]
    fn text_and_controls() {
        assert_eq!(parse(&[b"a\r\n"]), ["print a", "exec 0x0d", "exec 0x0a"]);
    }

    #[test]
    fn csi_params_and_private_marker() {
        assert_eq!(parse(&[b"\x1b[1;22H"]), ["csi [[1], [22]] H"]);
        assert_eq!(parse(&[b"\x1b[H"]), ["csi [[0]] H"]);
        assert_eq!(parse(&[b"\x1b[?1049h"]), ["csi ?[[1049]] h"]);
        assert_eq!(parse(&[b"\x1b[2 q"]), ["csi  [[2]] q"]);
    }

    #[test]
    fn subparameters() {
        assert_eq!(parse(&[b"\x1b[38:2::1:2:3m"]), ["csi [[38, 2, 0, 1, 2, 3]] m"]);
    }

    #[test]
    fn split_across_reads() {
        assert_eq!(parse(&[b"\x1b", b"[3", b"1m", b"\xe2\x94", b"\x80"]), ["csi [[31]] m", "print ─"]);
    }

    #[test]
    fn esc_sequences() {
        assert_eq!(parse(&[b"\x1b7\x1b(0"]), ["esc 7", "esc (0"]);
    }

    #[test]
    fn osc_with_bel_and_st() {
        assert_eq!(parse(&[b"\x1b]0;hi there\x07"]), ["osc 0|hi there"]);
        assert_eq!(parse(&[b"\x1b]2;x\x1b\\"]), ["osc 2|x", "esc \\"]);
    }

    #[test]
    fn dcs_is_skipped() {
        assert_eq!(parse(&[b"\x1bP+q544e\x1b\\z"]), ["esc \\", "print z"]);
    }

    #[test]
    fn invalid_utf8() {
        assert_eq!(parse(&[b"\xe2\x94z"]), ["print \u{fffd}", "print z"]);
    }

    #[test]
    fn cancel_aborts_sequence() {
        assert_eq!(parse(&[b"\x1b[31\x18x"]), ["exec 0x18", "print x"]);
    }
}
