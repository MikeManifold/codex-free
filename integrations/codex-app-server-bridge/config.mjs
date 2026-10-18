import { readFileSync, existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, isAbsolute, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const defaultFile = fileURLToPath(new URL('./bridge.config.local.json', import.meta.url));
const keys = new Set(['version', 'codexCli', 'workDir', 'codexHome', 'requestTimeoutMs', 'confirmationTimeoutMs', 'allowRawRpc']);

export function loadBridgeConfig({ env = process.env, configFile = defaultFile, home = homedir(), cwd = process.cwd() } = {}) {
  const selected = env.CODEX_APP_BRIDGE_CONFIG || configFile;
  let data = {};
  if (existsSync(selected)) {
    data = JSON.parse(readFileSync(selected, 'utf8'));
    if (!data || Array.isArray(data) || typeof data !== 'object' || data.version !== 1) {
      throw new Error('Bridge configuration must be an object with version=1.');
    }
    for (const key of Object.keys(data)) if (!keys.has(key)) throw new Error(`Unknown bridge configuration key: ${key}`);
  } else if (env.CODEX_APP_BRIDGE_CONFIG) {
    throw new Error('The explicitly selected bridge configuration does not exist.');
  }
  function path(value, base = cwd) {
    if (typeof value !== 'string' || !value.trim() || value.includes('\0')) throw new Error('Bridge paths must be nonempty strings.');
    const expanded = value === '~' ? home : value.startsWith('~/') ? resolve(home, value.slice(2)) : value;
    return isAbsolute(expanded) ? expanded : resolve(base, expanded);
  }
  function timeout(value, fallback) {
    const number = Number(value ?? fallback);
    if (!Number.isInteger(number) || number < 1000 || number > 600000) throw new Error('Bridge timeouts must be integer milliseconds between 1000 and 600000.');
    return number;
  }
  if (data.allowRawRpc !== undefined && typeof data.allowRawRpc !== 'boolean') throw new Error('allowRawRpc must be a boolean.');
  const base = dirname(resolve(selected));
  return {
    codexCli: path(env.CODEX_APP_CLI || data.codexCli || '/Applications/Codex.app/Contents/Resources/codex-cli/bin/codex', base),
    workDir: path(env.CODEX_APP_BRIDGE_WORK_DIR || data.workDir || cwd, base),
    codexHome: path(env.CODEX_HOME || data.codexHome || '~/.codex', base),
    requestTimeoutMs: timeout(env.CODEX_APP_BRIDGE_TIMEOUT_MS ?? data.requestTimeoutMs, 120000),
    confirmationTimeoutMs: timeout(data.confirmationTimeoutMs, 300000),
    allowRawRpc: env.CODEX_APP_BRIDGE_ALLOW_RAW_RPC === undefined ? (data.allowRawRpc ?? false) : env.CODEX_APP_BRIDGE_ALLOW_RAW_RPC === '1',
  };
}
