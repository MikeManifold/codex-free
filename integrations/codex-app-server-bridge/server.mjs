#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { randomUUID } from 'node:crypto';
import process from 'node:process';
import { forwardMcpResult } from './mcp-result.mjs';
import { loadBridgeConfig } from './config.mjs';
import { ConfirmationOperations } from './confirmations.mjs';

const VERSION = '0.3.0';
const CONFIG = loadBridgeConfig();
const CODEX_CLI = CONFIG.codexCli;
const WORK_DIR = CONFIG.workDir;
const REQUEST_TIMEOUT_MS = CONFIG.requestTimeoutMs;
const ALLOW_RAW_RPC = CONFIG.allowRawRpc;
const operations = new ConfirmationOperations({ timeoutMs: CONFIG.confirmationTimeoutMs });

function rpcResult(id, result) {
  return { jsonrpc: '2.0', id, result };
}
function rpcError(id, code, message, data) {
  return { jsonrpc: '2.0', id, error: { code, message, ...(data === undefined ? {} : { data }) } };
}
function writeMessage(msg) {
  process.stdout.write(`${JSON.stringify(msg)}\n`);
}
function textResult(text, structuredContent) {
  return {
    content: [{ type: 'text', text: typeof text === 'string' ? text : JSON.stringify(text, null, 2) }],
    ...(structuredContent === undefined ? {} : { structuredContent }),
  };
}
function asText(value) {
  if (value === undefined) return '';
  if (typeof value === 'string') return value;
  return JSON.stringify(value, null, 2);
}
function fileUri(path) {
  if (typeof path !== 'string' || !path.startsWith('/')) throw new Error('path must be an absolute macOS path');
  return path;
}
class AppServerClient {
  constructor() {
    this.child = null;
    this.pending = new Map();
    this.nextId = 1;
    this.readyPromise = null;
    this.stderrTail = [];
    this.utilityThreadId = null;
    this.utilitySessionId = null;
    this.syntheticTurnId = null;
  }

  async ensureReady() {
    if (this.readyPromise) return this.readyPromise;
    this.readyPromise = this.#start();
    try {
      await this.readyPromise;
    } catch (error) {
      this.readyPromise = null;
      throw error;
    }
  }

  async #start() {
    const child = spawn(CODEX_CLI, ['app-server', '--listen', 'stdio://'], {
      stdio: ['pipe', 'pipe', 'pipe'],
      env: { ...process.env, CODEX_HOME: CONFIG.codexHome },
    });
    this.child = child;

    const rl = createInterface({ input: child.stdout, crlfDelay: Infinity });
    rl.on('line', (line) => this.#onLine(line));
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (chunk) => {
      const pieces = String(chunk).split(/\r?\n/).filter(Boolean);
      this.stderrTail.push(...pieces);
      if (this.stderrTail.length > 80) this.stderrTail.splice(0, this.stderrTail.length - 80);
    });
    child.on('exit', (code, signal) => {
      const err = new Error(`Codex app-server exited (code=${code}, signal=${signal})${this.stderrTail.length ? `\n${this.stderrTail.join('\n')}` : ''}`);
      for (const { reject, timer } of this.pending.values()) {
        clearTimeout(timer);
        reject(err);
      }
      this.pending.clear();
      this.child = null;
      this.readyPromise = null;
      this.utilityThreadId = null;
      this.utilitySessionId = null;
      this.syntheticTurnId = null;
    });

