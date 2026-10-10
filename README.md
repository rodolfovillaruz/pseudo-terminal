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
ptmux capture [-t name]  # print the screen as text
ptmux kill [-t name]     # end a session
```

Tab completion covers subcommands, flags and live session names
(`ptmux attach -t <TAB>`). To turn it on, add one line to your shell rc:

```sh
source <(COMPLETE=bash ptmux)   # ~/.bashrc
source <(COMPLETE=zsh ptmux)    # ~/.zshrc
COMPLETE=fish ptmux | source    # ~/.config/fish/config.fish
```

Inside a session, `C-b d` detaches and `C-b C-b` sends a literal `C-b`.
Sockets and server logs live in `$XDG_RUNTIME_DIR/ptmux/`.

## Milestones

| # | Mechanism | Status | Notes |
|---|-----------|--------|-------|
| 1 | PTY passthrough (like `script`) | done | [docs/01-pty.md](docs/01-pty.md) |
| 2 | Detach / reattach over a Unix socket (like `dtach`) | done | [docs/02-detach.md](docs/02-detach.md) |
| 3 | Virtual screen: VT parser + cell grid | done | [docs/03-screen.md](docs/03-screen.md) |
| 4 | Diff-based rendering + status line | done | [docs/04-render.md](docs/04-render.md) |
| 5 | No status line: the pane is the whole terminal | done | [docs/05-no-status.md](docs/05-no-status.md) |

Start with [docs/00-overview.md](docs/00-overview.md).

## Tests

```sh
cargo test                # parser, screen, renderer simulation, protocol
experiments/parity.sh     # what tmux draws from our output vs our grid: vim/less/htop/seq
```

## Reading the real thing

Clone the upstream sources into `vendor/`, which git ignores:

```sh
git clone https://github.com/tmux/tmux vendor/tmux
git clone https://git.savannah.gnu.org/git/screen.git vendor/screen
```

Tools that help with observing the real programs: `tmux -vvv` (writes
`tmux-server-*.log` and `tmux-client-*.log` to the current directory), and
`strace -f` and `screen` (on Arch: `sudo pacman -S strace screen`).
