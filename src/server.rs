//! The session server: a daemon that owns the pty and outlives clients.
//!
//! tmux: server.c (`server_start` forks via `proc_fork_and_daemon`, then
//! runs a libevent loop), server-client.c (per-client state). screen: the
//! `SCREEN` backend process in screen.c, which forks itself away from
//! the attacher on startup.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::Winsize;
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, fork, setsid};

use crate::proto::{self, Msg};
use crate::tty::{Frame, Tty};
use crate::vt::{Parser, Screen};
use crate::pty;

/// What to run in the session's pty.
pub struct Command {
    pub path: CString,
    pub argv: Vec<CString>,
}

/// Binds the session socket, then forks the server into the background.
/// Returns in the original process once the server is running.
pub fn start(name: &str, cmd: Command, ws: Winsize) -> Result<()> {
    let path = crate::session::socket_path(name)?;
    if UnixStream::connect(&path).is_ok() {
        bail!("duplicate session: {name}");
    }
    let _ = fs::remove_file(&path);
    // Binding before the fork means the socket is ready the moment we
    // return, so the caller can connect without polling for it.
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    let log = File::create(path.with_extension("log"))?;

    match unsafe { fork() }? {
        ForkResult::Parent { child } => {
            waitpid(child, None)?;
            Ok(())
        }
        ForkResult::Child => {
            // Double fork: setsid() leaves us a session leader with no
            // controlling tty; forking again means we can never acquire one
            // by accident, e.g. when openpty() opens the slave.
            setsid()?;
            if let ForkResult::Parent { .. } = unsafe { fork() }? {
                unsafe { libc::_exit(0) };
            }
            let null = OpenOptions::new().read(true).write(true).open("/dev/null")?;
            unsafe {
                libc::dup2(null.as_raw_fd(), 0);
                libc::dup2(null.as_raw_fd(), 1);
                libc::dup2(log.as_raw_fd(), 2);
            }
            eprintln!("server {} starting session {name}", std::process::id());
            let code = match run(listener, &path, cmd, ws) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("server error: {e:#}");
                    1
                }
            };
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(path.with_extension("log"));
            std::process::exit(code);
        }
    }
}

struct Client {
    stream: UnixStream,
    buf: Vec<u8>,
    /// What the client's terminal shows; set once it attaches (`Hello`).
    /// Connections from `ls`, `capture` and `kill` never get one.
    tty: Option<Tty>,
    /// For the log: pane output since attaching vs. bytes we sent.
    pane_bytes_at_attach: usize,
    sent: usize,
}

enum Handled {
    Keep,
    Drop,
    KillSession,
}

/// The session's one pane.
struct Session {
    master: File,
    screen: Screen,
    parser: Parser,
    pane_bytes: usize,
}

fn run(listener: UnixListener, path: &Path, cmd: Command, ws: Winsize) -> Result<()> {
    let env = [
        CString::new("TERM=xterm-256color")?,
        CString::new(format!("PTMUX={}", path.display()))?,
    ];
    let pty = pty::spawn(&cmd.path, &cmd.argv, &env, &ws)?;
    let mut s = Session {
        master: File::from(pty.master),
        screen: Screen::new(ws.ws_row.into(), ws.ws_col.into()),
        parser: Parser::default(),
        pane_bytes: 0,
    };
    let mut clients: Vec<Client> = Vec::new();
    let mut killed = false;

    loop {
        let mut fds = vec![
            PollFd::new(listener.as_fd(), PollFlags::POLLIN),
            PollFd::new(s.master.as_fd(), PollFlags::POLLIN),
        ];
        fds.extend(clients.iter().map(|c| PollFd::new(c.stream.as_fd(), PollFlags::POLLIN)));
        match poll(&mut fds, PollTimeout::NONE) {
            Err(Errno::EINTR) => continue,
            r => r?,
        };
        let ready: Vec<bool> = fds.iter().map(readable).collect();
        drop(fds);

        if ready[1] && !s.read_pane()? {
            break;
        }

        let mut i = 0;
        clients.retain_mut(|c| {
            let is_ready = ready[2 + i];
            i += 1;
            if !is_ready || killed {
                return !killed;
            }
            match handle(c, &mut s) {
                Ok(Handled::Keep) => true,
                Ok(Handled::Drop) => false,
                Ok(Handled::KillSession) => {
                    killed = true;
                    false
                }
                Err(e) => {
                    eprintln!("dropping client: {e:#}");
                    false
                }
            }
        });
        if killed {
            break;
        }

        if ready[0] {
            let (stream, _) = listener.accept()?;
            clients.push(Client { stream, buf: Vec::new(), tty: None, pane_bytes_at_attach: 0, sent: 0 });
        }

        // Bring every attached terminal up to date. Pane output is never
        // forwarded as is: each client gets the difference between what its
        // terminal shows and what it should show.
        clients.retain_mut(|c| {
            let Some(tty) = &mut c.tty else { return true };
            let out = tty.draw(&s.frame(tty.rows, tty.cols));
            if out.is_empty() {
                return true;
            }
            c.sent += out.len();
            // Blocking write: a client that stops reading stalls the whole
            // server. tmux avoids that with per-client output buffers.
            Msg::Output(out).send(&mut c.stream).is_ok()
        });
    }

    eprintln!("session over (killed: {killed})");
    for mut c in clients {
        if let Some(tty) = &c.tty {
            let _ = Msg::Output(tty.stop()).send(&mut c.stream);
            let _ = Msg::Exited.send(&mut c.stream);
        }
    }
    // Closing the master hangs up the slave: the kernel sends SIGHUP to the
    // child's session, which is how tmux kill-session ends a pane too.
    drop(s);
    if !killed {
        let _ = waitpid(pty.child, None);
    }
    Ok(())
}

