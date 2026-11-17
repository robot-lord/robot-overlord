#!/bin/sh
# Runs inside a bare distro container for `cargo xtask smoke-linux`
# (xtask/src/smoke.rs, which compiles it in), mounted with the release
# files at /dist and get.sh at /get.sh. POSIX sh: the image may have nothing else. Prints OK last
# when everything worked.
set -e
# Minimal images may lack what get.sh itself needs.
if ! command -v tar >/dev/null || ! command -v gzip >/dev/null; then
  (dnf install -y tar gzip || yum install -y tar gzip) >/dev/null 2>&1 || true
fi
if command -v pacman >/dev/null; then pacman -Sy --noconfirm >/dev/null 2>&1; fi
sh /get.sh
export PATH="$HOME/.local/bin:$PATH"
rlord --version
tmux -V
export RLORD_TMUX_SOCKET=smoke RLORD_NO_LOGIN_SHELL=1
tmux -L smoke new-session -d -s keep "sleep 600"
rlord ls          # starts the daemon on demand
rlord ping
rlord search nothing-here || true
# The web plugin shipped beside it: this rlord's compiled-in hash takes it,
# and it starts (on 127.0.0.1, inside the container).
rlord web install --yes --from /dist
# (Through files: `grep -q` stopping early would break rlord's pipe.)
rlord web status >/tmp/web-status && cat /tmp/web-status
grep -q '(verified)' /tmp/web-status
rlord web url >/tmp/web-url
grep -q '^http://127.0.0.1:' /tmp/web-url
rlord web stop
echo OK
