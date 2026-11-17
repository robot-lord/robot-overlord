#!/bin/sh
# A fresh install of rlord, end to end, in a throwaway container: run by
# `cargo xtask fresh-install` (xtask/src/fresh_install.rs), which judges
# the output. Only glue: each step prints
#
#   @@step NAME
#   <what the commands printed>
#   @@rc <exit status>
#
# and xtask decides what passed. Runs as root first (`sh scenario.sh`:
# sudo, a user), then as that user (`sh scenario.sh user`).
#
# Environment: E2E_INSTALL tarball|deb|rpm|stable (how rlord is
# installed), E2E_NETWORK 1|0 (may reach GitHub). Mounts: /dist (release
# files and SHA256SUMS), /e2e (this script, get.sh, the stand-in agent).
set -u
user=alice

step() { # NAME CMD…: run CMD, framed
  name=$1
  shift
  echo "@@step $name"
  "$@" 2>&1
  echo "@@rc $?"
}

if [ "${1:-}" != user ]; then
  # What a fresh machine already has: sudo, curl, a user who can sudo.
  # (Not tmux: get.sh and the packages bring it.)
  prep() {
    if command -v apt-get >/dev/null; then
      apt-get update -qq && apt-get install -y -qq sudo curl ca-certificates >/dev/null
    elif command -v dnf >/dev/null; then
      # (util-linux: su; Fedora's image has only util-linux-core.)
      dnf install -y -q sudo util-linux tar gzip >/dev/null
    else
      echo "no apt-get or dnf"
      return 1
    fi
    useradd -m -s /bin/bash "$user" &&
      echo "$user ALL=(ALL) NOPASSWD:ALL" >"/etc/sudoers.d/$user"
  }
  step prep prep || exit 1
  exec su - "$user" -c "E2E_INSTALL=$E2E_INSTALL E2E_NETWORK=$E2E_NETWORK sh /e2e/fresh_install.sh user"
fi

# ---- as the user, from a login shell ----
cd "$HOME" || exit 1
arch_deb=$(dpkg --print-architecture 2>/dev/null || true)
arch_rpm=$(uname -m)

install_local() {
  case $E2E_INSTALL in
    tarball | stable) RLORD_DIST=/dist RLORD_YES=1 sh /e2e/get.sh ;;
    deb) sudo apt-get install -y -q "$(ls /dist/rlord_*_"$arch_deb".deb)" ;;
    rpm) sudo dnf install -y -q "$(ls /dist/rlord-*."$arch_rpm".rpm)" ;;
  esac
}

if [ "$E2E_INSTALL" = stable ]; then
  # The newest stable release from GitHub, as the README's one-liner does;
  # then the build under test over it, with the old daemon still running.
  step stable-install env RLORD_YES=1 sh /e2e/get.sh
  step stable-version "$HOME/.local/bin/rlord" --version
  step stable-ls "$HOME/.local/bin/rlord" ls
  step stable-update "$HOME/.local/bin/rlord" update
fi
step install install_local
# A new login shell finds it (get.sh's ~/.local/bin is on the distro's
# default PATH once the folder exists).
step login-path bash -lc 'command -v rlord'
PATH=$HOME/.local/bin:$PATH
export PATH
step version rlord --version
step tmux-version tmux -V
if [ "$E2E_INSTALL" = stable ]; then
  # The old daemon is still up: the first command moves it to this build.
  step upgrade-ls rlord ls
else
  step ping-cold rlord ping
  step ls-first rlord ls
fi
step ping rlord ping
# The web plugin from the same release files: the installed rlord's
# compiled-in hash must take it, and it must start.
step web-install rlord web install --yes --from /dist
step web-status rlord web status
step web-url rlord web url
step web-stop rlord web stop

# First run with no agent: the list opens on the overlord's setup dialog.
tui() {
  # (The pane stays when rlord exits, so its last words can be read.)
  tmux -L e2e new-session -d -x 120 -y 40 'rlord; echo "rlord exited: $?"; sleep 600' ||
    return 1
  i=0
  while [ $i -lt 40 ]; do
    tmux -L e2e capture-pane -p >/tmp/screen.txt
    grep -q 'Start your overlord\|rlord exited' /tmp/screen.txt && break
    sleep 0.5
    i=$((i + 1))
  done
  cat /tmp/screen.txt
  # Leave it: Esc closes the dialog.
  tmux -L e2e send-keys Escape
}
step tui-first-run tui
# Then the code-folder form (no projects seen yet); Esc puts it off.
setup_form() {
  i=0
  while [ $i -lt 20 ]; do
    tmux -L e2e capture-pane -p >/tmp/screen.txt
    # (It opens at once, then fills in rlord's suggestion.)
    grep -q 'no projects yet\|rlord exited' /tmp/screen.txt && break
    sleep 0.5
    i=$((i + 1))
  done
  cat /tmp/screen.txt
  tmux -L e2e send-keys Escape
}
step tui-setup-form setup_form
step agents-none-json rlord agents --json
step agents-none rlord agents
step overlord-status-none rlord overlord status
mkdir -p "$HOME/proj"
step new-none rlord new --project "$HOME/proj" --agent claude --name e2e-none
step setup rlord setup "$HOME/proj"
step setup-config rlord config show

# Stand-ins for the three agents' CLIs, where a global install would put
# them: they answer --version, log every call, and otherwise idle. No real
# agent, no model call.
standins() {
  for a in claude codex opencode; do
    sudo install -m 755 /e2e/standin.sh "/usr/local/bin/$a" || return 1
  done
  : >/tmp/standin-calls.log
}
step standins standins
step agents-standins-json rlord agents --json
step install-hooks rlord install-hooks
step agents-hooks-json rlord agents --json
for a in claude codex opencode; do
  step "new-$a" rlord new --project "$HOME/proj" --agent "$a" --name "e2e-$a"
done
sleep 2
step sessions-json rlord ls --json
# rlord's own tmux server, inside the container only.
step rlord-socket tmux -L rlord ls
for a in claude codex opencode; do
  step "stop-$a" rlord stop "e2e-$a"
done
step reset rlord reset
step ping-after-reset rlord ping
if [ "$E2E_NETWORK" = 1 ]; then
  step update rlord update
fi
step standin-calls cat /tmp/standin-calls.log

case $E2E_INSTALL in
  deb) step uninstall sudo apt-get remove -y -q rlord ;;
  rpm) step uninstall sudo dnf remove -y -q rlord ;;
esac
case $E2E_INSTALL in
  deb | rpm) step gone sh -c 'hash -r; command -v rlord' ;;
esac
echo "@@done"
