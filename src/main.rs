mod pty;
mod term;

use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::{Parser, Subcommand};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::sys::wait::waitpid;
use nix::unistd::{read, write};

#[derive(Parser)]
#[command(about = "A tiny tmux/screen clone built to learn how they work")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a shell inside a new pty (milestone 1: plain passthrough)
    New {
        /// Program to run instead of $SHELL
        #[arg(short, long)]
        command: Option<String>,
    },
}

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: i32) {
    WINCH.store(true, Ordering::Relaxed);
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::New { command } => {
            let shell = command
                .or_else(|| std::env::var("SHELL").ok())
                .unwrap_or_else(|| "/bin/sh".into());
            passthrough(&shell)
        }
    }
}

/// Milestone 1: the core loop every terminal multiplexer is built around.
/// Bytes from the real terminal go to the pty master, bytes from the pty
/// master go to the real terminal.
fn passthrough(shell: &str) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let tty = stdin.as_fd();

    let pty = pty::spawn(shell, &pty::get_winsize(tty)?)?;

    // No SA_RESTART: poll() returns EINTR on resize so we notice it promptly.
    let act = SigAction::new(SigHandler::Handler(on_winch), SaFlags::empty(), SigSet::empty());
    unsafe { sigaction(Signal::SIGWINCH, &act)? };

    let _raw = term::RawGuard::enter(tty)?;
    let mut buf = [0u8; 8192];

    loop {
        if WINCH.swap(false, Ordering::Relaxed) {
            pty::set_winsize(pty.master.as_fd(), &pty::get_winsize(tty)?)?;
        }

        let mut fds = [
            PollFd::new(tty, PollFlags::POLLIN),
            PollFd::new(pty.master.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Err(Errno::EINTR) => continue,
            r => r?,
        };

        if ready(&fds[0]) && !copy(tty, pty.master.as_fd(), &mut buf)? {
            break;
        }
        // EIO on the master means the slave side has been closed: the child exited.
        if ready(&fds[1]) && !copy(pty.master.as_fd(), stdout.as_fd(), &mut buf)? {
            break;
        }
    }

    let _ = waitpid(pty.child, None);
    Ok(())
}

fn ready(fd: &PollFd) -> bool {
    fd.revents()
        .is_some_and(|r| r.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR))
}

/// Copies one read's worth of bytes. Returns false at end of input.
fn copy(from: BorrowedFd, to: BorrowedFd, buf: &mut [u8]) -> Result<bool> {
    let n = match read(from, buf) {
        Ok(0) | Err(Errno::EIO) => return Ok(false),
        Err(Errno::EINTR | Errno::EAGAIN) => return Ok(true),
        r => r?,
    };
    let mut off = 0;
    while off < n {
        match write(to, &buf[off..n]) {
            Err(Errno::EINTR) => continue,
            r => off += r?,
        }
    }
    Ok(true)
}
