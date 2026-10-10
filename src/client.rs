//! The attach client: relays the real terminal to and from the server.
//!
//! tmux: client.c (`client_main`), which sends its tty fds to the server
//! and leaves the drawing to the server. screen: attacher.c. Here the client
//! keeps the tty and handles the prefix key itself, like dtach.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::unistd::{isatty, read};

use crate::proto::{self, Msg};
use crate::{pty, session, term};

/// Ctrl-b, tmux's default prefix.
pub const PREFIX: u8 = 0x02;

/// Splits keyboard input into bytes for the pane and prefix commands.
/// `C-b d` detaches, `C-b C-b` sends a literal C-b, and any other key after
/// the prefix is swallowed, the same as an unbound key in tmux.
#[derive(Default)]
pub struct KeyFilter {
    pending: bool,
}

impl KeyFilter {
    /// Returns the bytes to forward and whether the user asked to detach.
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            if self.pending {
                self.pending = false;
                match b {
                    b'd' => return (out, true),
                    PREFIX => out.push(PREFIX),
                    _ => {}
                }
            } else if b == PREFIX {
                self.pending = true;
            } else {
                out.push(b);
            }
        }
        (out, false)
    }
}

enum Outcome {
    Detached,
    Exited,
    LostServer,
}

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: i32) {
    WINCH.store(true, Ordering::Relaxed);
}

pub fn attach(name: &str) -> Result<()> {
    let stdin = io::stdin();
    let tty = stdin.as_fd();
    if !isatty(tty)? {
        bail!("not a terminal");
    }
    let path = session::socket_path(name)?;
    let mut stream = UnixStream::connect(&path).with_context(|| format!("can't find session: {name}"))?;

    // No SA_RESTART: poll() returns EINTR on resize so we notice it promptly.
    let act = SigAction::new(SigHandler::Handler(on_winch), SaFlags::empty(), SigSet::empty());
    unsafe { sigaction(Signal::SIGWINCH, &act)? };

    let ws = pty::get_winsize(tty)?;
    Msg::Hello { rows: ws.ws_row, cols: ws.ws_col }.send(&mut stream)?;

    let outcome = {
        let _raw = term::RawGuard::enter(tty)?;
        relay(&mut stream)?
    };
    // Undo whatever attributes the pane left on: tmux gets this for free
    // because it redraws the whole terminal itself (milestone 4).
    print!("\x1b[0m\x1b[?25h\r\n");
    match outcome {
        Outcome::Detached => println!("[detached (from session {name})]"),
        Outcome::Exited => println!("[exited]"),
        Outcome::LostServer => println!("[lost server]"),
    }
    Ok(())
}

fn relay(stream: &mut UnixStream) -> Result<Outcome> {
    let stdin = io::stdin();
    let tty = stdin.as_fd();
    let mut stdout = io::stdout().lock();
    let mut keys = KeyFilter::default();
    let mut inbuf = Vec::new();
    let mut buf = [0u8; 8192];

    loop {
        if WINCH.swap(false, Ordering::Relaxed) {
            let ws = pty::get_winsize(tty)?;
            Msg::Resize { rows: ws.ws_row, cols: ws.ws_col }.send(stream)?;
        }

        let mut fds = [
            PollFd::new(tty, PollFlags::POLLIN),
            PollFd::new(stream.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Err(Errno::EINTR) => continue,
            r => r?,
        };
        let ready = fds.map(|fd| {
            fd.revents()
                .is_some_and(|r| r.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR))
        });

        if ready[0] {
            let n = match read(tty, &mut buf) {
                Err(Errno::EINTR | Errno::EAGAIN) => continue,
                r => r?,
            };
            let (bytes, detach) = keys.feed(&buf[..n]);
            if !bytes.is_empty() {
                Msg::Input(bytes).send(stream)?;
            }
            if detach || n == 0 {
                Msg::Detach.send(stream)?;
                return Ok(Outcome::Detached);
            }
        }

        if ready[1] {
            let n = match stream.read(&mut buf) {
                Ok(0) | Err(_) => return Ok(Outcome::LostServer),
                Ok(n) => n,
            };
            inbuf.extend_from_slice(&buf[..n]);
            while let Some(msg) = proto::decode(&mut inbuf)? {
                match msg {
                    Msg::Output(bytes) => stdout.write_all(&bytes)?,
                    Msg::Exited => return Ok(Outcome::Exited),
                    _ => bail!("unexpected message from server: {msg:?}"),
                }
            }
            stdout.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_bytes_pass_through() {
        assert_eq!(KeyFilter::default().feed(b"ls -l\r"), (b"ls -l\r".to_vec(), false));
    }

    #[test]
    fn prefix_d_detaches() {
        let mut k = KeyFilter::default();
        assert_eq!(k.feed(b"ab\x02d"), (b"ab".to_vec(), true));
    }

    #[test]
    fn prefix_split_across_reads() {
        let mut k = KeyFilter::default();
        assert_eq!(k.feed(b"x\x02"), (b"x".to_vec(), false));
        assert_eq!(k.feed(b"d"), (vec![], true));
    }

    #[test]
    fn double_prefix_sends_literal() {
        assert_eq!(KeyFilter::default().feed(b"\x02\x02z"), (b"\x02z".to_vec(), false));
    }

    #[test]
    fn unbound_key_is_swallowed() {
        assert_eq!(KeyFilter::default().feed(b"\x02qz"), (b"z".to_vec(), false));
    }
}
