import test from 'node:test';
import assert from 'node:assert/strict';
import { ConfirmationOperations } from '../integrations/codex-app-server-bridge/confirmations.mjs';

const form = { mode: 'openai/form', serverName: 'fixture', threadId: 'private-routing-id', message: 'Allow reading the selected app?', requestedSchema: { type: 'object', properties: { allow: { type: 'boolean' } }, required: ['allow'], additionalProperties: false } };
const data = result => result.structuredContent;
function pending(operations, owner = 'caller-a', request = form) {
  return operations.run(owner, 'read-app', () => new Promise(resolve => {
    assert.equal(operations.request(request, answer => resolve({ content: [{ type: 'text', text: answer.action }], isError: answer.action !== 'accept' })), true);
  }));
}

test('forms suspend once, never consent automatically, and preserve the actual decision', async () => {
  const operations = new ConfirmationOperations();
  const receipt = data(await pending(operations));
  assert.equal(receipt.status, 'awaiting_confirmation');
  assert.equal(receipt.confirmation.threadId, undefined);
  assert.equal(receipt.confirmation.requiresHumanDecision, true);
  assert.equal(data(operations.list('caller-a')).operations.length, 1);
  const args = { operationId: receipt.operationId, confirmationId: receipt.confirmation.id, action: 'accept', content: { allow: true } };
  await assert.rejects(operations.respond('caller-a', args), /explicit human/);
  await assert.rejects(operations.respond('caller-a', { ...args, userConfirmed: true, content: {} }), /required/);
  const result = await operations.respond('caller-a', { ...args, userConfirmed: true });
  assert.equal(result.content[0].text, 'accept');
  await assert.rejects(operations.respond('caller-a', { ...args, userConfirmed: true }), /already answered/);
  assert.deepEqual(await operations.result('caller-a', receipt.operationId), result);
});

test('another caller cannot see, answer, or retrieve a confirmation', async () => {
  const operations = new ConfirmationOperations();
  const receipt = data(await pending(operations));
  assert.deepEqual(data(operations.list('caller-b')), { operations: [] });
  await assert.rejects(operations.respond('caller-b', { operationId: receipt.operationId }), /Unknown operation/);
  assert.throws(() => operations.result('caller-b', receipt.operationId), /Unknown operation/);
  await operations.respond('caller-a', { operationId: receipt.operationId, confirmationId: receipt.confirmation.id, action: 'decline', userConfirmed: true });
});

test('decline/cancel are forwarded without invented content and later work remains ordered', async () => {
  const operations = new ConfirmationOperations();
  const receipt = data(await pending(operations));
  let executions = 0;
  const later = operations.run('caller-b', 'other', async () => { executions++; return { content: [] }; });
  assert.equal(executions, 0);
  const result = await operations.respond('caller-a', { operationId: receipt.operationId, confirmationId: receipt.confirmation.id, action: 'cancel', userConfirmed: true });
  assert.equal(result.isError, true);
  await later;
  assert.equal(executions, 1);
});

test('text consent cannot manufacture device-verification proof', async () => {
  const operations = new ConfirmationOperations();
  const receipt = data(await pending(operations, 'caller-a', { mode: 'openai/userVerification', title: 'Verify', description: 'Device proof required', challenge: 'private-challenge' }));
  assert.equal(receipt.confirmation.challenge, undefined);
  await assert.rejects(operations.respond('caller-a', { operationId: receipt.operationId, confirmationId: receipt.confirmation.id, action: 'accept', content: { fake: true }, userConfirmed: true }), /Device verification/);
  operations.shutdown();
});

test('expired confirmations cancel the original operation rather than replaying it', async () => {
  const operations = new ConfirmationOperations({ timeoutMs: 15 });
  const receipt = data(await pending(operations));
  await new Promise(resolve => setTimeout(resolve, 25));
  const result = await operations.result('caller-a', receipt.operationId);
  assert.equal(result.content[0].text, 'cancel');
  await assert.rejects(operations.respond('caller-a', { operationId: receipt.operationId, confirmationId: receipt.confirmation.id, action: 'accept', content: { allow: true }, userConfirmed: true }), /expired|already answered/);
});

test('normal results do not fill the retained-operation cache', async () => {
  const operations = new ConfirmationOperations({ limit: 2 });
  for (let i = 0; i < 5; i++) assert.deepEqual(await operations.run('caller', 'read', async () => ({ content: [] })), { content: [] });
  assert.equal(operations.operations.size, 0);
});

test('an upstream failure while a form is open closes that form and completes the operation', async () => {
  const operations = new ConfirmationOperations();
  let fail;
  let cancelled = false;
  const receipt = data(await operations.run('caller', 'read', () => new Promise((_resolve, reject) => {
    fail = reject;
    operations.request(form, answer => { cancelled = answer.action === 'cancel'; });
  })));
  fail(new Error('upstream disconnected'));
  await new Promise(resolve => setImmediate(resolve));
  const result = await operations.result('caller', receipt.operationId);
  assert.equal(result.isError, true);
  assert.equal(cancelled, true);
  assert.equal(data(operations.list('caller')).operations.length, 0);
});

test('cancelling queued requests prevents execution after another user answers', async () => {
  const operations = new ConfirmationOperations();
  const receipt = data(await pending(operations));
  let executed = false;
  const queued = operations.run('caller-b', 'write', async () => { executed = true; return { content: [] }; }, 42);
  operations.cancelRequest(42);
  assert.equal((await queued).isError, true);
  operations.shutdown();
  await operations.result('caller-a', receipt.operationId);
  assert.equal(executed, false);
});
