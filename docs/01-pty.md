# 01 — PTY passthrough

Goal: run a shell on a pseudo-terminal we own, and copy bytes between it
and the real terminal. This works like `script(1)`. Every multiplexer is
built on this loop.

## What ptmux does

`src/pty.rs` `spawn()`:

1. `openpty()` gives a **master/slave pair**. The slave (`/dev/pts/N`) looks
   like a real terminal to the child. We read and write the master.
2. `fork()`. Then, in the child:
   - `setsid()` starts a new session, which detaches the child from our
     controlling tty.
   - `ioctl(slave, TIOCSCTTY)` makes the slave the session's **controlling
     terminal**. Without it, `^C` never becomes SIGINT and job control
     breaks.
   - `dup2(slave, 0/1/2)`, close the extra fds, set `TERM`, then `execvp`.
3. The parent drops its copy of the slave. Once the child exits, reading the
   master returns **EIO**, which is how we detect that the child is gone.

`src/term.rs` `RawGuard` runs `cfmakeraw` on the real terminal, so the
outer line discipline stops interpreting keys. Typing `^C` then sends a
`0x03` byte into the master, and the **inner** line discipline turns that
byte into SIGINT for the foreground job. The guard restores the saved
termios on drop.

`passthrough()` in `src/main.rs` (commit `33ac67b`; milestone 2 splits it
into the server and client loops):

- A `poll()` loop over stdin and the master copies whichever side is ready.
- On **SIGWINCH**, it reads the real size (`TIOCGWINSZ` on stdin) and writes
  it to the master (`TIOCSWINSZ`). The kernel then sends SIGWINCH to the
  inner foreground process group. The handler is installed without
  `SA_RESTART`, so `poll()` wakes with EINTR.

## How tmux does it

- `spawn.c` `spawn_pane()` calls `fdforkpty()` (`compat/forkpty-*.c` or
  libc). The child sets up its environment and `execvp`s the shell.
  It also sets `TERM` from the `default-terminal` option.
- Pane output is read through libevent `bufferevent`s (`window.c`,
  `window_pane_read_callback`). tmux has no hand-written poll loop.
- Resize: `window_pane_resize()` → `TIOCSWINSZ` on the pane fd.
- Raw mode is set by the **server** in `tty.c` `tty_start_tty()`, on the
  client's terminal fd that was passed over the socket. Our ptmux client
  will keep that job, because it is simpler.

## How screen does it

- `pty.c` `OpenPTY()` has many `#ifdef` variants for old Unixes, which is
  instructive in itself.
- `window.c` `ForkWindow()` forks, runs `setsid`, opens the slave, sets
  `TIOCSCTTY`, sets up environment variables such as `STY`/`WINDOW`, then
  `execvp`.
- Raw mode is in `tty.sh`, which generates `tty.c` (`SetTTY`, `GetTTY`).

## Experiments

```sh
# Which pty does each layer get? Run inside `cargo run -- new`:
tty; ps -o pid,sid,tty,stat,comm -t $(tty | sed 's|/dev/||')

# Watch tmux's syscalls while it spawns a pane (needs strace):
strace -f -e trace=openat,ioctl,setsid,execve -o /tmp/tmux.trace tmux new -d
grep -E 'ptmx|TIOCSCTTY|TIOCSWINSZ|setsid|execve' /tmp/tmux.trace

# Raw bytes tmux writes to the outer terminal:
script -q -c 'tmux new "sleep 1"' /tmp/tmux.bytes; cat -v /tmp/tmux.bytes | head
```

## Verified

The binary was driven inside a detached tmux session:

- The shell runs on its own `/dev/pts/N`, with `TERM=xterm-256color`.
- `stty size` follows a resize from 24x80 to 30x100.
- `^C` interrupts the inner `sleep 100` and ptmux keeps running.
- `exit` returns 0, and the outer terminal gets `icanon echo` back.
