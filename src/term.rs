//! Raw mode for the real (outer) terminal.
//!
//! tmux does the same in `tty_start_tty()` (tty.c) and screen in
//! `SetTTY()` (tty.sh): the outer terminal must stop cooking input
//! (no line buffering, no echo, no ^C -> SIGINT) so every byte reaches
//! the program inside the pty untouched.

use std::os::fd::{AsFd, BorrowedFd};

use anyhow::Result;
use nix::sys::termios::{self, SetArg, Termios};

/// Puts a terminal into raw mode and restores the original settings on drop.
pub struct RawGuard<'a> {
    fd: BorrowedFd<'a>,
    saved: Termios,
}

impl<'a> RawGuard<'a> {
    pub fn enter(fd: BorrowedFd<'a>) -> Result<Self> {
        let saved = termios::tcgetattr(fd)?;
        let mut raw = saved.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(fd, SetArg::TCSAFLUSH, &raw)?;
        Ok(Self { fd, saved })
    }
}

impl Drop for RawGuard<'_> {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(self.fd.as_fd(), SetArg::TCSAFLUSH, &self.saved);
    }
}
