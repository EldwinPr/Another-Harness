#!/usr/bin/env bash
# Start the chat. It runs in workspace/. The provider and model are chosen inside it:
# /provider, /provider add, /model, /models (see /help). It starts on the provider you switched
# to last time, or on none the first time.
#   ./chat.sh
#   ./chat.sh --resume                # continue the most recent saved chat
#   ./chat.sh --mcp rust-docs         # add a ready-made MCP toolset (once; it starts in this chat)
#   ./chat.sh --mcp-add NAME COMMAND [ARGS] [--env N=v] [--timeout S]   # add any MCP server
#   ./chat.sh --mcp-serve             # no chat: an MCP server on stdin/stdout for another harness: it
#                                     # reads the database and can submit proposals to your inbox (below)
#   ./chat.sh --mcp-serve --read-only # the same, with the read tools alone (what --mcp-serve was before
#                                     # the inbox existed)
#   ./chat.sh --mcp-serve --direct    # the same, with tools that change the database in place of submit
# --mcp and --mcp-add save the server in the database and leave one that is already saved as it is;
# change rules with /mcp config, or ask the model (it has an mcp tool and asks you before every change).
# --mcp-serve needs no provider. By default (the buffer mode) the other harness cannot change what you
# keep: it can only `submit` a proposal, which waits in your inbox (the web page's Inbox, ./chat.sh
# --serve) until you accept or reject it, and read the outcome with `submission_get`. Note that a plain
# --mcp-serve registered before this change now offers `submit`, which adds one row to the inbox and
# nothing else; use --read-only to keep it to reading. With --direct it can instead add, replace and
# delete context entries, glossary terms, skills, personalities, prompts and profiles itself (never
# switching one on). --read-only and --direct exclude each other. It stays in the folder it was
# started from, so that folder is the project whose context entries and glossary it serves (a tool
# can name another).
# Register it with a harness as a stdio server whose command is this script plus --mcp-serve.
#   ./chat.sh --mcp-serve --decider SPEC   # a model suggests accept, reject or unsure for each new
#                                          # submission; you still decide (see below)
#   ./chat.sh --eval-decider SPEC          # run 18 labelled proposals through a model and print how
#                                          # it did, then exit; reads the saved providers, writes nothing
# SPEC is none (the default: no model, nothing is sent anywhere), llm (the provider the chat used
# last, with its default model), llm:PROVIDER (a provider saved with /provider add, with its default
# model) or llm:PROVIDER:MODEL (the model may hold ':' and '/'); or jev, jev:PROVIDER or
# jev:PROVIDER:MODEL: OpenRouter's Jev decisions model (POST .../api/alpha/decisions, an alpha API),
# which gives probabilities and no reasons. The provider must be a saved OpenRouter one with a key
# (it supplies the address and the key; its chat model is not used); the model is the pinned
# typesafe/jev-1.14-20261014 unless MODEL is given. An unknown provider, one without an API key (a
# router on this machine may need none, but not for jev) or without a model (llm only) or, for jev,
# one that is not OpenRouter stops the start with the reason. --decider excludes --read-only and
# --direct; --eval-decider excludes every other option and none. With a decider, the text of each
# new submission (and the stored text it would replace) is SENT TO THAT PROVIDER and spends its
# tokens (jev: $0.042 per million input tokens); the answer is a
# suggestion shown next to the entry on the Inbox page and in /inbox, never applied by itself and
# never shown to the sender. A failed call leaves the entry without a suggestion (one line on stderr).
set -euo pipefail
src="$(cd "$(dirname "$0")" && pwd)"

# The program starts in workspace/, so the model's tools (read_file, bash) work there and not in
# src/, where the provider files (API keys) live. The chat database and the logs are in the
# program's own folder in your user data folder (~/.local/share/aharness), the same from any
# project; set HARNESS_LOG_DIR to use another one (for example "$src/logs", the old place).
# /provider lists the provider files in this folder next to the providers saved in the database.
export HARNESS_PROVIDER_DIR="$src/providers"
serving=0
for arg in "$@"; do
    if [ "$arg" = "--mcp-serve" ]; then serving=1; fi
done
if [ "$serving" = 0 ]; then
    mkdir -p "$src/workspace"
    cd "$src/workspace"
    exec cargo run -q --manifest-path "$src/Cargo.toml" -p runner -- "$@"
fi
# A server stays in the caller's folder (that folder is its project). Build from src/ so the
# toolchain pin and cargo settings there apply whatever the caller's folder holds, then run the
# binary from the caller's folder.
(cd "$src" && cargo build -q -p runner)
exec "$src/target/debug/runner" "$@"
