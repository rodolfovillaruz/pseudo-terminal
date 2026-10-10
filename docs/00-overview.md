# 00 — Architecture overview: tmux vs screen

Every terminal multiplexer comes down to three jobs:

1. **Own the ptys.** Programs run on the slave side of pseudo-terminals that
   the multiplexer holds open. The real terminal never talks to them directly.
2. **Survive the terminal.** The ptys live in a long-running process that
   does not die when you close the terminal window, which is what makes
   detach possible.
3. **Emulate a terminal.** Pty output is parsed into an in-memory screen, so
   it can be redrawn on reattach, split into panes, scrolled back, and so on.

## tmux

- There is **one server process** per socket (`/tmp/tmux-$UID/default`). It
  holds every session, window and pane.
- Each `tmux` command you run is a **client**. It connects to the server's
  Unix socket and speaks the `imsg` protocol (from OpenBSD; see `proc.c` and
  the `MSG_*` types in `tmux-protocol.h`).
- When you attach, the client sends its terminal file descriptors to the
  server over the socket (`SCM_RIGHTS`). The **server** then writes to your
  terminal directly (`tty.c`). The client mostly just relays signals and
  waits.
- Terminal emulation happens in `input.c` (parser), `grid.c` (cell storage)
  and `screen-write.c` (editing operations). Output to the real terminal goes
  through `tty.c` and `tty-term.c` (terminfo).

## GNU screen

- There is **one backend process per session**, the `SCREEN` process. Its
  socket lives in a per-user directory such as `/run/screens/S-$USER/` or
  `~/.screen`, depending on how it was built.
- `screen -r` starts an **attacher** that finds that socket and connects
  (`attacher.c` and `socket.c`). The backend then takes over the attacher's
  tty, much as tmux does.
- Terminal emulation happens in `ansi.c`. Output to the real terminal goes
  through `display.c` and `termcap.c`. Pty handling is in `pty.c`, and
  window/process spawning is in `window.c`.
- Older screen ran setuid root for multiuser mode. That history explains a
  lot of the code's complexity.

## Mapping to ptmux

| Concern | tmux | screen | ptmux |
|---|---|---|---|
| spawn on pty | `spawn.c` | `window.c`, `pty.c` | `src/pty.rs` |
| raw mode on real tty | `tty.c` | `tty.sh` (generates `tty.c`) | `src/term.rs` |
| client/server | `client.c`, `server.c`, `proc.c` | `attacher.c`, `socket.c` | milestone 2 |
| VT parsing | `input.c` | `ansi.c` | milestone 3 |
| screen model | `grid.c`, `screen.c` | `ansi.c`, `mline` structs | milestone 3 |
| drawing | `tty.c`, `screen-redraw.c` | `display.c` | milestone 4 |
| splits | `layout.c` | regions in `display.c`/`canvas.c` | milestone 5 |

File names come from recent upstream trees. Check them against `vendor/`,
because code moves around between versions.