    const init = await this.request('initialize', {
      clientInfo: {
        name: 'chatgpt_local_mcp_bridge',
        title: 'ChatGPT Local Codex App Bridge',
        version: VERSION,
      },
      capabilities: {
        experimentalApi: true,
        explicitGatewayOauth: true,
        extensions: { 'openai/form': {} },
        mcpServerOpenaiFormElicitation: true,
        optOutNotificationMethods: [
          'thread/started',
          'item/agentMessage/delta',
          'item/reasoning/delta',
          'command/exec/outputDelta',
        ],
      },
    }, 20000, true);
    this.notify('initialized', {});
    return init;
  }

  #onLine(line) {
    if (!line.trim()) return;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      return;
    }
    if (msg && Object.prototype.hasOwnProperty.call(msg, 'id')) {
      const key = String(msg.id);
      const pending = this.pending.get(key);
      if (pending && (Object.prototype.hasOwnProperty.call(msg, 'result') || Object.prototype.hasOwnProperty.call(msg, 'response') || Object.prototype.hasOwnProperty.call(msg, 'error'))) {
        this.pending.delete(key);
        clearTimeout(pending.timer);
        if (msg.error) {
          const e = new Error(msg.error.message || 'Codex app-server RPC error');
          e.rpc = msg.error;
          pending.reject(e);
        } else {
          pending.resolve(Object.prototype.hasOwnProperty.call(msg, 'result') ? msg.result : msg.response);
        }
        return;
      }
      if (msg.method === 'mcpServer/elicitation/request') {
        const respond = result => {
          if (this.child?.stdin?.writable) this.child.stdin.write(`${JSON.stringify({ id: msg.id, result })}\n`);
        };
        if (operations.request(msg.params, respond)) {
          // Waiting for a person is not an upstream RPC timeout. Keep the
          // original request alive; never resubmit a potentially mutating call.
          for (const [key, pending] of this.pending) {
            clearTimeout(pending.timer);
            pending.timer = setTimeout(() => {
              this.pending.delete(key);
              pending.reject(new Error('App Server did not finish after confirmation; check the original operation before retrying.'));
            }, CONFIG.confirmationTimeoutMs + REQUEST_TIMEOUT_MS);
          }
        } else respond({ action: 'cancel' });
        return;
      }
      // Fail closed for unexpected server->client requests instead of hanging.
      if (msg.method) {
        this.child?.stdin.write(`${JSON.stringify({ id: msg.id, error: { code: -32001, message: `Unsupported server request: ${msg.method}` } })}\n`);
      }
    }
  }

  request(method, params, timeoutMs = REQUEST_TIMEOUT_MS, duringInit = false) {
    if (!duringInit && !this.child) {
      return this.ensureReady().then(() => this.request(method, params, timeoutMs));
    }
    if (!this.child?.stdin?.writable) throw new Error('Codex app-server is not running');
    const id = this.nextId++;
    const payload = params === undefined ? { method, id } : { method, id, params };
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(String(id));
        reject(new Error(`Codex app-server RPC timed out after ${timeoutMs}ms: ${method}`));
      }, timeoutMs);
      this.pending.set(String(id), { resolve, reject, timer, method });
      this.child.stdin.write(`${JSON.stringify(payload)}\n`);
    });
  }

  notify(method, params) {
    if (!this.child?.stdin?.writable) throw new Error('Codex app-server is not running');
    const payload = params === undefined ? { method } : { method, params };
    this.child.stdin.write(`${JSON.stringify(payload)}\n`);
  }

  async utilityThread() {
    await this.ensureReady();
    if (this.utilityThreadId) return this.utilityThreadId;
    const response = await this.request('thread/start', {
      cwd: WORK_DIR,
      approvalPolicy: 'never',
      sandbox: 'danger-full-access',
      ephemeral: true,
    });
    const id = response?.thread?.id || response?.threadId || response?.id;
    if (!id) throw new Error(`thread/start did not return a thread id: ${JSON.stringify(response)}`);
    this.utilityThreadId = id;
    this.utilitySessionId = response?.thread?.sessionId || response?.sessionId || id;
    this.syntheticTurnId = randomUUID();
    return id;
  }

  turnMetadata() {
    if (!this.utilityThreadId) throw new Error('utility thread is not initialized');
    return JSON.stringify({
      session_id: this.utilitySessionId || this.utilityThreadId,
      thread_id: this.utilityThreadId,
      turn_id: this.syntheticTurnId || (this.syntheticTurnId = randomUUID()),
      thread_source: 'user',
    });
  }

  diagnostics() {
    return {
      cli: CODEX_CLI,
      workDir: WORK_DIR,
      appServerRunning: Boolean(this.child),
      utilityThreadId: this.utilityThreadId,
      utilitySessionId: this.utilitySessionId,
      syntheticTurnId: this.syntheticTurnId,
      modelTurnsStartedByBridge: 0,
      stderrTail: this.stderrTail.slice(-20),
    };
  }
}

const app = new AppServerClient();

