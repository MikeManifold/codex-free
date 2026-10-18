import { randomBytes } from 'node:crypto';

function receipt(data) {
  return { content: [{ type: 'text', text: JSON.stringify(data) }], structuredContent: data };
}

function validate(value, schema, path = 'content') {
  if (!schema || typeof schema !== 'object') return;
  if (schema.enum && !schema.enum.some(item => JSON.stringify(item) === JSON.stringify(value))) throw new Error(`${path} is not an allowed option.`);
  if (schema.const !== undefined && JSON.stringify(schema.const) !== JSON.stringify(value)) throw new Error(`${path} does not match the required option.`);
  const alternatives = schema.oneOf || schema.anyOf;
  if (alternatives && !alternatives.some(option => { try { validate(value, option, path); return true; } catch { return false; } })) throw new Error(`${path} does not match any allowed choice.`);
  if (schema.type === 'object') {
    if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error(`${path} must be an object.`);
    for (const key of schema.required || []) if (!(key in value)) throw new Error(`${path}.${key} is required.`);
    for (const [key, item] of Object.entries(value)) {
      if (schema.additionalProperties === false && !(key in (schema.properties || {}))) throw new Error(`${path}.${key} is not allowed.`);
      validate(item, schema.properties?.[key], `${path}.${key}`);
    }
  } else if (schema.type === 'array') {
    if (!Array.isArray(value)) throw new Error(`${path} must be an array.`);
    if (value.length < (schema.minItems ?? 0) || value.length > (schema.maxItems ?? Infinity)) throw new Error(`${path} has an invalid number of items.`);
    for (const item of value) validate(item, schema.items, path);
  } else if (schema.type === 'string') {
    if (typeof value !== 'string') throw new Error(`${path} must be a string.`);
    if (value.length < (schema.minLength ?? 0) || value.length > (schema.maxLength ?? Infinity)) throw new Error(`${path} has an invalid length.`);
  } else if (schema.type === 'boolean' && typeof value !== 'boolean') throw new Error(`${path} must be a boolean.`);
  else if (['integer', 'number'].includes(schema.type)) {
    if (typeof value !== 'number' || !Number.isFinite(value) || (schema.type === 'integer' && !Number.isInteger(value)) || value < (schema.minimum ?? -Infinity) || value > (schema.maximum ?? Infinity)) throw new Error(`${path} must be a valid ${schema.type}.`);
  }
}

/** One serialized App Server operation owns each confirmation. No automatic consent. */
export class ConfirmationOperations {
  constructor({ timeoutMs = 300000, limit = 64 } = {}) {
    this.timeoutMs = timeoutMs;
    this.limit = limit;
    this.operations = new Map();
    this.tail = Promise.resolve();
    this.active = null;
  }

  async run(owner, tool, work, requestId) {
    this.sweep();
    if (this.operations.size >= this.limit) throw new Error('Too many bridge operations; retrieve or cancel pending operations first.');
    const operation = {
      id: randomBytes(16).toString('hex'), owner, tool, requestId, state: 'queued', created: Date.now(),
      result: null, confirmation: null, waiters: new Set(),
    };
    this.operations.set(operation.id, operation);
    const execute = async () => {
      if (operation.cancelled) return;
      this.active = operation;
      operation.state = 'running';
      try { operation.result = await work(); }
      catch (error) { operation.result = { content: [{ type: 'text', text: String(error?.message || error) }], isError: true }; }
      finally {
        if (operation.confirmation) this.cancelConfirmation(operation);
        operation.state = 'completed';
        operation.finished = Date.now();
        this.active = null;
        this.wake(operation);
      }
    };
    this.tail = this.tail.then(execute, execute);
    return this.observe(operation);
  }

  wake(operation) { for (const resolve of operation.waiters) resolve(); operation.waiters.clear(); }

  async observe(operation, waitMs) {
    if (!['completed', 'awaiting_confirmation'].includes(operation.state)) {
      await new Promise(resolve => {
        let timer;
        const ready = () => { clearTimeout(timer); operation.waiters.delete(ready); resolve(); };
        operation.waiters.add(ready);
        if (waitMs !== undefined) timer = setTimeout(ready, waitMs);
      });
    }
    if (operation.state === 'completed') {
      if (!operation.exposed) this.operations.delete(operation.id);
      return operation.result;
    }
    operation.exposed = true;
    return receipt({
      status: operation.state,
      operationId: operation.id,
      tool: operation.tool,
      ...(operation.confirmation ? { confirmation: this.publicConfirmation(operation.confirmation) } : {}),
      instruction: 'Do not repeat the original call. Display the confirmation to the human user, obtain their explicit decision and input, then call codex_confirmation_respond. Use codex_operation_result to retrieve completion. Never infer consent from a repair request.',
    });
  }

