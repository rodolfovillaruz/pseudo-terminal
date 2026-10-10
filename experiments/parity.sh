#!/usr/bin/env bash
# Runs programs inside ptmux inside a tmux pane, and diffs what tmux drew
# (a real terminal emulator, fed only by ptmux's renderer) against
# ptmux's own screen model. There is no status line: the whole tmux pane
# must equal the ptmux pane.
# Usage: cargo build && experiments/parity.sh   (needs tmux, vim, less, htop)
set -u
R=$(cd "$(dirname "$0")/.." && pwd)
P=$R/target/debug/ptmux
S=ptmux-parity-$$
OUT=${OUT:-$(mktemp -d)}
mkdir -p "$OUT"
fails=0

tmux kill-session -t $S 2>/dev/null
$P kill -t $S 2>/dev/null
tmux new-session -d -s $S -x 90 -y 28 -e PTMUX= 'env -u PTMUX bash --norc --noprofile'
tmux send-keys -t $S "PS1='outer\$ '; $P new -s $S" Enter
sleep 1

keys() { tmux send-keys -t $S "$@"; }
check() { # name expected-text
    sleep "${WAIT:-1}"
    # tmux capture keeps tabs; normalize.
    tmux capture-pane -p -t $S | expand | sed 's/ *$//' > "$OUT/$1.tmux"
    $P capture -t $S > "$OUT/$1.ptmux"
    if ! grep -qF -- "$2" "$OUT/$1.tmux"; then
        echo "BAD TEST $1: '$2' not on screen"; fails=$((fails + 1)); return
    fi
    if diff -u "$OUT/$1.tmux" "$OUT/$1.ptmux" > "$OUT/$1.diff"; then
        echo "PASS $1"
    else
        echo "FAIL $1"; head -20 "$OUT/$1.diff"; fails=$((fails + 1))
    fi
}

keys "PS1='inner\$ '; clear; ls --color=always /usr/bin | head -40 | column -c 90; printf '\\e[1;31mred\\e[0m wide:中文 box:\\e(0lqqk\\e(B\\n'" Enter
check shell "box:┌──┐"

keys "vim -u NONE -N +'set nu' +'split' +'vsplit' $R/src/vt/screen.rs" Enter
check vim "The virtual screen"
keys "50Gdd" ; check vim-edit "screen.rs"
keys Escape ":qa!" Enter
check after-vim "inner$ "

keys "less +200 $R/src/vt/parser.rs" Enter
check less "parser.rs"
keys "/Perform" Enter; check less-search "Perform"
keys q

keys "htop -d 300" Enter
WAIT=2 check htop "Load average"
keys q

# Fast output: everything must still land, and scrolling keeps it cheap.
keys "clear; seq 1 20000; echo seq-done" Enter
WAIT=3 check seq "seq-done"

# Detach: the outer shell comes back with its own screen, not the pane's.
keys C-b d; sleep 0.5
tmux capture-pane -p -t $S > "$OUT/detach.full"
if grep -qF "[detached (from session $S)]" "$OUT/detach.full" && grep -qF "PS1='outer" "$OUT/detach.full" \
    && [ "$(tmux display -p -t $S '#{alternate_on}')" = 0 ]; then
    echo "PASS detach"
else
    echo "FAIL detach"; cat "$OUT/detach.full"; fails=$((fails + 1))
fi
grep -h 'client detached' "$XDG_RUNTIME_DIR/ptmux/$S.log"

tmux kill-session -t $S
$P kill -t $S 2>/dev/null
echo "failures: $fails (captures in $OUT)"
exit $((fails > 0))
