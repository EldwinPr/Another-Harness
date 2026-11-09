#!/usr/bin/env bash
# Start the chat. It runs in workspace/. The provider and model are chosen inside it:
# /provider, /provider add, /model, /models (see /help). It starts on the provider you switched
# to last time, or on none the first time.
#   ./chat.sh
#   ./chat.sh --resume                # continue the most recent saved chat
set -euo pipefail
src="$(cd "$(dirname "$0")" && pwd)"

# The program starts in workspace/, so the model's tools (read_file, bash) work there and not in
# src/, where the provider files (API keys) live. The chat database and the logs are in the
# program's own folder in your user data folder (~/.local/share/aharness), the same from any
# project; set HARNESS_LOG_DIR to use another one (for example "$src/logs", the old place).
# /provider lists the provider files in this folder next to the providers saved in the database.
export HARNESS_PROVIDER_DIR="$src/providers"
mkdir -p "$src/workspace"
cd "$src/workspace"
exec cargo run -q --manifest-path "$src/Cargo.toml" -p runner -- "$@"