  publicConfirmation(confirmation) {
    const { id, request, expiresAt } = confirmation;
    // App Server routing/session IDs and private proof challenges are not UI fields.
    const { threadId, turnId, challenge, ...form } = request;
    return { id, expiresAt, ...form, requiresHumanDecision: true };
  }

  request(params, respond) {
    const operation = this.active;
    if (!operation || operation.confirmation || !['form', 'openai/form', 'openaiForm', 'url', 'openai/userVerification'].includes(params?.mode)) return false;
    const confirmation = {
      id: randomBytes(16).toString('hex'), request: structuredClone(params), respond,
      expiresAt: Date.now() + this.timeoutMs,
    };
    confirmation.timer = setTimeout(() => this.cancelConfirmation(operation), this.timeoutMs);
    confirmation.timer.unref?.();
    operation.confirmation = confirmation;
    operation.state = 'awaiting_confirmation';
    this.wake(operation);
    return true;
  }

  cancelConfirmation(operation) {
    const confirmation = operation.confirmation;
    if (!confirmation) return;
    clearTimeout(confirmation.timer);
    operation.confirmation = null;
    operation.state = 'running';
    confirmation.respond({ action: 'cancel' });
    this.wake(operation);
  }

  owned(owner, id) {
    const operation = this.operations.get(id);
    if (!operation || operation.owner !== owner) throw new Error('Unknown operation for this caller.');
    return operation;
  }

  list(owner) {
    this.sweep();
    return receipt({ operations: [...this.operations.values()].filter(op => op.owner === owner && op.state !== 'completed').map(op => ({
      operationId: op.id, tool: op.tool, status: op.state,
      ...(op.confirmation ? { confirmation: this.publicConfirmation(op.confirmation) } : {}),
    })) });
  }

  async respond(owner, { operationId, confirmationId, action, content, userConfirmed }) {
    const operation = this.owned(owner, operationId);
    const confirmation = operation.confirmation;
    if (!confirmation || confirmation.id !== confirmationId || Date.now() >= confirmation.expiresAt) throw new Error('This confirmation is expired, already answered, or does not belong to the operation.');
    if (!['accept', 'decline', 'cancel'].includes(action)) throw new Error('action must be accept, decline, or cancel.');
    if (userConfirmed !== true) throw new Error('An explicit human decision is required before replying to a confirmation.');
    if (action === 'accept') {
      if (confirmation.request.mode === 'openai/userVerification') throw new Error('Device verification must be completed in the supported Codex UI; a textual answer cannot replace device proof.');
      if (confirmation.request.mode !== 'url') {
        if (content === undefined) throw new Error('Accepted forms require the actual user-provided content.');
        validate(content, confirmation.request.requestedSchema);
        if (JSON.stringify(content).length > 65536) throw new Error('Confirmation input is too large.');
      }
    } else if (content !== undefined) throw new Error('Decline/cancel must not contain form content.');
    clearTimeout(confirmation.timer);
    operation.confirmation = null;
    operation.state = 'running';
    confirmation.respond({ action, ...(action === 'accept' && content !== undefined ? { content } : {}) });
    return this.observe(operation, 30000);
  }

  result(owner, id, waitMs = 1000) {
    if (!Number.isInteger(waitMs) || waitMs < 0 || waitMs > 30000) throw new Error('waitMs must be between 0 and 30000.');
    return this.observe(this.owned(owner, id), waitMs);
  }

  sweep() {
    for (const [id, op] of this.operations) if (op.finished && Date.now() - op.finished > this.timeoutMs) this.operations.delete(id);
  }

  shutdown() {
    for (const operation of this.operations.values()) this.cancelConfirmation(operation);
  }

  cancelRequest(requestId) {
    let active = false;
    for (const operation of this.operations.values()) {
      if (operation.requestId !== requestId || operation.state === 'completed') continue;
      operation.cancelled = true;
      this.cancelConfirmation(operation);
      if (this.active === operation) active = true;
      else {
        operation.state = 'completed'; operation.finished = Date.now();
        operation.result = { content: [{ type: 'text', text: 'Operation cancelled before execution.' }], isError: true };
        this.wake(operation);
      }
    }
    return active;
  }
}
