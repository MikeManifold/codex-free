import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

test('stdio confirmation resumes exactly one original call and preserves refusal and images', { timeout: 15000 }, async () => {
  const temporary = await mkdtemp(join(tmpdir(), 'bridge-consent-'));
  const fakeCli = join(temporary, 'fixture-cli.mjs');
  await writeFile(fakeCli, `#!${process.execPath}
import { createInterface } from 'node:readline';
let counter = 0;
const pending = new Map();
const send = value => process.stdout.write(JSON.stringify(value)+'\\n');
createInterface({input:process.stdin}).on('line', line => {
  const msg = JSON.parse(line);
  if (msg.id === undefined) return;
  if (!msg.method) {
    const original = pending.get(msg.id);
    if (!original) throw new Error('Unexpected or duplicate confirmation response');
    pending.delete(msg.id);
    if (msg.result.action !== 'accept') send({id:original.id,error:{code:-32000,message:'User declined App access.'}});
    else if (msg.result.content.approved !== true) throw new Error('Wrong user input');
    else send({id:original.id,result:{content:[{type:'image',data:'Zml4dHVyZQ==',mimeType:'image/png'}],structuredContent:{executions:counter}}});
    return;
  }
  if (msg.method === 'initialize') {
    if (!msg.params.capabilities.extensions['openai/form'] || !msg.params.capabilities.mcpServerOpenaiFormElicitation) throw new Error('Form support not advertised');
    send({id:msg.id,result:{}});
  } else if (msg.method === 'thread/start') send({id:msg.id,result:{thread:{id:'fixture-thread'}}});
  else if (msg.method === 'app/read' || msg.method === 'mcpServer/tool/call') {
    const id = 'form-'+(++counter);
    pending.set(id,msg);
    send({id,method:'mcpServer/elicitation/request',params:{mode:'openai/form',threadId:'fixture-thread',turnId:'fixture-turn',serverName:'fixture',message:'Allow this fixture read?',requestedSchema:{type:'object',required:['approved'],properties:{approved:{type:'boolean'}}}}});
  } else if (msg.method === 'app/list') send({id:msg.id,error:{code:-32601,message:'Method not found'}});
  else if (msg.method === 'mcpServerStatus/list') send({id:msg.id,result:{data:[{name:'codex_apps',tools:{}}]}});
  else throw new Error('Unexpected or model-turn RPC: '+msg.method);
});
`, { mode: 0o700 });
  const bridge = spawn(process.execPath, [fileURLToPath(new URL('../integrations/codex-app-server-bridge/server.mjs', import.meta.url))], {
    env: { ...process.env, CODEX_APP_CLI: fakeCli, CODEX_APP_BRIDGE_WORK_DIR: temporary },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let stderr = '';
  bridge.stderr.on('data', chunk => { stderr += chunk; });
  const pending = new Map();
  const output = createInterface({ input: bridge.stdout });
  output.on('line', line => {
    const message = JSON.parse(line);
    pending.get(message.id)?.(message.result);
    pending.delete(message.id);
  });
  let sequence = 0;
  const rpc = (method, params) => new Promise(resolve => {
    const id = ++sequence;
    pending.set(id, resolve);
    bridge.stdin.write(JSON.stringify({id,method,params})+'\n');
  });
  const call = (name, args = {}, owner = 'fixture-a') => rpc('tools/call', {name,arguments:args,_meta:{'codex-free/call-owner':owner}});
  try {
    await rpc('initialize', {});
    const first = (await call('codex_apps_read', {appIds:['fixture'],source:'directory'})).structuredContent;
    assert.equal(first.status, 'awaiting_confirmation');
    assert.equal(first.confirmation.mode, 'openai/form');
    assert.equal(first.confirmation.threadId, undefined);
    assert.equal((await call('codex_confirmations_list', {}, 'fixture-b')).structuredContent.operations.length, 0);
    const response = {operationId:first.operationId,confirmationId:first.confirmation.id,action:'decline',userConfirmed:true};
    assert.equal((await call('codex_confirmation_respond', response, 'fixture-b')).isError, true);
    assert.equal((await call('codex_confirmation_respond', {...response,userConfirmed:false})).isError, true);
    const refused = await call('codex_confirmation_respond', response);
    assert.equal(refused.isError, true);
    assert.match(refused.content[0].text, /declined/);
    assert.equal((await call('codex_confirmation_respond', response)).isError, true);
    const second = (await call('codex_mcp_call', {server:'fixture',tool:'read'})).structuredContent;
    const accepted = await call('codex_confirmation_respond', {
      operationId:second.operationId,confirmationId:second.confirmation.id,action:'accept',content:{approved:true},userConfirmed:true,
    });
    assert.equal(accepted.content[0].type, 'image');
    assert.equal(accepted.structuredContent.executions, 2);
    assert.deepEqual(await call('codex_operation_result', {operationId:second.operationId}), accepted);
    const fallback = await call('codex_apps_list', {source:'directory'});
    assert.equal(fallback.structuredContent.fallback, true);
    assert.equal(stderr, '');
  } finally {
    bridge.kill('SIGTERM');
    output.close();
    await new Promise(resolve => bridge.once('close', resolve));
    await rm(temporary, {recursive:true,force:true});
  }
});