const TOOLS = [
  {
    name: 'codex_confirmations_list',
    description: 'List only this caller\'s pending App Server confirmations. Show the actual form/message to the human. Never infer consent or repeat the original pending tool call.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
  },
  {
    name: 'codex_confirmation_respond',
    description: 'Reply once to a pending confirmation only after the human explicitly answers that exact form. Set userConfirmed=true only for an actual user decision. Content must contain the user\'s supplied fields, never fabricated approval. Device verification requires the supported Codex UI. Returns the original operation result or its pending status; do not recreate an operation.',
    inputSchema: { type: 'object', additionalProperties: false, required: ['operationId', 'confirmationId', 'action', 'userConfirmed'], properties: {
      operationId: { type: 'string' }, confirmationId: { type: 'string' }, action: { enum: ['accept', 'decline', 'cancel'] },
      userConfirmed: { type: 'boolean' }, content: { description: 'Actual structured user input required for accepted forms.' },
    } },
  },
  {
    name: 'codex_operation_result',
    description: 'Retrieve the result of an existing bridge operation, including native image blocks. Poll this after confirmation; never repeat a pending write or create call.',
    inputSchema: { type: 'object', additionalProperties: false, required: ['operationId'], properties: { operationId: { type: 'string' }, waitMs: { type: 'integer', minimum: 0, maximum: 30000 } } },
  },
  {
    name: 'codex_app_status',
    description: 'Check the local Codex App bridge, App Server initialization, configured work directory, and MCP status without starting a model turn.',
    inputSchema: { type: 'object', additionalProperties: false, properties: {} },
  },
  {
    name: 'codex_exec',
    description: 'Execute an argv-vector command through Codex App Server command/exec. This uses Codex\'s own local execution environment and configured sandbox; it does not create a model turn.',
    inputSchema: {
      type: 'object', additionalProperties: false, required: ['command'],
      properties: {
        command: { type: 'array', minItems: 1, items: { type: 'string' }, description: 'argv vector, e.g. ["python3","-c","print(2+2)"]' },
        cwd: { type: 'string', description: 'Absolute working directory. Defaults to the configured project directory.' },
        env: { type: 'object', additionalProperties: { type: ['string', 'null'] } },
        timeoutMs: { type: 'integer', minimum: 1, maximum: 600000 },
      },
    },
  },
  {
    name: 'codex_fs_read',
    description: 'Read a local file through Codex App Server fs/readFile. The path is an absolute Mac path; Codex App owns filesystem policy.',
    inputSchema: {
      type: 'object', additionalProperties: false, required: ['path'],
      properties: {
        path: { type: 'string' },
        encoding: { type: 'string', enum: ['utf8', 'base64'] },
        followSymlinks: { type: 'boolean' },
      },
    },
  },
  {
    name: 'codex_fs_write',
    description: 'Write a local file through Codex App Server fs/writeFile. This uses App Server native filesystem RPC rather than codex-free review checkpoints.',
    inputSchema: {
      type: 'object', additionalProperties: false, required: ['path', 'content'],
      properties: {
        path: { type: 'string' },
        content: { type: 'string' },
        encoding: { type: 'string', enum: ['utf8', 'base64'] },
        followSymlinks: { type: 'boolean' },
      },
    },
  },
  {
    name: 'codex_fs_list',
    description: 'List a local directory through Codex App Server fs/readDirectory.',
    inputSchema: { type: 'object', additionalProperties: false, required: ['path'], properties: { path: { type: 'string' } } },
  },
  {
    name: 'codex_mcp_status',
    description: 'List MCP servers as seen by Codex App Server, including runtime/tool/auth status. This uses Codex\'s native MCP registry rather than re-parsing config files.',
    inputSchema: {
      type: 'object', additionalProperties: false,
      properties: {
        serverName: { type: 'string' },
        detail: { type: 'string', enum: ['full', 'toolsAndAuthOnly'] },
      },
    },
  },
  {
    name: 'codex_mcp_call',
    description: 'Call an MCP tool through Codex App Server. The bridge creates an ephemeral utility thread only as MCP runtime scope and never starts a model turn.',
    inputSchema: {
      type: 'object', additionalProperties: false, required: ['server', 'tool'],
      properties: {
        server: { type: 'string' },
        tool: { type: 'string' },
        arguments: { type: 'object', additionalProperties: true },
      },
    },
  },
  {
    name: 'codex_mcp_oauth_login',
    description: 'Start Codex App Server native OAuth login for a configured MCP server. Codex owns token storage and refresh; this bridge never reads refresh tokens.',
    inputSchema: {
      type: 'object', additionalProperties: false, required: ['name'],
      properties: { name: { type: 'string' } },
    },
  },
  {
    name: 'codex_cua_docs',
    description: 'Return the live documentation generated by Codex unified Computer Use / Browser runtime. Use before writing nontrivial cua JavaScript.',
    inputSchema: { type: 'object', additionalProperties: false, properties: {} },
  },
  {
    name: 'codex_cua_state',
    description: 'Get the native apps and browser backends actually exposed by this runtime. Check this before browser actions. The standalone CLI bridge normally exposes Chrome and native Computer Use; an in-app browser requires a desktop-provided context and must not be assumed available.',
    inputSchema: { type: 'object', additionalProperties: false, properties: {} },
  },
  {
    name: 'codex_cua_js',
    description: 'Execute JavaScript in the unified Computer Use runtime (cua_repl when available, otherwise the trusted node_repl runtime). Use codex_cua_state to discover available backends first. Chrome and native macOS control can work independently of a desktop in-app browser; do not substitute another backend after an access denial.',
    inputSchema: {
      type: 'object', additionalProperties: false, required: ['code'],
      properties: {
        code: { type: 'string' },
        title: { type: 'string', maxLength: 80 },
        timeout_ms: { type: 'integer', minimum: 1, maximum: 300000 },
      },
    },
  },
  {
    name: 'codex_skills_list',
    description: 'List skills through Codex App Server using the same Codex home and project cwd.',
    inputSchema: { type: 'object', additionalProperties: false, properties: { forceReload: { type: 'boolean' } } },
  },
  {
    name: 'codex_plugins_list',
    description: 'List plugins through Codex App Server, including bundled Computer Use and app-managed plugins.',
    inputSchema: { type: 'object', additionalProperties: false, properties: { forceRefetch: { type: 'boolean' } } },
  },
  {
    name: 'codex_apps_list',
    description: 'List Codex Apps/connectors through App Server, using the same installed/connected app catalog as Codex Desktop.',
    inputSchema: {
      type: 'object', additionalProperties: false,
      properties: {
        source: { type: 'string', enum: ['runtime', 'directory'], description: 'runtime (default) reads the already loaded codex_apps MCP catalog; directory explicitly queries the remote App directory.' },
        forceRefetch: { type: 'boolean' },
        limit: { type: 'integer', minimum: 1, maximum: 500 },
        cursor: { type: 'string' },
      },
    },
  },
  {
    name: 'codex_apps_read',
    description: 'Read metadata and optional public tool summaries for selected Codex Apps/connectors.',
    inputSchema: {
      type: 'object', additionalProperties: false, required: ['appIds'],
      properties: {
        appIds: { type: 'array', minItems: 1, maxItems: 100, items: { type: 'string' } },
        includeTools: { type: 'boolean' },
        source: { type: 'string', enum: ['runtime', 'directory'], description: 'runtime (default) reads the already loaded codex_apps MCP catalog.' },
      },
    },
  },
  {
    name: 'codex_permissions_list',
    description: 'List Codex permission profiles resolved for the configured project cwd.',
    inputSchema: {
      type: 'object', additionalProperties: false,
      properties: {
        limit: { type: 'integer', minimum: 1, maximum: 500 },
        cursor: { type: 'string' },
      },
    },
  },
  {
    name: 'codex_projects_list',
    description: 'List projects known to Codex App Server without creating a model turn.',
    inputSchema: {
      type: 'object', additionalProperties: false,
      properties: {
        limit: { type: 'integer', minimum: 1, maximum: 500 },
        cursor: { type: 'string' },
        sortKey: { type: 'string', enum: ['position', 'recencyAt'] },
        sortDirection: { type: 'string', enum: ['asc', 'desc'] },
      },
    },
  },
  {
    name: 'codex_hooks_list',
    description: 'List Codex hooks resolved for the configured project cwd.',
    inputSchema: { type: 'object', additionalProperties: false, properties: {} },
  },
  {
    name: 'codex_models_list',
    description: 'List models exposed by this Codex App Server runtime.',
    inputSchema: {
      type: 'object', additionalProperties: false,
      properties: {
        includeHidden: { type: 'boolean' },
        limit: { type: 'integer', minimum: 1, maximum: 500 },
        cursor: { type: 'string' },
      },
    },
  },
  {
    name: 'codex_account_read',
    description: 'Read the Codex account/session identity summary through App Server. Does not expose OAuth refresh tokens.',
    inputSchema: { type: 'object', additionalProperties: false, properties: {} },
  },
  {
    name: 'codex_remote_control_status',
    description: 'Read Codex App Server remote-control status without enabling or changing remote control.',
    inputSchema: { type: 'object', additionalProperties: false, properties: {} },
  },
  {
    name: 'codex_capabilities',
    description: 'Summarize Codex App-managed MCP servers, skills, plugins, permission profiles, and app-server execution availability without starting a model turn.',
    inputSchema: { type: 'object', additionalProperties: false, properties: {} },
  },
  {
    name: 'codex_rpc',
    description: 'Advanced escape hatch for an allowlisted Codex App Server RPC. By default only read/diagnostic/app-server capability methods are allowed. Set CODEX_APP_BRIDGE_ALLOW_RAW_RPC=1 to permit arbitrary methods.',
    inputSchema: {
      type: 'object', additionalProperties: false, required: ['method'],
      properties: {
        method: { type: 'string' },
        params: {},
      },
    },
  },
];

