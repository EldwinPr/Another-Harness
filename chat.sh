#!/usr/bin/env bash
# Start the chat. It runs in workspace/. The provider and model are chosen inside it:
# /provider, /provider add, /model, /models (see /help). It starts on the provider you switched
# to last time, or on none the first time.
#   ./chat.sh
#   ./chat.sh --resume                # continue the most recent saved chat
set -euo pipefail
src="$(cd "$(dirname "$0")" && pwd)"

# The program starts in workspace/, so the model's tools (read_file, bash) work there and not in
# src/, where the profiles (API keys) and the chat database live. The logs and the database stay
# in src/logs, so give their path explicitly: the default is relative to where the program starts.
export HARNESS_LOG_DIR="${HARNESS_LOG_DIR:-$src/logs}"
# /provider lists the profile files in this folder next to the providers saved in the database.
export HARNESS_PROFILE_DIR="$src/profiles"
mkdir -p "$src/workspace"
cd "$src/workspace"
exec cargo run -q --manifest-path "$src/Cargo.toml" -p runner -- "$@"
