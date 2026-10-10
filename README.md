# ptmux

A tiny tmux/screen clone in Rust, built to learn how terminal multiplexers
work. Each milestone rebuilds one mechanism that tmux and GNU screen
share. A note in `docs/` goes with each milestone and compares the code here
with the real implementations.

## Build & run

```sh
cargo build
cargo run -- new                 # runs $SHELL inside a new pty
cargo run -- new -c /bin/bash    # or any program
```

## Milestones

| # | Mechanism | Status | Notes |
|---|-----------|--------|-------|
| 1 | PTY passthrough (like `script`) | done | [docs/01-pty.md](docs/01-pty.md) |
| 2 | Detach / reattach over a Unix socket (like `dtach`) | todo | |
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