const SAFE_RPC = new Set([
  'config/read',
  'permissionProfile/list',
  'model/list',
  'experimentalFeature/list',
  'account/read',
  'account/usage/read',
  'mcpServerStatus/list',
  'mcpServer/resource/read',
  'skills/list',
  'hooks/list',
  'plugin/list',
  'plugin/read',
  'plugin/installed',
  'app/list',
  'app/read',
  'app/installed',
  'environment/info',
  'environment/status',
  'remoteControl/status/read',
  'project/list',
  'project/read',
  'fs/readFile',
  'fs/getMetadata',
  'fs/readDirectory',
]);

async function mcpStatus(args = {}) {
  const params = {
    ...(args.detail ? { detail: args.detail } : { detail: 'toolsAndAuthOnly' }),
    ...(args.serverName ? { serverName: args.serverName } : {}),
  };
  try {
    return await app.request('mcpServerStatus/list', params);
  } catch (error) {
    // Older App Server builds may not know the detail selector. Retry with the stable core params.
    if (params.detail) return app.request('mcpServerStatus/list', args.serverName ? { serverName: args.serverName } : {});
    throw error;
  }
}

function summarizeMcpStatus(status) {
  const data = Array.isArray(status?.data) ? status.data : [];
  return {
    data: data.map((x) => {
      const tools = x?.tools && typeof x.tools === 'object' ? Object.keys(x.tools) : [];
      return {
        name: x?.name ?? null,
        authStatus: x?.authStatus ?? null,
        runtimeStatus: x?.runtimeStatus ?? null,
        pluginId: x?.pluginId ?? null,
        httpOrigin: x?.httpOrigin ?? null,
        serverInfo: x?.serverInfo ? {
          name: x.serverInfo.name ?? null,
          title: x.serverInfo.title ?? null,
          version: x.serverInfo.version ?? null,
        } : null,
        toolCount: tools.length,
        toolNames: tools.slice(0, 40),
        toolsTruncated: tools.length > 40,
        toolsError: x?.toolsError ?? null,
        resourceCount: Array.isArray(x?.resources) ? x.resources.length : 0,
        resourceTemplateCount: Array.isArray(x?.resourceTemplates) ? x.resourceTemplates.length : 0,
      };
    }),
    nextCursor: status?.nextCursor ?? null,
  };
}

