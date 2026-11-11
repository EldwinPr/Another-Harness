# Changelog

## 0.1.0

First release. Early software: the author's daily tool, with a large test suite (1,000+ tests, none
touch the network), few other users.

- A terminal chat agent assembled from system prompts, skills, personalities and profiles stored in
  a SQLite database, with a pure request builder (`kernel::assemble`).
- OpenRouter-style chat-completions and the Anthropic Messages API, with prompt-cache markers.
- File tools, `bash`, approval modes, plan mode, subagents, background commands, compaction.
- Glossary, context entries, a local web page for editing the library and reviewing the inbox.
- MCP client (use other servers' tools) and MCP server (read-only, buffer-with-inbox, direct),
  with optional LLM or Jev deciders that suggest an answer for inbox entries.

Platforms: Linux x86_64 only (`.deb`, `.rpm`, `.tar.gz`). Windows is not supported yet, because
process handling in `bash`, `tasks` and `mcp` uses Unix-only APIs. macOS is untested.
