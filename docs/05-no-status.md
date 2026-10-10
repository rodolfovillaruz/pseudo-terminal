# 05 — No status line: the pane is the terminal

Goal: drop the green status line from milestone 4, so the pane fills the
whole client terminal. This replaces the original plan of windows and split
panes. Removing something shows what it was costing: the status line was
the only reason the pane and the terminal had different sizes.

## What ptmux does

`src/status.rs` is gone, and with it everything that existed only to
support it:

- **Pane size = terminal size.** `Session::resize` (`src/server.rs`) no
  longer subtracts a row, so a program in the pane sees exactly the
  terminal's `rows x cols` (`stty size` inside a 24-row terminal prints
  `24 80`).
- **No scroll region.** `Tty::scroll` (`src/tty.rs`) used to wrap `CSI k S`
  in `CSI 1;n r` … `CSI r`, so scrolling the pane left the status line
  where it was. With nothing below the pane, a plain `CSI k S` scrolls the
  whole screen, which is the pane. `Frame::pane_rows` is gone too: the
  pane's rows are the frame's rows.
- **No timer.** The server's `poll` had a one-second timeout while a
  client was attached, only to move the clock. It now sleeps until the pty
  or a socket has something to say. A quiet session costs no CPU at all.
- **No foreground-process lookup.** The window name (`tcgetpgrp()` on the
  master + `/proc/<pgrp>/comm`) was computed on every frame just for the
  status line.

The pane's own title (OSC 2) is still forwarded to the outer terminal, so
the window title is where you see what is running.

## How tmux does it

tmux's equivalent is `set -g status off`. The `status` option can also be a
number from 2 to 5 for a multi-line status. `status_line_size()` in
status.c returns how many rows the status takes, and the code that sizes
windows for a client (resize.c) subtracts it. With `status off` that is
zero, so the window gets the full client size, just as here.

tmux keeps sending scroll regions even then. `tty_cmd_linefeed` and
friends always set the pane's region (`tty_region_pane`), because a pane is
usually only part of the screen. When the pane is the whole screen, that
region just covers everything. ptmux has only one pane, so it can skip the
region.

## How screen does it

screen has no status line by default. The last row only shows messages
for a moment. You opt in with `hardstatus alwayslastline` or a `caption`,
and the window shrinks by one row when you do.

## Verified

- 35 unit tests (the three `status.rs` tests went with it). The renderer
  simulation now uses a pane exactly the size of the terminal, and the
  scroll test checks that scrolling output is a bare `CSI 1 S` with no
  `CSI r`.
- `experiments/parity.sh` (9 checks) now compares the *entire* tmux pane
  with ptmux's grid, since there is no status row to cut off: a coloured
  `ls`, vim with splits, less, htop, `seq 1 20000` and detach all pass.
  The server log shows the same ratio as before: `pane wrote 146167 bytes,
  client was sent 18955`.

## Known gaps

The milestone 4 gaps remain: a slow client blocks the server, and several
clients of different sizes share one pane size.
