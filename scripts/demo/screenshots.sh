#!/usr/bin/env bash
# Screenshots (and a short GIF) of the demo (scripts/demo/demo.sh) for the
# README, taken in a real terminal by vhs (brew install vhs).
#
#   scripts/demo/screenshots.sh DEMO_DIR [OUT_DIR]   # default docs/images
set -euo pipefail
d=$(cd "${1:?DEMO_DIR}" && pwd)
root=$(cd "$(dirname "$0")/../.." && pwd)
out=${2:-$root/docs/images}
mkdir -p "$out"
command -v vhs >/dev/null || { echo "needs vhs: brew install vhs" >&2; exit 1; }
tape="$root/target/demo-shots.tape"; mkdir -p "$root/target"
trap 'rm -f "$tape"' EXIT
cat > "$tape" <<TAPE
Output "$out/tour.gif"
Set Shell "bash"
Set FontSize 15
Set Width 1600
Set Height 940
Set Padding 16
Set Theme "Builtin Tango Dark"
Set TypingSpeed 0

Hide
Type "source $d/env.sh && clear && rlord"
Enter
Sleep 3s
Escape
Sleep 1s
Show

Sleep 1s
Screenshot "$out/list.png"
Down 3
Sleep 1.5s
Screenshot "$out/waiting.png"
Tab
Sleep 1.5s
Screenshot "$out/groups.png"
Tab
Sleep 1s
Type "a"
Sleep 1.5s
Screenshot "$out/archived.png"
Type "a"
Sleep 1s
Type "?"
Sleep 1.5s
Screenshot "$out/help.png"
Escape
Sleep 1s
Up 3
Sleep 1s
# The overlord, over the list (through the launcher: vhs's Alt keys don't
# reach tmux as Meta).
Type ":"
Sleep 0.5s
Type "overlord"
Sleep 0.5s
Enter
Sleep 3s
Screenshot "$out/overlord.png"
Sleep 2s
TAPE
vhs "$tape"
ls -1 "$out"
