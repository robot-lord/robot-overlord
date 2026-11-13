#!/bin/sh
# Install rlord: the newest stable release for this machine (Apple Silicon
# macOS, or Linux x86_64/aarch64), checksum-verified, into ~/.local/bin.
#
#   curl -fsSL https://raw.githubusercontent.com/robot-lord/robot-overlord/main/scripts/get.sh | sh
#
# rlord needs tmux. If it's missing, this offers to install it with the
# system's package manager (asking first; that usually needs sudo).
#
# Environment:
#   RLORD_VERSION=v0.18.0   a specific release instead of the newest
#   RLORD_INSTALL_DIR=dir   where to put rlord (default ~/.local/bin)
#   RLORD_YES=1             install tmux without asking
#   RLORD_DIST=dir          install from local release files (testing)
set -eu
repo=robot-lord/robot-overlord
dir=${RLORD_INSTALL_DIR:-$HOME/.local/bin}

say() { printf '%s\n' "$*"; }
die() { printf 'rlord install: %s\n' "$*" >&2; exit 1; }
has() { command -v "$1" >/dev/null 2>&1; }

case "$(uname -s) $(uname -m)" in
  "Darwin arm64") target=aarch64-apple-darwin ;;
  "Darwin x86_64") die "Intel Macs aren't supported (Apple Silicon only); build from source: cargo install robot-overlord" ;;
  "Linux x86_64" | "Linux amd64") target=x86_64-unknown-linux-musl ;;
  "Linux aarch64" | "Linux arm64") target=aarch64-unknown-linux-musl ;;
  *) die "no prebuilt rlord for $(uname -s) $(uname -m); build from source: cargo install robot-overlord" ;;
esac

fetch() { # url dest
  if has curl; then curl -fsSL -o "$2" "$1"
  elif has wget; then wget -qO "$2" "$1"
  else die "needs curl or wget"; fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

if [ -n "${RLORD_DIST:-}" ]; then
  tag=$(ls "$RLORD_DIST" | sed -n "s/^rlord-\(v.*\)-$target\.tar\.gz$/\1/p" | head -n1)
  [ -n "$tag" ] || die "no $target tarball in $RLORD_DIST"
  cp "$RLORD_DIST/rlord-$tag-$target.tar.gz" "$RLORD_DIST/SHA256SUMS" "$tmp/"
else
  tag=${RLORD_VERSION:-}
  if [ -z "$tag" ]; then
    fetch "https://api.github.com/repos/$repo/releases/latest" "$tmp/latest.json" ||
      die "couldn't reach GitHub"
    tag=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$tmp/latest.json" | head -n1)
    [ -n "$tag" ] || die "couldn't find the latest release"
  fi
  base="https://github.com/$repo/releases/download/$tag"
  fetch "$base/rlord-$tag-$target.tar.gz" "$tmp/rlord-$tag-$target.tar.gz" ||
    die "no $target build in $tag"
  fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "no checksums in $tag"
fi

asset="rlord-$tag-$target.tar.gz"
want=$(awk -v a="$asset" '$2 == a {print $1}' "$tmp/SHA256SUMS")
if has sha256sum; then got=$(sha256sum "$tmp/$asset" | awk '{print $1}')
else got=$(shasum -a 256 "$tmp/$asset" | awk '{print $1}'); fi
[ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $asset"

tar -xzf "$tmp/$asset" -C "$tmp"
src="$tmp/rlord-$tag-$target/rlord"
mkdir -p "$dir"
if [ "$(uname -s)" = Darwin ]; then
  xattr -d com.apple.quarantine "$src" 2>/dev/null || true
fi
# Copy then rename: overwriting a signed binary in place makes macOS kill
# it (the kernel caches signatures per inode); harmless elsewhere.
cp "$src" "$dir/.rlord.new"
chmod 755 "$dir/.rlord.new"
mv -f "$dir/.rlord.new" "$dir/rlord"
say "installed $("$dir/rlord" --version) to $dir/rlord"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) say "note: $dir isn't on your PATH; add it in your shell's rc file:"
     say "  export PATH=\"$dir:\$PATH\"" ;;
esac

# tmux: rlord runs its own private tmux server, so it needs the binary.
tmux_ok() {
  has tmux || return 1
  v=$(tmux -V 2>/dev/null | sed -n 's/^tmux \([0-9][0-9]*\)\.\([0-9][0-9]*\).*/\1 \2/p')
  [ -n "$v" ] || return 0 # e.g. "tmux next-3.5": assume new enough
  # shellcheck disable=SC2086 # split "3 2" into $1 $2
  set -- $v
  [ "$1" -gt 3 ] || { [ "$1" -eq 3 ] && [ "$2" -ge 2 ]; }
}
if ! tmux_ok; then
  if has tmux; then say "rlord needs tmux 3.2 or newer (you have $(tmux -V))."
  else say "rlord needs tmux, which isn't installed."; fi
  sudo=
  [ "$(id -u)" -eq 0 ] || sudo=sudo
  cmd=
  if has brew; then cmd="brew install tmux"
  elif has apt-get; then cmd="$sudo apt-get update -qq && $sudo apt-get install -y tmux"
  elif has dnf; then cmd="$sudo dnf install -y tmux"
  elif has yum; then cmd="$sudo yum install -y tmux"
  elif has pacman; then cmd="$sudo pacman -S --noconfirm tmux"
  elif has zypper; then cmd="$sudo zypper install -y tmux"
  elif has apk; then cmd="$sudo apk add tmux"
  fi
  if [ -z "$cmd" ]; then
    say "Install tmux with your package manager, then run rlord."
  else
    answer=n
    if [ "${RLORD_YES:-}" = 1 ]; then answer=y
    elif [ -r /dev/tty ]; then
      printf 'Install it now with: %s ? [Y/n] ' "$cmd"
      read -r answer </dev/tty || answer=n
      [ -z "$answer" ] && answer=y
    fi
    case $answer in
      y | Y | yes) sh -c "$cmd" || say "That didn't work; install tmux yourself: $cmd" ;;
      *) say "Install it later with: $cmd" ;;
    esac
  fi
fi

say ""
say "Next: run \`rlord install-hooks\` once (lets rlord see what Claude Code and Codex are doing), then \`rlord\`."
