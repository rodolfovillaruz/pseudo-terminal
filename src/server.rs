//! The session server: a daemon that owns the pty and outlives clients.
//!
//! tmux: server.c (`server_start` forks via `proc_fork_and_daemon`, then
//! runs a libevent loop), server-client.c (per-client state). screen: the
//! `SCREEN` backend process in screen.c, which forks itself away from
//! the attacher on startup.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::Winsize;
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, fork, setsid};

use crate::proto::{self, Msg};
use crate::vt::{Parser, Screen};
use crate::{pty, render};

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
}

enum Handled {
    Keep,
    Drop,
    KillSession,
}

fn run(listener: UnixListener, path: &Path, cmd: Command, ws: Winsize) -> Result<()> {
    let env = [
        CString::new("TERM=xterm-256color")?,
        CString::new(format!("PTMUX={}", path.display()))?,
    ];
    let pty = pty::spawn(&cmd.path, &cmd.argv, &env, &ws)?;
    let mut master = File::from(pty.master);
    let mut screen = Screen::new(ws.ws_row.into(), ws.ws_col.into());
    let mut parser = Parser::default();
    let mut clients: Vec<Client> = Vec::new();
    let mut buf = [0u8; 8192];
    let mut killed = false;

    loop {
        let mut fds = vec![
            PollFd::new(listener.as_fd(), PollFlags::POLLIN),
            PollFd::new(master.as_fd(), PollFlags::POLLIN),
        ];
        fds.extend(clients.iter().map(|c| PollFd::new(c.stream.as_fd(), PollFlags::POLLIN)));
        match poll(&mut fds, PollTimeout::NONE) {
            Err(Errno::EINTR) => continue,
            r => r?,
        };
        let ready: Vec<bool> = fds.iter().map(readable).collect();
        drop(fds);

        if ready[1] {
            let n = match master.read(&mut buf) {
                Ok(0) => break,
                // EIO: every slave fd is closed, i.e. the child has exited.
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => 0,
                r => r?,
            };
            // Every byte goes through the emulator, so the server always
            // knows what the pane looks like, attached clients or not.
            parser.advance(&mut screen, &buf[..n]);
            // Live output still goes to clients unchanged; milestone 4
            // replaces this with rendering from `screen`.
            // Blocking writes: a client that stops reading stalls the whole
            // server. tmux avoids that with per-client output buffers.
            let msg = Msg::Output(buf[..n].to_vec()).encode();
            clients.retain_mut(|c| c.stream.write_all(&msg).is_ok());
        }

        let mut i = 0;
        clients.retain_mut(|c| {
            let is_ready = ready[2 + i];
            i += 1;
            if !is_ready || killed {
                return !killed;
            }
            match handle(c, &master, &mut screen) {
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
            eprintln!("client connected");
            clients.push(Client { stream, buf: Vec::new() });
        }
    }

    eprintln!("session over (killed: {killed})");
    let reset = Msg::Output(render::reset(&screen));
    for mut c in clients {
        let _ = reset.send(&mut c.stream);
        let _ = Msg::Exited.send(&mut c.stream);
    }
    // Closing the master hangs up the slave: the kernel sends SIGHUP to the
    // child's session, which is how tmux kill-session ends a pane too.
    drop(master);
    if !killed {
        let _ = waitpid(pty.child, None);
    }
    Ok(())
}

fn resize(master: &File, screen: &mut Screen, rows: u16, cols: u16) -> Result<()> {
    screen.resize(rows.into(), cols.into());
    pty::set_winsize(master.as_fd(), &pty::winsize(rows, cols))
}

fn handle(c: &mut Client, master: &File, screen: &mut Screen) -> Result<Handled> {
    let mut tmp = [0u8; 8192];
    let n = c.stream.read(&mut tmp)?;
    if n == 0 {
        eprintln!("client disconnected");
        return Ok(Handled::Drop);
    }
    c.buf.extend_from_slice(&tmp[..n]);
    while let Some(msg) = proto::decode(&mut c.buf)? {
        match msg {
            Msg::Hello { rows, cols } => {
                eprintln!("client attached at {cols}x{rows}");
                // Fit the screen to this client, then paint it from our copy.
                // If the size changed, the program also gets SIGWINCH and
                // redraws itself on top.
                resize(master, screen, rows, cols)?;
                Msg::Output(render::repaint(screen)).send(&mut c.stream)?;
            }
            Msg::Resize { rows, cols } => resize(master, screen, rows, cols)?,
            Msg::Input(bytes) => (&*master).write_all(&bytes)?,
            Msg::Detach => {
                eprintln!("client detached");
                // Undo the pane's terminal modes before the client goes back
                // to its shell. The client reads until we hang up.
                Msg::Output(render::reset(screen)).send(&mut c.stream)?;
                return Ok(Handled::Drop);
            }
            Msg::Capture => {
                Msg::Output(screen.text().into_bytes()).send(&mut c.stream)?;
                return Ok(Handled::Drop);
            }
            Msg::Kill => return Ok(Handled::KillSession),
            Msg::Output(_) | Msg::Exited => bail!("unexpected message from client: {msg:?}"),
        }
    }
    Ok(Handled::Keep)
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
