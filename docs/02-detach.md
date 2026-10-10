# 02 — Detach and reattach

Goal: the pty and the program in it outlive the terminal you started them
from. This is the dtach feature set: a background server owns the pty, and
clients come and go over a Unix socket.

## What ptmux does

**Starting a server** (`src/server.rs` `start()`):

1. Bind `$XDG_RUNTIME_DIR/ptmux/<name>` *before* forking. The caller can
   then connect right away without waiting for the socket to appear.
2. `fork()` → `setsid()` → `fork()` again. After `setsid` the child is a
   session leader with no controlling tty. Under System V rules, a session
   leader that opens a tty without `O_NOCTTY` acquires it as its controlling
   terminal. The second fork gives a process that is *not* a session leader,
   so this can never happen, even when `openpty()` opens the slave.
3. Point stdin and stdout at `/dev/null` and stderr at `<name>.log`. The log
   is our equivalent of `tmux -v`.
4. Spawn the pty (milestone 1 code) with `PTMUX=<socket>` in the
   environment, the way tmux sets `$TMUX`. `ptmux new` refuses to nest
   when `$PTMUX` is set.

**Server loop** (`run()`): `poll` on the listener, the pty master and every
client.

- Pty output is sent to *every* attached client as `Output`.
- Client `Input` is written to the master. `Resize` becomes `TIOCSWINSZ`.
- `Detach`, or EOF from a client, drops that client. `Kill` ends the
  session.
- When the master returns EIO (the child exited) or the session is killed,
  every client is sent `Exited`. Then the server closes the master, which
  hangs up the slave and SIGHUPs the child's session, and removes its
  socket.

**Wire format** (`src/proto.rs`): `[tag u8][len u32 BE][payload]`. A single
`read()` can return half a message or several, so each side keeps a buffer
and `decode()` takes out complete frames.

**Client** (`src/client.rs`): raw mode, then send `Hello{rows, cols}`, then
relay. `KeyFilter` handles the prefix: `C-b d` detaches, `C-b C-b` sends a
literal 0x02, and other keys after the prefix are swallowed.

**Redraw on reattach.** We have no screen model yet, so the server cannot
repaint a client that attaches. Instead, on `Hello` it sets the pty one
column narrower and then back. The kernel only sends SIGWINCH when the size
*changes*, so a single `TIOCSWINSZ` to the same size would do nothing. Each
change makes full-screen programs (top, vim, htop) redraw. dtach calls this
the `winch` redraw method. It does nothing for a plain shell prompt:
reattaching shows a blank screen until the shell prints something.

## How tmux does it

- **One server, many sessions.** `tmux new` starts a server if none is
  listening on `/tmp/tmux-$UID/default`; otherwise it asks the existing
  server to create the session. `server_start()` → `proc_fork_and_daemon()`
  (or `daemon()`).
- **imsg protocol** (`proc.c`, `tmux-protocol.h`). The header holds type,
  length, peer id and pid. The first messages identify the client
  (`MSG_IDENTIFY_*`: terminal name, cwd, environment, and the tty fd itself
  through `SCM_RIGHTS`).
- **The server draws, not the client.** Because it holds the client's tty
  fd, the server writes to your terminal directly (`tty.c`), and the client
  mostly waits. This is the biggest architectural difference from ptmux.
- **Prefix keys** are handled in the server (`server-client.c`
  `server_client_handle_key`, key tables in `key-bindings.c`). `detach-client`
  is just a bound command.
- **Reattach is a full repaint** from the server's grid
  (`server_redraw_client` / `screen-redraw.c`). No SIGWINCH trick is needed.
  That is milestone 3 and 4 for us.
- Slow clients: output is buffered per client with libevent `bufferevent`s,
  so one stuck terminal can't stall the server. ptmux uses blocking writes
  and *would* stall.

## How screen does it

- **One backend process per session.** The socket is named
  `<pid>.<tty>.<host>` (or `<pid>.<name>` with `-S`) inside the socket
  directory (`$SCREENDIR`, `/run/screens/S-$USER`, or similar).
- The socket used to be a **named pipe (FIFO)** on systems without Unix
  sockets. Its file permission bits store state: the owner-execute bit
  means "attached". That is why `screen -ls` can report Attached or Detached
  without connecting (`socket.c` `FindSocket`).
- `screen -r` (`attacher.c` `Attach()`) sends a fixed-size `struct msg` with
  `MSG_ATTACH`. The backend then opens the attacher's tty **by name** and
  sends `SIGCONT`/`SIGHUP`-style signals back to the attacher process to
  coordinate. Look for `SIG_BYE`, `SIG_POWER_BYE` and `SIG_LOCK` in screen.h.
- `C-a d` → `Detach(D_DETACH)` in screen.c.

## Experiments

```sh
# Watch our protocol: a client connecting, attaching, detaching.
tail -f $XDG_RUNTIME_DIR/ptmux/<name>.log

# See the double fork: the server's parent is 1 (or a subreaper), and it is
# not a session leader (SID != PID) and has no tty (TT is ?).
ps -o pid,ppid,sid,tty,comm -p $(pgrep -f 'ptmux new' | head -1)

# Compare with tmux's server:
ps -o pid,ppid,sid,tty,comm -p $(pgrep -x tmux | head -1)

# tmux's imsg traffic, with -vv logging:
tmux -vv new -d -s logtest; grep -h 'MSG_' tmux-*.log | head; tmux kill-server

# Is the session alive? screen encodes it in the socket's mode bits:
ls -l /run/screens/S-$USER/    # after `screen -dmS x`, then `screen -r x`
```

## Verified

The binary was driven inside a detached tmux session:

- `new -s t` → `C-b d` → `[detached (from session t)]`. The server keeps
  running, and `ls` prints `t`.
- `attach` with `top` running → top redraws in full (resize nudge).
- Resizing the outer window to 100x30 → top uses the wider layout.
- `kill -t t` → the attached client prints `[exited]`, and the socket, log
  and server process are gone.
- `new -d sleep 1` → the session disappears by itself when `sleep` exits.
  Reusing a live name gives `duplicate session`.
- Unit tests: protocol round-trip, partial frames, malformed frames, prefix
  key handling (8 tests).

## Known gaps (fixed by later milestones)

- The terminal is blank after reattaching to a plain shell, because there
  is no screen model (milestone 3).
- If you detach while a program uses the alternate screen (vim, less), your
  outer terminal stays on the alternate screen. ptmux passes raw bytes
  through, so it doesn't know the inner program switched screens.
  Milestone 3 tracks this.
- When several clients with different sizes attach, the pty takes the size
  of whichever client spoke last. tmux sizes to the smallest or largest
  client, depending on the `window-size` option.
