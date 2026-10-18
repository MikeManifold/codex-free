import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { forwardMcpResult } from '../integrations/codex-app-server-bridge/mcp-result.mjs';

// An opaque large payload exercises transport loss without relying on image
// rendering; real screenshot decoding is a separate deployment acceptance.
const image = { type: 'image', data: Buffer.alloc(90_000, 42).toString('base64'), mimeType: 'image/png' };

test('large image-only result remains a native image block', () => {
  const result = forwardMcpResult({ content: [image], isError: false });
  assert.deepEqual(result, { content: [image], isError: false });
  assert.equal(result.structuredContent, undefined);
  assert.equal(result.content.some(block => block.type === 'text'), false);
});

test('mixed content, annotations, metadata, and structured data survive', () => {
  const upstream = {
    content: [
      { type: 'text', text: 'Captured the requested view.', annotations: { audience: ['user'] } },
      image,
      { type: 'resource_link', uri: 'fixture://artifact', name: 'artifact' },
    ],
    structuredContent: { captured: true },
    _meta: { source: 'fixture' },
    isError: false,
  };
  assert.deepEqual(forwardMcpResult(upstream), upstream);
});

test('upstream refusal/error remains an error without altering its message', () => {
  const upstream = { content: [{ type: 'text', text: 'App access denied.' }], isError: true };
  assert.deepEqual(forwardMcpResult(upstream), upstream);
});

test('only repeated CUA documentation is compacted in normal calls', () => {
  const docs = { type: 'text', text: '## Computer Use\nFixture documentation.' };
  assert.deepEqual(forwardMcpResult({ content: [docs, image] }, { compactCuaDocs: true }).content, [image]);
  assert.deepEqual(forwardMcpResult({ content: [docs, image] }).content, [docs, image]);
  assert.deepEqual(forwardMcpResult({ content: [docs] }, { compactCuaDocs: true }).content, [docs]);
});

test('malformed upstream result fails explicitly', () => {
  assert.equal(forwardMcpResult(null).isError, true);
  assert.equal(forwardMcpResult({ payload: image }).isError, true);
});

test('stdio dispatch preserves images for generic MCP and every CUA entry', { timeout: 15_000 }, async () => {
  const temporary = await mkdtemp(join(tmpdir(), 'free-bridge-regression-'));
  const fakeCli = join(temporary, 'fixture-cli');
  const fixture = { content: [{ type: 'text', text: 'fixture' }, image], _meta: { source: 'fixture' }, isError: false };
  await writeFile(fakeCli, `#!${process.execPath}
import { createInterface } from 'node:readline';
const fixture = ${JSON.stringify(fixture)};
const input = createInterface({ input: process.stdin });
input.on('line', line => {
  const msg = JSON.parse(line);
  if (msg.id === undefined) return;
  let result;
  switch (msg.method) {
    case 'initialize': result = {}; break;
    case 'thread/start': result = { thread: { id: 'fixture-thread' } }; break;
    case 'mcpServerStatus/list': result = { data: [{ name: 'cua_repl' }] }; break;
    case 'mcpServer/tool/call': result = fixture; break;
    default: process.stdout.write(JSON.stringify({id:msg.id,error:{message:'Unexpected or model-turn RPC: '+msg.method}})+'\\n'); return;
  }
  process.stdout.write(JSON.stringify({ id: msg.id, result })+'\\n');
});
`, { mode: 0o700 });
  const bridge = spawn(process.execPath, [fileURLToPath(new URL('../integrations/codex-app-server-bridge/server.mjs', import.meta.url))], {
    env: { ...process.env, CODEX_APP_CLI: fakeCli },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const pending = new Map();
  const output = createInterface({ input: bridge.stdout });
  output.on('line', line => {
    const message = JSON.parse(line);
    pending.get(message.id)?.(message.result);
    pending.delete(message.id);
  });
  let nextId = 0;
  function request(method, params) {
    const id = ++nextId;
    return new Promise(resolve => {
      pending.set(id, resolve);
      bridge.stdin.write(JSON.stringify({ id, method, params }) + '\n');
    });
  }
  try {
    const init = await request('initialize', {});
    assert.equal(init.serverInfo.version, '0.3.0');
    for (const [name, args] of [
      ['codex_mcp_call', { server: 'fixture', tool: 'capture' }],
      ['codex_cua_docs', {}],
      ['codex_cua_state', {}],
      ['codex_cua_js', { code: 'fixture' }],
    ]) {
      const result = await request('tools/call', { name, arguments: args });
      assert.deepEqual(result, fixture, name);
      assert.equal(result.content[1].data.length, 120_000, name);
      assert.equal(result.structuredContent, undefined, name);
    }
  } finally {
    bridge.kill('SIGTERM');
    output.close();
    await new Promise(resolve => bridge.once('close', resolve));
    await rm(temporary, { recursive: true, force: true });
  }
});
