import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { loadBridgeConfig } from '../integrations/codex-app-server-bridge/config.mjs';

test('personal config expands home and environment overrides stay compatible', () => {
  const temp = mkdtempSync(join(tmpdir(), 'bridge-config-'));
  try {
    const file = join(temp, 'private.local.json');
    writeFileSync(file, JSON.stringify({ version: 1, workDir: './project', codexHome: '~/.codex', requestTimeoutMs: 9000 }));
    const config = loadBridgeConfig({ configFile: file, home: temp, env: { CODEX_APP_CLI: join(temp, 'codex') }, cwd: temp });
    assert.equal(config.workDir, join(temp, 'project'));
    assert.equal(config.codexHome, join(temp, '.codex'));
    assert.equal(config.codexCli, join(temp, 'codex'));
    assert.equal(config.requestTimeoutMs, 9000);
    assert.equal(config.allowRawRpc, false);
    assert.equal(loadBridgeConfig({ configFile: file, env: { CODEX_APP_BRIDGE_WORK_DIR: temp } }).workDir, temp);
  } finally { rmSync(temp, { recursive: true, force: true }); }
});

test('explicit missing files, unknown fields and unsafe timeout values fail closed', () => {
  const temp = mkdtempSync(join(tmpdir(), 'bridge-invalid-'));
  try {
    const file = join(temp, 'private.local.json');
    assert.throws(() => loadBridgeConfig({ env: { CODEX_APP_BRIDGE_CONFIG: file } }), /does not exist/);
    writeFileSync(file, JSON.stringify({ version: 1, unknownKey: true }));
    assert.throws(() => loadBridgeConfig({ configFile: file, env: {} }), /Unknown/);
    writeFileSync(file, JSON.stringify({ version: 1, requestTimeoutMs: -1 }));
    assert.throws(() => loadBridgeConfig({ configFile: file, env: {} }), /timeouts/);
  } finally { rmSync(temp, { recursive: true, force: true }); }
});