function summarizeNamedCollection(value) {
  if (Array.isArray(value)) {
    return { count: value.length, names: value.map((x) => x?.name || x?.id || x?.title).filter(Boolean).slice(0, 200) };
  }
  if (!value || typeof value !== 'object') return value;
  for (const key of ['data', 'items', 'plugins', 'skills', 'apps', 'projects', 'profiles']) {
    if (Array.isArray(value[key])) {
      const arr = value[key];
      return {
        count: arr.length,
        names: arr.map((x) => x?.name || x?.id || x?.title || x?.displayName).filter(Boolean).slice(0, 200),
        nextCursor: value.nextCursor ?? value.next_cursor ?? null,
      };
    }
  }
  return value;
}

async function codexAppsCatalogSnapshot() {
  const status = await mcpStatus({ detail: 'toolsAndAuthOnly' });
  const servers = Array.isArray(status?.data) ? status.data : [];
  const server = servers.find((x) => x?.name === 'codex_apps');
  if (!server) throw new Error('codex_apps MCP server is not available');

  const groups = new Map();
  const tools = server?.tools && typeof server.tools === 'object' ? Object.values(server.tools) : [];
  for (const tool of tools) {
    const meta = tool?._meta || {};
    const connectorId = meta.connector_id || meta._codex_apps?.connector_id || 'unknown';
    const connectorName = meta.connector_name || connectorId;
    const connectorDescription = meta.connector_description || null;
    let item = groups.get(connectorId);
    if (!item) {
      item = {
        id: connectorId,
        name: connectorName,
        description: connectorDescription,
        source: 'codex_apps_mcp_fallback',
        tools: [],
      };
      groups.set(connectorId, item);
    }
    item.tools.push({
      name: tool?.name || null,
      title: tool?.title || null,
      description: tool?.description || null,
      annotations: tool?.annotations || null,
    });
  }
  return [...groups.values()].map((item) => ({
    ...item,
    toolCount: item.tools.length,
  })).sort((a, b) => String(a.name).localeCompare(String(b.name)));
}

