#!/usr/bin/env bash
# Runs programs inside ptmux inside a tmux pane, and diffs what tmux drew
# (a real terminal emulator) against ptmux's own screen model.
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
    # tmux capture keeps tabs and prints DEC line drawing as ASCII; normalize.
    tmux capture-pane -p -t $S | expand | sed 's/ *$//' > "$OUT/$1.tmux"
    $P capture -t $S | sed 's/┌──┐/lqqk/' > "$OUT/$1.ptmux"
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
check shell "box:lqqk"

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

tmux kill-session -t $S
$P kill -t $S 2>/dev/null
echo "failures: $fails (captures in $OUT)"
exit $((fails > 0))
