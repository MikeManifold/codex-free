# Codex App Server MCP bridge

This optional Node.js stdio MCP server exposes local App Server tools without starting a model turn. It uses the official installed Codex CLI. Browser availability and OS permissions depend on the actual host; inspect `codex_cua_state` before choosing a backend.

Copy `bridge.config.example.json` to `bridge.config.local.json` and set your workspace and CLI paths. Personal files are ignored by Git. `CODEX_APP_BRIDGE_CONFIG` selects another file. Existing `CODEX_APP_CLI`, `CODEX_APP_BRIDGE_WORK_DIR`, `CODEX_HOME`, timeout and raw-RPC environment overrides remain supported. Relative paths resolve against the configuration directory; `~` expands to the user home. Unknown keys and invalid values are rejected. Raw RPC is disabled by default.

Run `node integrations/codex-app-server-bridge/server.mjs` as an MCP stdio server. Keep stdout reserved for protocol messages.

## Confirmations

The bridge supports App Server's OpenAI form extension. A tool requiring confirmation returns `awaiting_confirmation`, its original operation ID and the actual form. Display that form to the human and collect an explicit decision before calling `codex_confirmation_respond`. Never infer consent from a setup or repair request, fabricate field values, or repeat the original pending call. `codex_confirmations_list` lists this caller's pending operations; `codex_operation_result` retrieves the original result, preserving native image blocks.

Operations are serialized because upstream elicitation does not identify its originating tool call. When accessed through Free, a server-derived hashed caller identity scopes pending operations across MCP reconnects. A direct stdio connection represents one caller. Confirmations expire and cancel; device-verification challenges cannot be replaced with textual approval. Operations are memory-only and disappear when the bridge restarts, so inspect external state before retrying a write after a restart.

The bridge never grants confirmation automatically. A client still needs a human-facing interaction step to present and answer forms.

## Tests

```sh
node --test tests/test_bridge_config.mjs tests/test_bridge_confirmations.mjs tests/test_bridge_confirmation_stdio.mjs tests/test_codex_app_server_bridge.mjs
```

`mcp-result.mjs` preserves upstream media, metadata, errors and structured results. Screenshots are ordinary MCP image content, separate from project preview widgets.