async function appsListWithFallback(args = {}) {
  const useDirectory = args.source === 'directory';
  if (useDirectory) {
    try {
      return await app.request('app/list', {
        ...(args.forceRefetch === undefined ? {} : { forceRefetch: Boolean(args.forceRefetch) }),
        ...(args.limit ? { limit: args.limit } : {}),
        ...(args.cursor ? { cursor: args.cursor } : {}),
      });
    } catch (error) {
      if (!unsupportedDirectoryMethod(error)) throw error;
      const catalog = await codexAppsCatalogSnapshot();
      const limit = args.limit || catalog.length;
      return {
        data: catalog.slice(0, limit).map(({ tools, ...rest }) => rest),
        nextCursor: null,
        source: 'runtime',
        fallback: true,
        fallbackReason: String(error?.message || error).slice(0, 500),
      };
    }
  }
  const catalog = await codexAppsCatalogSnapshot();
  const limit = args.limit || catalog.length;
  return {
    data: catalog.slice(0, limit).map(({ tools, ...rest }) => rest),
    nextCursor: null,
    source: 'runtime',
    fallback: false,
  };
}

async function appsReadWithFallback(args = {}) {
  const useDirectory = args.source === 'directory';
  if (useDirectory) {
    try {
      return await app.request('app/read', {
        appIds: args.appIds,
        includeTools: Boolean(args.includeTools),
      });
    } catch (error) {
      if (!unsupportedDirectoryMethod(error)) throw error;
      const wanted = new Set(args.appIds || []);
      const catalog = await codexAppsCatalogSnapshot();
      return {
        data: catalog
          .filter((item) => wanted.has(item.id))
          .map((item) => args.includeTools ? item : (({ tools, ...rest }) => rest)(item)),
        source: 'runtime',
        fallback: true,
        fallbackReason: String(error?.message || error).slice(0, 500),
      };
    }
  }
  const wanted = new Set(args.appIds || []);
  const catalog = await codexAppsCatalogSnapshot();
  return {
    data: catalog
      .filter((item) => wanted.has(item.id))
      .map((item) => args.includeTools ? item : (({ tools, ...rest }) => rest)(item)),
    source: 'runtime',
    fallback: false,
  };
}

function unsupportedDirectoryMethod(error) {
  // A declined confirmation or an authorization error must remain visible.
  return error?.rpc?.code === -32601 || /^(?:unknown|unsupported) (?:method|variant)\b/i.test(error?.message || '');
}

async function chooseCuaServer() {
  const status = await mcpStatus({ detail: 'toolsAndAuthOnly' });
  const data = Array.isArray(status?.data) ? status.data : [];
  const names = data.map((x) => x?.name).filter(Boolean);
  if (names.includes('cua_repl')) return 'cua_repl';
  if (names.includes('node_repl')) return 'node_repl';
  throw new Error(`No Codex Computer Use runtime is available. MCP servers: ${names.join(', ')}`);
}

async function callMcp(server, tool, args = {}, options = {}) {
  const threadId = await app.utilityThread();
  const params = { threadId, server, tool, arguments: args };
  if (options.turnMetadata) {
    params._meta = { 'x-codex-turn-metadata': app.turnMetadata() };
  }
  return app.request('mcpServer/tool/call', params);
}

async function callCuaJs(args) {
  const server = await chooseCuaServer();
  return callMcp(server, 'js', args, { turnMetadata: true });
}