impl Session {
    /// Reads what the pane has written, up to 64 KiB at a time, so a fast
    /// producer is drawn once per batch instead of once per read. Returns
    /// false once the pane has exited.
    fn read_pane(&mut self) -> Result<bool> {
        let mut buf = [0u8; 8192];
        let mut total = 0;
        loop {
            let n = match self.master.read(&mut buf) {
                Ok(0) => return Ok(false),
                // EIO: every slave fd is closed, i.e. the child has exited.
                Err(e) if e.raw_os_error() == Some(libc::EIO) => return Ok(false),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                r => r?,
            };
            self.parser.advance(&mut self.screen, &buf[..n]);
            total += n;
            if total >= 64 * 1024 || !readable_now(self.master.as_fd()) {
                break;
            }
        }
        self.pane_bytes += total;
        if !self.screen.replies.is_empty() {
            let replies = std::mem::take(&mut self.screen.replies);
            (&self.master).write_all(&replies)?;
        }
        Ok(true)
    }

    fn frame(&self, rows: usize, cols: usize) -> Frame {
        Frame::compose(&self.screen, rows, cols)
    }

    /// Fits the pane to a client terminal: the pane is the whole terminal.
    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.screen.resize(rows.into(), cols.into());
        pty::set_winsize(self.master.as_fd(), &pty::winsize(rows, cols))
    }
}

fn handle(c: &mut Client, s: &mut Session) -> Result<Handled> {
    let mut tmp = [0u8; 8192];
    let n = c.stream.read(&mut tmp)?;
    if n == 0 {
        return Ok(Handled::Drop);
    }
    c.buf.extend_from_slice(&tmp[..n]);
    while let Some(msg) = proto::decode(&mut c.buf)? {
        match msg {
            Msg::Hello { rows, cols } => {
                let (rows, cols) = (rows.max(1), cols.max(1));
                eprintln!("client attached at {cols}x{rows}");
                s.resize(rows, cols)?;
                let mut tty = Tty::new(rows.into(), cols.into());
                let mut out = tty.start();
                out.extend(tty.draw(&s.frame(tty.rows, tty.cols)));
                c.sent = out.len();
                c.pane_bytes_at_attach = s.pane_bytes;
                c.tty = Some(tty);
                Msg::Output(out).send(&mut c.stream)?;
            }
            Msg::Resize { rows, cols } => {
                let (rows, cols) = (rows.max(1), cols.max(1));
                s.resize(rows, cols)?;
                if let Some(tty) = &mut c.tty {
                    tty.resize(rows.into(), cols.into());
                }
            }
            Msg::Input(bytes) => (&s.master).write_all(&bytes)?,
            Msg::Detach => {
                if let Some(tty) = &c.tty {
                    eprintln!(
                        "client detached: pane wrote {} bytes, client was sent {}",
                        s.pane_bytes - c.pane_bytes_at_attach,
                        c.sent
                    );
                    // Give the terminal back before the client returns to its shell.
                    Msg::Output(tty.stop()).send(&mut c.stream)?;
                }
                return Ok(Handled::Drop);
            }
            Msg::Capture => {
                Msg::Output(s.screen.text().into_bytes()).send(&mut c.stream)?;
                return Ok(Handled::Drop);
            }
            Msg::Kill => return Ok(Handled::KillSession),
            Msg::Output(_) | Msg::Exited => bail!("unexpected message from client: {msg:?}"),
        }
    }
    Ok(Handled::Keep)
}

fn readable_now(fd: BorrowedFd) -> bool {
    let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
    matches!(poll(&mut fds, PollTimeout::ZERO), Ok(n) if n > 0) && readable(&fds[0])
}

fn readable(fd: &PollFd) -> bool {
    fd.revents()
        .is_some_and(|r| r.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR))
}

/// The command to run when none is given: the user's shell as a login
/// shell (argv[0] starts with '-'), which is what tmux and screen do.
pub fn default_command() -> Result<Command> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let base = PathBuf::from(&shell)
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sh".into());
    Ok(Command {
        path: CString::new(shell)?,
        argv: vec![CString::new(format!("-{base}"))?],
    })
}

/// A user-supplied command line, run through `sh -c` like tmux does.
pub fn shell_command(line: &str) -> Result<Command> {
    Ok(Command {
        path: CString::new("/bin/sh")?,
        argv: vec![CString::new("sh")?, CString::new("-c")?, CString::new(line)?],
    })
}
