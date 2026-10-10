# 04 — Rendering: the server draws every client

Goal: pane output never reaches a client terminal as is. The server keeps,
for each client, a model of what that terminal shows, and sends only the
bytes that turn it into the next frame. This is the step that makes the
multiplexer own the terminal. Everything else (status line, splits, copy
mode, several clients) depends on it.

## What ptmux does

**The loop** (`src/server.rs`):

1. Read the pty, up to 64 KiB at a time (`Session::read_pane`), and feed it
   to the emulator. Batching means a fast producer like `seq 1 20000` is
   drawn once per batch, not once per `read()`.
2. Write any query replies the emulator produced (DSR cursor position, DA)
   back into the pty. The real terminal never sees the query, so we must
   answer it, as tmux does.
3. For each attached client: `Frame::compose` builds the frame (pane plus
   status line), and `Tty::draw` produces the difference from what that
   client's terminal shows.
4. While anyone is attached, `poll` wakes every second so the clock moves.

**`Tty`** (`src/tty.rs`) is our model of one client's terminal:

- `start()`: alternate screen (`?1049h`), autowrap **off** (`?7l`),
  clear. With autowrap off, writing the bottom-right cell can't scroll the
  screen, and every run of cells is positioned with CUP anyway. tmux does
  the same (`tty_start_tty` sends `smcup` and `rmam`).
- `draw(frame)`:
  - **Scroll detection.** If shifting the pane up by k rows makes more rows
    match than not shifting, it sets a scroll region over the pane (so the
    status line stays put), then sends `CSI k S` and resets the region.
    The diff then fixes whatever is left. A new line of output then costs
    roughly 15 bytes of scroll plus one short row.
  - **Per row:** find the first and last cells that differ, widen to whole
    wide characters, then CUP and write that run with SGR changes only where
    the pen changes. A trailing run of blanks becomes `EL` (`CSI K`).
  - The cursor is hidden while drawing (`?25l` … `?25h`), so it doesn't
    flicker across the screen.
  - Mode changes (mouse, bracketed paste, application keys), the keypad
    mode and the title are diffed too, and only changes are sent.
- `stop()`: turn those modes off, show the cursor, turn autowrap back on,
  and leave the alternate screen. The user's shell and its scrollback are
  exactly as they were before attaching.
- `resize()`: the terminal's contents are unknown after a resize, so the
  next draw clears and repaints everything.

**Status line** (`src/status.rs`), tmux's default layout and colours:

```
[work] 0:vim*                              "OMEN" 17:05 10-Oct-26
```

The window name is the **foreground process of the pty**, found the way
tmux does it: `tcgetpgrp()` on the *master* fd gives the foreground
process group of the slave side, and `/proc/<pgrp>/comm` gives its name.
The title is the pane's OSC 2 title, trimmed to 21 columns (tmux's
`#{=21:pane_title}`), or the hostname if the pane never set one.

## How tmux does it

tmux's renderer is **command-driven, not diff-driven**:

- Every editing operation in `screen-write.c` (insert line, clear to end of
  line, write a cell, ...) both updates the grid *and* builds a `tty_ctx`
  that is replayed on each client by a matching `tty_cmd_*` function in
  `tty.c`: `tty_cmd_insertline`, `tty_cmd_clearendofline`, `tty_cmd_cell`,
  `tty_cmd_linefeed`, ... So when a program scrolls, tmux sends a scroll
  because it *saw* one, not because it found one afterwards as we do.
- To save work, `screen-write.c` collects runs of cells into a pending
  "collect" list and flushes them in batches (`screen_write_collect_flush`).
  It also joins consecutive linefeeds into a single scroll.
- When a command can't be replayed (the client is smaller than the pane,
  overlapping panes, a client that just attached), tmux marks the client for
  a full redraw: `CLIENT_REDRAW*` flags → `screen_redraw_screen` in
  screen-redraw.c.
- Terminal capabilities come from terminfo (`tty-term.c`, `tty-features.c`).
  tmux only uses `CSI S`, scroll regions, and so on if the outer terminal
  claims them. ptmux assumes an xterm-compatible terminal.
- Slow clients: output goes into a per-client libevent buffer, and when a
  client falls too far behind tmux discards the backlog and redraws
  (`tty_block_maybe`). ptmux's blocking writes would stall the whole server.

The trade-off: commands are cheaper per update and keep the program's
intent (a real insert line, for example). A diff is simpler, can't get out
of sync with the grid, and copes with any change, because it only compares
states.

## How screen does it

screen sits between the two. Like tmux, ansi.c updates the window's lines
and, if the window is visible, calls display functions directly
(`PutChar`, `ScrollV`, `InsertMode`...) on each display (display.c). A
full redraw is `Activate()` → `RefreshArea()`. screen's own status line
is the `hardstatus`/`caption` lines, rendered from `%`-escape strings
(`MakeWinMsg` in screen.c).

## Measurements

From the server log during `experiments/parity.sh` (`seq 1 20000` plus
vim, less and htop in a 90x28 terminal):

```
client detached: pane wrote 145646 bytes, client was sent 18768
```

The pane produced 7.8× more output than was sent. Most of the `seq` output
scrolled past between frames and never had to be drawn.

## Verified

- 38 unit tests. The main one: feed chunks to a pane, draw a frame after
  each, and run *only the draw output* through a second `Screen` standing
  in for the real terminal. Its grid, cursor, cursor visibility and modes
  must equal the frame after every chunk. This covers colours, wide CJK,
  wrapping, the alternate screen, mouse and paste modes, and line drawing.
  Another test checks that scrolling output uses `CSI S`, with less than
  half the bytes of a redraw.
- `experiments/parity.sh` (9 checks): the tmux pane, fed only by our
  renderer, matches our grid for a coloured `ls`, vim with splits, less,
  htop and `seq 1 20000`. The status line names the foreground process
  (`0:vim*`, `0:less*`, `0:htop*`). After a detach, the outer terminal is
  back on its main screen with its own history and a `[detached]` line.
- The milestone 3 drift is fixed: reattach while vim runs, then quit vim,
  and the pane's shell history is there.

Writing the scroll test found a bug: the first version of scroll detection
required *every* shifted row to match. But the new line of output lands
inside the shifted region, so it never matched, and every frame fell back
to rewriting each changed row.

## Known gaps

- One window and one pane (milestone 5).
- A slow or stopped client blocks the server.
- A client bigger than the pane shows blank space. A smaller one is
  clipped. tmux draws the unused area with dots and offers `window-size`.
- Mouse coordinates are passed through unchanged, which works only because
  the pane is at the top-left.
