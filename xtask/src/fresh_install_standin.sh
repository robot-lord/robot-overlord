#!/bin/sh
# A stand-in for an agent's CLI (installed as claude, codex and opencode by
# fresh_install.sh): logs every call, answers --version, and otherwise
# idles in a terminal or fails. Never a real agent, never a model call.
name=$(basename "$0")
echo "$name $*" >>/tmp/standin-calls.log
case "${1:-}" in
  --version)
    case $name in
      claude) echo "2.1.200 (Claude Code)" ;;
      codex) echo "codex-cli 0.200.0" ;;
      opencode) echo "2.5.0" ;;
    esac
    exit 0
    ;;
esac
if [ -t 0 ]; then
  echo "stand-in $name: $*"
  exec sleep 3600
fi
exit 1
