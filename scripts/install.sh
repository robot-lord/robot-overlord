#!/usr/bin/env bash
# Install (or upgrade) the rlord binary sitting next to this script into
# ~/.local/bin. Shipped inside each release tarball; see scripts/release.sh.
#
# No daemon management needed: the next rlord command notices the running
# daemon is from an older build and restarts it on the new binary.
#
#   install.sh [--force]   # --force: replace ~/.local/bin/rlord even if it
#                          # is a symlink (e.g. a dev install into target/)
set -euo pipefail
force=0
[[ "${1:-}" == "--force" ]] && force=1
src="$(cd "$(dirname "$0")" && pwd)/rlord"
dest_dir=${RLORD_INSTALL_DIR:-$HOME/.local/bin}
dest=$dest_dir/rlord

[[ -f "$src" ]] || { echo "no rlord binary next to $0" >&2; exit 1; }
mkdir -p "$dest_dir"
if [[ -L "$dest" && $force -eq 0 ]]; then
  echo "$dest is a symlink to $(readlink "$dest") (a dev install?)." >&2
  echo "Installing would replace it with this release; rerun with --force to do that." >&2
  exit 1
fi

# Browsers tag downloads with quarantine (gh/curl don't); clear it either way.
xattr -d com.apple.quarantine "$src" 2>/dev/null || true
codesign --verify "$src"

# Copy then rename: overwriting a signed binary in place makes macOS kill it
# ("Killed: 9") because the kernel caches signatures per inode.
tmp=$(mktemp "$dest_dir/.rlord.XXXXXX")
trap 'rm -f "$tmp"' EXIT  # no-op once mv has consumed it
cp "$src" "$tmp"
chmod 755 "$tmp"
mv -f "$tmp" "$dest"

echo "installed $("$dest" --version) to $dest"
case ":$PATH:" in
  *":$dest_dir:"*) ;;
  *) echo "note: $dest_dir is not on your PATH" ;;
esac
if [[ ! -f "$HOME/.claude/settings.json" ]] || ! grep -q "rlord hook" "$HOME/.claude/settings.json" 2>/dev/null; then
  echo "first install? run: $dest claude install-hooks"
fi
