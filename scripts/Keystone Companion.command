#!/bin/zsh
set -eu
TASK_REPO_DIR="${0:A:h:h}"
cd "$TASK_REPO_DIR"
if [[ ! -x target/debug/chunguschillercord ]]; then
  cargo build --locked
fi
exec ./target/debug/chunguschillercord --keystones watch
