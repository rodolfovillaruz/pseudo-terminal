//! Pseudo-terminal creation and child spawning.
//!
//! This is done by hand (openpty + fork + setsid + TIOCSCTTY + dup2 + exec)
//! instead of forkpty(3) so every step is visible. Compare with tmux
//! `spawn_pane()` (spawn.c), which calls fdforkpty(), and screen
//! `OpenDevice()` / `ForkWindow()` (pty.c, window.c).

use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

use anyhow::{Context, Result};
use nix::pty::{Winsize, openpty};
use nix::unistd::{ForkResult, Pid, fork};

pub struct Pty {
    pub master: OwnedFd,
    pub child: Pid,
}

/// Reads the window size of a terminal (TIOCGWINSZ).
pub fn get_winsize(fd: BorrowedFd) -> Result<Winsize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } < 0 {
        return Err(std::io::Error::last_os_error()).context("TIOCGWINSZ");
    }
    Ok(Winsize {
        ws_row: ws.ws_row,
        ws_col: ws.ws_col,
        ws_xpixel: ws.ws_xpixel,
        ws_ypixel: ws.ws_ypixel,
    })
}

/// Sets the window size of a pty master (TIOCSWINSZ). If the size actually
/// changed, the kernel sends SIGWINCH to the foreground process group on
/// the slave side.
pub fn set_winsize(fd: BorrowedFd, ws: &Winsize) -> Result<()> {
    let ws = libc::winsize {
        ws_row: ws.ws_row,
        ws_col: ws.ws_col,
        ws_xpixel: ws.ws_xpixel,
        ws_ypixel: ws.ws_ypixel,
    };
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(std::io::Error::last_os_error()).context("TIOCSWINSZ");
    }
    Ok(())
}

pub fn winsize(rows: u16, cols: u16) -> Winsize {
    Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

/// Opens a pty of the given size and execs `path` with `argv` on its slave
/// side. `env` holds extra `KEY=VALUE` strings for the child.
pub fn spawn(path: &CStr, argv: &[CString], env: &[CString], ws: &Winsize) -> Result<Pty> {
    let pair = openpty(Some(ws), None).context("openpty")?;

    // Allocate everything before fork: only async-signal-safe calls in the child.
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());

    match unsafe { fork() }.context("fork")? {
        ForkResult::Child => unsafe {
            let slave = pair.slave.as_raw_fd();
            // New session, then make the slave our controlling terminal.
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            libc::close(slave);
            libc::close(pair.master.as_raw_fd());
            for e in env {
                libc::putenv(e.as_ptr() as *mut _);
            }
            libc::execvp(path.as_ptr(), argv_ptrs.as_ptr());
            libc::_exit(127);
        },
        ForkResult::Parent { child } => {
            drop(pair.slave);
            Ok(Pty {
                master: pair.master,
                child,
            })
        }
    }
}
