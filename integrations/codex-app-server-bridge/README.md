# Official App Server integration

Free's native Rust adapter (`src/app_server/`) starts an owned official `codex app-server --listen stdio://` child per trusted caller on first use. It does not start a model turn or a persistent Node bridge. This directory retains the host configuration template at its existing path.

Copy `bridge.config.example.json` to an ignored `bridge.config.local.json`, enter your workspace and installed CLI paths, then add this source to Free's `codex.config.json`:

```json
{
  "mcpServers": {
    "codex_app_server": {
      "type": "codex-app-server",
      "mode": "catalog",
      "toolTimeoutSec": 600,
      "appServer": {
        "configFile": "/absolute/path/to/bridge.config.local.json"
      }
    }
  }
}
```

The same host fields may be written directly inside `appServer`. Do not combine this transport with `command`, `args`, `url`, HTTP headers or MCP OAuth settings. `tools` and `disabledTools` still filter the exposed adapter tools.

Existing `CODEX_APP_BRIDGE_CONFIG`, `CODEX_APP_CLI`, `CODEX_APP_BRIDGE_WORK_DIR`, `CODEX_HOME`, timeout and raw-RPC environment overrides remain supported. Relative host paths resolve against the referenced configuration directory; `~` expands to the user home. `cwd` sets the child process directory separately from `workDir`, which controls its utility context. Unknown host settings and invalid values are rejected. Raw RPC is disabled by default.

## Live tools

The 26 compatibility tools keep their names; `codex_ui_session_end` adds immediate caller-scoped cleanup. Upstream MCP tools are discovered at runtime: use `codex_mcp_status` to list names, add `serverName` and `toolName` to retrieve the exact live schema, then call `codex_mcp_call`. No provider-specific tool list is compiled into Free. Changes to the underlying App Server RPC protocol may still require an adapter update. With this adapter enabled, Free suppresses duplicate direct `cua_repl` and `node_repl` sources; use their runtime tools through App Server.

## Human confirmations

A pending operation returns `awaiting_confirmation`, its operation ID and the actual form. Present that form to the human; pass their explicit decision to `codex_confirmation_respond`. Continue the original operation rather than repeating its call. `codex_confirmations_list` and `codex_operation_result` are isolated by Free's trusted caller identity. Results preserve native image blocks, metadata and errors.

Operations are serialized within each caller's independent runtime because upstream confirmation requests do not always identify their originating tool. Confirmations expire and cancel; device verification cannot be replaced with textual approval.

## Session cleanup

Consecutive calls from one trusted conversation reuse its runtime and variables. Different callers have separate processes, REPL variables, queues, and confirmation registries. After five idle minutes following the last completed call, Free sends the official `turn_ended` hook, unsubscribes the utility thread, and stops the owned runtime. Queued/running calls and pending confirmations prevent idle reclamation. RPC and confirmation timeouts remain bounded independently. This timer does not require another incoming request or a model-issued close call; MCP does not supply ChatGPT's turn-completed event.

Call `codex_ui_session_end` with `{}` to end the current caller's session immediately. It also cancels this caller's pending operations, is idempotent, and never selects another caller's session. Cleanup failures return warnings and fall back to stopping that caller's owned process. A subsequent new call creates fresh state; old REPL variables and browser handles cannot be reused. Pending operations and UI variables are memory-only. After expiry, errors, cancellation, or restart, inspect external state before retrying a write. Never automatically replay the original operation.

## Recovery and tests

An exited child is rebuilt on the next new call, without replaying the failed operation. Only the adapter's owned process is stopped. Rust's native commands, file tools and `local_shell/exec` remain available when App Server is unavailable.

```sh
cargo test --offline app_server::
```

These tests use a fake CLI and do not request OS permissions. An opt-in read-only smoke test uses an existing installation and configuration:

```sh
CODEX_APP_SERVER_TEST_CONFIG=/absolute/path/to/bridge.config.local.json cargo test --offline installed_app_server_read_only_smoke -- --ignored
```

Actual UI actions and screenshots must also be checked through the deployed Free connector. The adapter does not imply access to the desktop App's embedded browser; inspect `codex_cua_state` for available backends.