async function executeTool(name, args = {}) {
  switch (name) {
    case 'codex_app_status': {
      const init = await app.ensureReady();
      let mcp;
      try { mcp = await mcpStatus({ detail: 'toolsAndAuthOnly' }); } catch (e) { mcp = { error: String(e.message || e) }; }
      const data = { version: VERSION, initialize: init, bridge: app.diagnostics(), mcp: mcp?.error ? mcp : summarizeMcpStatus(mcp) };
      return textResult(data, data);
    }
    case 'codex_exec': {
      await app.ensureReady();
      const response = await app.request('command/exec', {
        command: args.command,
        cwd: args.cwd || WORK_DIR,
        ...(args.env ? { env: args.env } : {}),
        tty: false,
        streamStdin: false,
        streamStdoutStderr: false,
        ...(args.timeoutMs ? { timeoutMs: args.timeoutMs } : {}),
      }, args.timeoutMs ? Math.min(args.timeoutMs + 10000, 610000) : REQUEST_TIMEOUT_MS);
      const text = [
        `exitCode: ${response?.exitCode ?? 'unknown'}`,
        response?.stdout ? `stdout:\n${response.stdout}` : '',
        response?.stderr ? `stderr:\n${response.stderr}` : '',
      ].filter(Boolean).join('\n');
      return textResult(text, response);
    }
    case 'codex_fs_read': {
      await app.ensureReady();
      const response = await app.request('fs/readFile', {
        path: fileUri(args.path),
        ...(args.followSymlinks === undefined ? {} : { followSymlinks: args.followSymlinks }),
      });
      const data = response?.dataBase64 ?? response?.data_base64;
      if (typeof data !== 'string') return textResult(response, response);
      const mode = args.encoding || 'utf8';
      const text = mode === 'base64' ? data : Buffer.from(data, 'base64').toString('utf8');
      return textResult(text, { ...response, decodedEncoding: mode });
    }
    case 'codex_fs_write': {
      await app.ensureReady();
      const dataBase64 = (args.encoding || 'utf8') === 'base64' ? args.content : Buffer.from(args.content, 'utf8').toString('base64');
      const response = await app.request('fs/writeFile', {
        path: fileUri(args.path),
        dataBase64,
        ...(args.followSymlinks === undefined ? {} : { followSymlinks: args.followSymlinks }),
      });
      return textResult({ ok: true, path: args.path, response }, { ok: true, path: args.path, response });
    }
    case 'codex_fs_list': {
      await app.ensureReady();
      const response = await app.request('fs/readDirectory', { path: fileUri(args.path) });
      return textResult(response, response);
    }
    case 'codex_mcp_status': {
      const response = await mcpStatus(args);
      const data = args.detail === 'full' ? response : summarizeMcpStatus(response);
      return textResult(data, data);
    }
    case 'codex_mcp_call': {
      const response = await callMcp(args.server, args.tool, args.arguments || {});
      return forwardMcpResult(response);
    }
    case 'codex_mcp_oauth_login': {
      await app.ensureReady();
      const response = await app.request('mcpServer/oauth/login', { name: args.name });
      return textResult(response, response);
    }
    case 'codex_cua_docs': {
      const response = await callCuaJs({ code: 'nodeRepl.write(await cua.rewriteDocumentation())', title: 'Read live Codex Computer Use documentation' });
      return forwardMcpResult(response);
    }
    case 'codex_cua_state': {
      const response = await callCuaJs({ code: 'let s = await cua.getState({emit:false}); nodeRepl.write(JSON.stringify(s))', title: 'Read Codex Computer Use state' });
      return forwardMcpResult(response, { compactCuaDocs: true });
    }
    case 'codex_cua_js': {
      const response = await callCuaJs({ code: args.code, ...(args.title ? { title: args.title } : {}), ...(args.timeout_ms ? { timeout_ms: args.timeout_ms } : {}) });
      return forwardMcpResult(response, { compactCuaDocs: true });
    }
    case 'codex_skills_list': {
      await app.ensureReady();
      const response = await app.request('skills/list', { cwds: [WORK_DIR], forceReload: Boolean(args.forceReload) });
      return textResult(response, response);
    }
    case 'codex_plugins_list': {
      await app.ensureReady();
      const response = await app.request('plugin/list', { cwds: null, marketplaceKinds: null, forceRefetch: Boolean(args.forceRefetch) });
      return textResult(response, response);
    }
    case 'codex_apps_list': {
      await app.ensureReady();
      const response = await appsListWithFallback(args);
      return textResult(response, response);
    }
    case 'codex_apps_read': {
      await app.ensureReady();
      const response = await appsReadWithFallback(args);
      return textResult(response, response);
    }
    case 'codex_permissions_list': {
      await app.ensureReady();
      const response = await app.request('permissionProfile/list', {
        cwd: WORK_DIR,
        ...(args.limit ? { limit: args.limit } : {}),
        ...(args.cursor ? { cursor: args.cursor } : {}),
      });
      return textResult(response, response);
    }
    case 'codex_projects_list': {
      await app.ensureReady();
      const response = await app.request('project/list', {
        ...(args.limit ? { limit: args.limit } : {}),
        ...(args.cursor ? { cursor: args.cursor } : {}),
        ...(args.sortKey ? { sortKey: args.sortKey } : {}),
        ...(args.sortDirection ? { sortDirection: args.sortDirection } : {}),
      });
      return textResult(response, response);
    }
    case 'codex_hooks_list': {
      await app.ensureReady();
      const response = await app.request('hooks/list', { cwds: [WORK_DIR] });
      return textResult(response, response);
    }
    case 'codex_models_list': {
      await app.ensureReady();
      const response = await app.request('model/list', {
        ...(args.includeHidden === undefined ? {} : { includeHidden: Boolean(args.includeHidden) }),
        ...(args.limit ? { limit: args.limit } : {}),
        ...(args.cursor ? { cursor: args.cursor } : {}),
      });
      return textResult(response, response);
    }
    case 'codex_account_read': {
      await app.ensureReady();
      const response = await app.request('account/read', {});
      const summary = {
        accountType: response?.account?.type ?? null,
        planType: response?.account?.planType ?? null,
        requiresOpenaiAuth: response?.requiresOpenaiAuth ?? null,
        backendOrigin: response?.workspaceRouting?.backendOrigin ?? null,
        accountRoutingOverride: response?.workspaceRouting?.accountRoutingOverride ?? null,
      };
      return textResult(summary, summary);
    }
    case 'codex_remote_control_status': {
      await app.ensureReady();
      const response = await app.request('remoteControl/status/read', {});
      const summary = {
        status: response?.status ?? null,
        serverName: response?.serverName ?? null,
        environmentAttached: Boolean(response?.environmentId),
      };
      return textResult(summary, summary);
    }
    case 'codex_capabilities': {
      await app.ensureReady();
      const results = {};
      const probes = [
        ['mcp', () => mcpStatus({ detail: 'toolsAndAuthOnly' })],
        ['skills', () => app.request('skills/list', { cwds: [WORK_DIR], forceReload: false })],
        ['plugins', () => app.request('plugin/list', { cwds: null, marketplaceKinds: null, forceRefetch: false })],
        ['apps', () => appsListWithFallback({ source: 'runtime', limit: 200 })],
        ['permissions', () => app.request('permissionProfile/list', { cwd: WORK_DIR })],
        ['projects', () => app.request('project/list', { limit: 200 })],
        ['hooks', () => app.request('hooks/list', { cwds: [WORK_DIR] })],
        ['models', () => app.request('model/list', { limit: 200 })],
        ['remoteControl', () => app.request('remoteControl/status/read', {})],
      ];
      for (const [key, fn] of probes) {
        try {
          const value = await fn();
          results[key] = key === 'mcp' ? summarizeMcpStatus(value) : summarizeNamedCollection(value);
        } catch (e) {
          results[key] = { error: String(e?.message || e) };
        }
      }
      results.bridge = app.diagnostics();
      return textResult(results, results);
    }
    case 'codex_rpc': {
      if (!ALLOW_RAW_RPC && !SAFE_RPC.has(args.method)) {
        throw new Error(`RPC method is not allowlisted: ${args.method}. Set CODEX_APP_BRIDGE_ALLOW_RAW_RPC=1 to allow arbitrary App Server methods.`);
      }
      await app.ensureReady();
      const response = await app.request(args.method, args.params);
      return textResult(response, response);
    }
    default:
      throw new Error(`Unknown tool: ${name}`);
  }
}

