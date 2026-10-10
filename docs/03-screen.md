# 03 — The virtual screen

Goal: the server always knows what the pane looks like, whether or not
anyone is attached. This is what separates tmux and screen from dtach. A
reattach is repainted from that copy instead of hoping the program redraws.

## What ptmux does

**Parser** (`src/vt/parser.rs`) is a byte-at-a-time state machine after
[Paul Williams' VT500 diagram](https://vt100.net/emu/dec_ansi_parser).
It calls five actions on a `Perform` implementation:

| action | example input | meaning |
|---|---|---|
| `print` | `a`, `中` (UTF-8 decoded) | put a glyph at the cursor |
| `execute` | `\r`, `\n`, `\b`, `\t`, SO/SI | C0 control |
| `csi_dispatch` | `ESC [ 1 ; 31 m`, `ESC [ ? 1049 h` | params, private marker / intermediates, final byte |
| `esc_dispatch` | `ESC 7`, `ESC ( 0` | intermediates, final byte |
| `osc_dispatch` | `ESC ] 2 ; title BEL` | `;`-split string |

Simplifications compared with a real VT500: DCS/SOS/PM/APC strings are
skipped, 8-bit C1 controls are not recognised (bytes ≥ 0x80 are UTF-8),
and `:` sub-parameters are kept per parameter so `38:2::r:g:b` works.
Sequences can be split across `read()`s, because all state lives in the
parser.

**Screen** (`src/vt/screen.rs`) implements `Perform`:

- Two cell grids, primary and alternate. `?1049h` saves the cursor,
  switches buffers and clears; `?1049l` switches back and restores.
- `Cell { ch, pen, width }`. A wide character takes two cells, and the
  right one has width 0. Overwriting either half blanks the other.
- **Deferred wrap.** Printing in the last column sets `wrap_pending`
  instead of moving to the next line. The wrap happens only when the next
  character arrives. Without this, every full-width line followed by
  `\r\n` would leave an empty line behind. There is no visible state for
  it, which makes it the classic emulator bug.
- Scroll region (DECSTBM), origin mode, insert/delete lines and chars,
  erase with the current background (`bce`), tab stops, DEC line drawing
  through G0/G1 + SO/SI, SGR including 256-colour and truecolour,
  DECSC/DECRC, title (OSC 0/2).
- Modes the screen doesn't act on, but that change what the terminal
  *sends*, are recorded so they can be replayed: application cursor keys
  (`?1`), mouse modes (`?1000`… `?1015`), focus events (`?1004`) and
  bracketed paste (`?2004`).
- Resize keeps the cursor line visible by dropping lines from the top
  (like xterm). Text is not reflowed.

**Render** (`src/render.rs`):

- `repaint(screen)` brings a blank terminal into the same state: enter the
  alternate screen if needed, clear, draw each row with minimal SGR changes,
  then restore margins, origin mode, cursor, pen, charsets and modes. For
  pending wrap it rewrites the last cell, because CUP can't express that
  state.
- `reset(screen)` undoes the pane's modes: leave the alternate screen, turn
  off mouse reporting and bracketed paste, and so on. This is why your
  terminal is no longer stuck in vim's screen after a detach.

**Server** (`src/server.rs`): every byte from the pty master goes through
`parser.advance(&mut screen, …)` before it is sent to clients. `Hello` →
resize the screen and pty, then send `repaint`. `Detach` → send `reset`,
then hang up. The client reads until EOF, so the reset always arrives.
`Capture` → send `screen.text()` (`ptmux capture`).

## How tmux does it

- **input.c** is the parser. It uses the same Williams states
  (`input_state_ground`, `input_state_csi_enter`, …), each a table of byte
  ranges → handlers. CSI finals are looked up in `input_csi_table` and map to
  `screen_write_*` calls.
- **screen-write.c** holds the editing operations: `screen_write_cell`,
  `screen_write_linefeed`, `screen_write_clearline`, … These also work out
  what must be redrawn and queue `tty_cmd_*` calls (milestone 4).
- **grid.c** stores lines (`grid_line`) with a compact cell encoding:
  `grid_cell_entry` is 1 byte of data plus flags, and only cells that need
  more go to an "extended" array. Lines that scroll off the top go into
  history (scrollback), which we don't keep yet.
- **screen.c** holds the cursor, modes (`MODE_*` flags) and title, and
  `screen_alternate_on/off` for `?1049`.
- tmux **answers queries itself** (`input_csi_dispatch` → DA, DSR) and sets
  `TERM=tmux-256color` or `screen-256color`, a terminal it fully controls.
  ptmux still lets the real terminal answer, because output is passed
  through unchanged.

## How screen does it

- **ansi.c** `WriteString()` is the parser and `DoESC`/`DoCSI` are the
  switches. The window's grid is `w_mlines` (`struct mline`), with separate
  arrays for image, attributes, font and colour.
- Alternate screen: `EnterAltScreen`/`LeaveAltScreen`. Optional (`altscreen`
  option, off by default in old versions!), which is why screen used to
  leave vim's text on your terminal after you quit.
- screen sets `TERM=screen`. Its termcap entry is the "terminal" that
  programs inside it talk to.

## Reverse-engineering findings

Found while diffing tmux's `capture-pane` against ours
(`experiments/parity.sh`):

- **tmux's capture keeps tabs.** Lines printed with `\t` come back with
  literal tab characters, because tmux 3.4+ records tabs in the grid. We
  store spaces, and the parity script runs tmux's output through `expand`.
- **tmux stores line drawing as ASCII plus a charset flag.** `ESC(0 lqqk`
  is captured as `lqqk` (rendered as `┌──┐`). We translate to Unicode
  when printing, so the parity script maps our `┌──┐` back.

## Experiments

```sh
cargo build && experiments/parity.sh   # vim, less, htop: tmux vs ptmux grids

# What does a program actually send? Record it and look:
script -q -c 'vim -u NONE -N +q' /tmp/vim.raw; cat -v /tmp/vim.raw | head

# Compare tmux's parser on the same bytes:
tmux -vvv new -d 'cat /tmp/vim.raw; sleep 1'; grep -h 'input_' tmux-server-*.log | head
```

## Verified

- 31 unit tests. They cover parser states, screen operations, and a
  **round-trip property**: for each case, `repaint` of a screen, fed into
  a blank screen, must reproduce identical cells, cursor and modes. This
  caught a real bug: the screen set `wrap_pending` with autowrap off.
- `experiments/parity.sh`: ptmux's grid equals what tmux drew, character
  for character, for a coloured `ls` with wide CJK and line drawing, vim
  with three splits plus an edit, the shell after vim exits, less with a
  search, and htop.
- Reattach, driven inside tmux: a plain shell's screen comes back after
  detaching and clearing the outer terminal (blank in milestone 2).
  Detaching from vim returns the outer terminal to its main screen
  (`#{alternate_on}` = 0), and reattaching re-enters the alternate screen
  and repaints vim.

## Known gaps

- **Live output is still passed through unchanged**, so the outer terminal
  and our grid can drift apart. Example: reattach to vim, then quit. vim's
  `?1049l` sends the *outer* terminal back to its own main screen, which
  doesn't have the pane's main-screen content. Milestone 4 fixes this by
  drawing clients only from the grid.
- No scrollback, no reflow on resize, combining characters are dropped,
  and DECSC state isn't replayed on reattach.
- Queries (DA, DSR) are answered by whichever real terminal happens to be
  attached, or not at all while detached.
