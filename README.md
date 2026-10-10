# ptmux

A tiny tmux/screen clone in Rust, built to learn how terminal multiplexers
work. Each milestone rebuilds one mechanism that tmux and GNU screen
share. A note in `docs/` goes with each milestone and compares the code here
with the real implementations.

## Build & run

```sh
cargo build
alias ptmux=$PWD/target/debug/ptmux

ptmux new                # new session running $SHELL, attached
ptmux new -s work        # named session
ptmux new -d top         # detached session running a command
ptmux ls                 # list sessions
ptmux attach [-t name]   # reattach
ptmux kill [-t name]     # end a session
```

Inside a session, `C-b d` detaches and `C-b C-b` sends a literal `C-b`.
Sockets and server logs live in `$XDG_RUNTIME_DIR/ptmux/`.

## Milestones

| # | Mechanism | Status | Notes |
|---|-----------|--------|-------|
| 1 | PTY passthrough (like `script`) | done | [docs/01-pty.md](docs/01-pty.md) |
| 2 | Detach / reattach over a Unix socket (like `dtach`) | done | [docs/02-detach.md](docs/02-detach.md) |
| 3 | Virtual screen: VT parser + cell grid | todo | |
| 4 | Diff-based rendering + status line | todo | |
| 5 | Windows and split panes | todo | |

Start with [docs/00-overview.md](docs/00-overview.md).

## Reading the real thing

Clone the upstream sources into `vendor/`, which git ignores:

```sh
git clone https://github.com/tmux/tmux vendor/tmux
git clone https://git.savannah.gnu.org/git/screen.git vendor/screen
```

Tools that help with observing the real programs: `tmux -vvv` (writes
`tmux-server-*.log` and `tmux-client-*.log` to the current directory), and
`strace -f` and `screen` (on Arch: `sudo pacman -S strace screen`).