async function handleMcpMessage(msg) {
  const id = msg?.id;
  const method = msg?.method;
  if (method === 'initialize') {
    return rpcResult(id, {
      protocolVersion: msg.params?.protocolVersion || '2025-11-25',
      capabilities: { tools: {} },
      serverInfo: { name: 'codex-app-server-bridge', version: VERSION },
      instructions: 'Use Codex App Server as the local execution plane. command/exec and MCP tool dispatch do not start a model turn. Use codex_cua_docs before advanced Chrome/Computer Use automation and codex_cua_state to discover actual browser availability. Native Computer Use can be available while the desktop in-app browser is absent. MCP images are returned as native content blocks.',
    });
  }
  if (method === 'notifications/initialized' || method === 'initialized') return null;
  if (method === 'notifications/cancelled') {
    if (operations.cancelRequest(msg.params?.requestId)) app.child?.kill('SIGTERM');
    return null;
  }
  if (method === 'ping') return rpcResult(id, {});
  if (method === 'tools/list') return rpcResult(id, { tools: TOOLS });
  if (method === 'tools/call') {
    try {
      const name = msg.params?.name;
      const args = msg.params?.arguments || {};
      const owner = msg.params?._meta?.['codex-free/call-owner'] || 'stdio-client';
      if (typeof owner !== 'string' || owner.length > 200) throw new Error('Invalid caller identity.');
      let result;
      if (name === 'codex_confirmations_list') result = operations.list(owner);
      else if (name === 'codex_confirmation_respond') result = await operations.respond(owner, args);
      else if (name === 'codex_operation_result') result = await operations.result(owner, args.operationId, args.waitMs);
      else result = await operations.run(owner, name, () => executeTool(name, args), id);
      return rpcResult(id, result);
    } catch (error) {
      return rpcResult(id, {
        content: [{ type: 'text', text: `${error?.message || error}${error?.rpc ? `\n${JSON.stringify(error.rpc, null, 2)}` : ''}` }],
        isError: true,
      });
    }
  }
  return rpcError(id, -32601, `Method not found: ${method}`);
}

const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on('line', async (line) => {
  if (!line.trim()) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch (error) {
    writeMessage(rpcError(null, -32700, `Parse error: ${error.message}`));
    return;
  }
  const response = await handleMcpMessage(msg);
  if (response) writeMessage(response);
});

function shutdown() {
  operations.shutdown();
  try { app.child?.kill('SIGTERM'); } catch {}
  process.exit(0);
}
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
