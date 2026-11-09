#!/usr/bin/env bash
# Run the harness in the folder you are in now, for example inside a repository:
#   cd ~/code/some-repo && /path/to/src/harness.sh
# or put a link on your PATH once:
#   ln -s /path/to/src/harness.sh ~/.local/bin/harness
# The model's tools work in the current folder. The chat database and the logs are not written
# there: they are in the program's own folder in your user data folder (~/.local/share/aharness,
# or HARNESS_LOG_DIR). The only thing it may add to the repository is .Aharness/ (subagent notes),
# which ignores itself.
set -euo pipefail
src="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
export HARNESS_PROVIDER_DIR="${HARNESS_PROVIDER_DIR:-$src/providers}"
exec cargo run -q --manifest-path "$src/Cargo.toml" -p runner -- "$@"
