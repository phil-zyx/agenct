#!/usr/bin/env node
'use strict';
// Real Hub processes and SQLite, deterministic loopback transport, no provider.
// Python's standard sqlite3 module reads persisted evidence without mutating it.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const { createHash, randomBytes } = require('node:crypto');
const { spawn, execFileSync } = require('node:child_process');
const { Fixture } = require('./three-person-acceptance.cjs');
const root = path.resolve(__dirname, '..');
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
async function eventually(label, fn, timeout = 15000) {
  const until = Date.now() + timeout;
  do { if (await fn()) return; await pause(75); } while (Date.now() < until);
  throw new Error(`Timed out: ${label}`);
}
async function listen(server) {
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  return server.address().port;
}
async function close(server) {
  const done = new Promise(resolve => server.close(resolve));
  server.closeAllConnections();
  await done;
}
async function freePort() { const s = http.createServer(); const p = await listen(s); await close(s); return p; }
class Adapter {
  constructor() {
    this.token = randomBytes(32).toString('hex');
    this.calls = [];
    this.holdChat = true;
    this.holdDraft = false;
    this.server = http.createServer((req, res) => {
      if (req.url !== '/v1/turn' || req.method !== 'POST' || req.headers.authorization !== `Bearer ${this.token}`) {
        res.writeHead(403); res.end(); return;
      }
      let body = '';
      req.on('data', chunk => { body += chunk; });
      req.on('end', () => {
        let value;
        try { value = JSON.parse(body); } catch { res.writeHead(400); res.end(); return; }
        // Legacy chat transport deliberately omits kind.
        if (value.kind === undefined) value.kind = 'chat';
        const call = { value, res };
        this.calls.push(call);
        if (!(value.kind === 'chat' && this.holdChat) && !(value.kind === 'draft_delivery' && this.holdDraft)) this.reply(call);
      });
    });
  }
  reply(call) {
    if (call.res.destroyed || call.res.writableEnded) return;
    call.res.writeHead(200, { 'content-type': 'application/json' });
    call.res.end(JSON.stringify({ backend: 'dsh', reply: 'PRIVATE-FAKE-DRAFT: owner review required.', proposal: null }));
  }
  count(kind) { return this.calls.filter(c => c.value.kind === kind).length; }
  async start() { this.url = `http://127.0.0.1:${await listen(this.server)}/v1/turn`; }
  async stop() { await close(this.server); }
}
class DurableFixture extends Fixture {
  constructor(binary, port, adapter) { super(binary, port); this.adapter = adapter; this.enabled = true; this.logs = ''; }
  environment() {
    // No live HOME/config, credential variables, proxy, or AGENCT_* inheritance.
    return { PATH: process.env.PATH, HOME: this.directory, TMPDIR: this.directory, LANG: 'C.UTF-8',
      AGENCT_BIND: `127.0.0.1:${this.port}`, AGENCT_DATABASE: path.join(this.directory, 'hub.sqlite3'),
      AGENCT_RUNTIME: 'dsh', AGENCT_ADAPTER_URL: this.adapter.url, AGENCT_ADAPTER_TOKEN: this.adapter.token,
      AGENCT_AUTO_DRAFT: this.enabled ? '1' : '0',
      ...(this.privateConfig ? { AGENCT_PRIVATE_CONFIG: this.privateConfig } : {}), AGENCT_USERS_JSON: JSON.stringify(Object.entries(this.tokens).map(([id, token]) => ({
        id, token_sha256: createHash('sha256').update(token).digest('hex') }))) };
  }
  async start() {
    this.child = spawn(this.binary, [], { cwd: this.directory, env: this.environment(), stdio: ['ignore', 'ignore', 'pipe'] });
    this.child.stderr.on('data', data => { this.logs = (this.logs + data.toString()).slice(-12000); });
    let failure;
    this.child.once('error', error => { failure = error; });
    await eventually('temporary Hub startup', async () => {
      if (failure) throw failure;
      assert(this.child.exitCode === null && this.child.signalCode === null, `Hub exited: ${this.logs}`);
      try { return (await this.api('alice', '/api/me')).status === 200; } catch { return false; }
    });
  }
  writeRoster(includeBob) {
    this.privateConfig = path.join(this.directory, 'private-config.json');
    const users = Object.entries(this.tokens).filter(([id]) => includeBob || id !== 'bob').map(([id, token]) => ({
      id, token_sha256: createHash('sha256').update(token).digest('hex') }));
    fs.writeFileSync(this.privateConfig, JSON.stringify({ version: 1, port: this.port, runtime: 'dsh', users }), { mode: 0o600 });
    fs.chmodSync(this.privateConfig, 0o600);
  }
  rows(sql, params = []) {
    return JSON.parse(execFileSync('python3', ['-c',
      'import sqlite3,json,sys\nc=sqlite3.connect("file:"+sys.argv[1]+"?mode=ro",uri=True)\nc.row_factory=sqlite3.Row\nprint(json.dumps([dict(r) for r in c.execute(sys.argv[2],json.loads(sys.argv[3]))]))',
      path.join(this.directory, 'hub.sqlite3'), sql, JSON.stringify(params)], { encoding: 'utf8' }));
  }
  intent(task) { return this.rows('SELECT * FROM auto_draft_intents WHERE owner=? AND task_id=?', ['bob', task])[0]; }
  async queuedTask(label) {
    const p = await this.ok('alice', '/api/proposals', 'POST', { request_id: `propose-${label}`, owner: 'bob', summary: `Review ${label}` });
    const confirmed = await this.ok('alice', `/api/proposals/${p.id}/actions`, 'POST', { action: 'confirm', expected_revision: 0, details: 'Explicitly shared fixture scope' });
    const task = confirmed.task_id;
    const accepted = await this.ok('bob', `/api/tasks/${task}/actions`, 'POST', { action: 'accept', expected_revision: 0 });
    assert.equal(accepted.auto_draft.request_id, `auto-${task}-r1`);
    await eventually('persisted busy queued attempt', () => { const row = this.intent(task); return row?.status === 'queued' && row.attempts >= 1 && row.reason === 'busy'; });
    const row = this.intent(task);
    assert.equal(row.request_id, accepted.auto_draft.request_id);
    assert.equal(row.task_revision, 1);
    assert(row.created_at != null && row.next_attempt_at != null);
    assert.equal((await this.ok('bob', '/api/tasks')).find(t => t.id === task).my_draft.status, 'queued');
    return task;
  }
  async holdOwner() {
    const previous = this.adapter.count('chat');
    // A rejection on SIGKILL is expected; attach the handler immediately.
    this.chat = this.api('bob', '/api/messages', 'POST', { request_id: `hold-${randomBytes(8).toString('hex')}`, message: 'Hold this private chat turn.' }).catch(() => null);
    await eventually('owner chat reached adapter and holds runtime gate', () => this.adapter.count('chat') === previous + 1);
  }
  async isolation(task) {
    const requester = (await this.ok('alice', '/api/tasks')).find(t => t.id === task);
    assert.equal(requester.my_draft, undefined);
    assert(!JSON.stringify(requester).includes('auto-'));
    assert.deepEqual(await this.ok('eve', '/api/tasks'), []);
    await this.expect(404, 'eve', `/api/tasks/${task}`);
    for (const who of ['alice', 'eve']) {
      await this.expect(404, who, `/api/tasks/${task}/drafts`);
      await this.expect(404, who, `/api/tasks/${task}/drafts/turns/auto-${task}-r1`);
      assert(!JSON.stringify(await this.ok(who, `/api/tasks`)).includes('PRIVATE-FAKE-DRAFT'));
    }
  }
}
async function scenario(binary, label, fn) {
  const adapter = new Adapter();
  await adapter.start();
  const f = new DurableFixture(binary, await freePort(), adapter);
  try { await f.start(); await fn(f, adapter); console.log(`PASS [durable-dispatch] ${label}`); }
  catch (error) { error.message += `\nTemporary Hub diagnostics:\n${f.logs}`; throw error; }
  finally { await f.dispose(); await adapter.stop(); await f.chat; }
}
async function acceptance(binary) {
  await scenario(binary, 'queued acceptance survives SIGKILL and drafts exactly once', async (f, a) => {
    await f.holdOwner(); const task = await f.queuedTask('restart-queued');
    await f.isolation(task);
    const queued = f.intent(task);
    assert.equal(a.count('draft_delivery'), 0);
    await f.stop('SIGKILL');
    assert.deepEqual(f.intent(task), queued, 'queue evidence must survive process death unchanged');
    await f.start();
    await eventually('recovered queue completes', async () => (await f.ok('bob', '/api/tasks')).find(t => t.id === task).my_draft?.status === 'complete');
    assert.equal(a.count('draft_delivery'), 1);
    assert.equal(f.intent(task).status, 'reserved');
    await f.isolation(task);
    await f.restart(); await pause(3500);
    assert.equal(a.count('draft_delivery'), 1, 'completed reservation must not replay');
    const detail = await f.ok('alice', `/api/tasks/${task}`);
    assert.equal(detail.task.status, 'working'); assert.equal(detail.task.result, null); assert.equal(detail.deliveries.length, 0);
  });
  await scenario(binary, 'pending reservation becomes uncertain on restart without redispatch', async (f, a) => {
    a.holdDraft = true;
    await f.holdOwner(); const task = await f.queuedTask('restart-pending');
    await f.restart();
    await eventually('draft reached fake adapter', () => a.count('draft_delivery') === 1);
    const receipt = `/api/tasks/${task}/drafts/turns/auto-${task}-r1`;
    assert.equal((await f.ok('bob', receipt)).status, 'pending');
    assert.equal(f.intent(task).status, 'reserved');
    await f.restart();
    assert.equal((await f.ok('bob', receipt)).status, 'uncertain');
    await pause(3500); assert.equal(a.count('draft_delivery'), 1);
    await f.isolation(task);
  });
  await scenario(binary, 'private roster removal skips queued intent permanently', async (f, a) => {
    f.writeRoster(true);
    await f.restart();
    await f.holdOwner(); const task = await f.queuedTask('roster-removal');
    await f.stop('SIGKILL');
    f.writeRoster(false);
    await f.start();
    await f.expect(401, 'bob', '/api/me');
    await eventually('removed owner intent skipped', () => f.intent(task).status === 'skipped');
    await pause(3500);
    assert.equal(a.count('draft_delivery'), 0);
    assert.equal(f.rows('SELECT * FROM draft_turns WHERE task_id=?', [task]).length, 0);
    await f.isolation(task);
    await f.stop('SIGKILL');
    f.writeRoster(true);
    await f.start();
    await f.ok('bob', '/api/me');
    await pause(3500);
    assert.equal(f.intent(task).status, 'skipped');
    assert.equal(a.count('draft_delivery'), 0, 'restoring membership must not resurrect skipped intent');
  });
  for (const mode of ['owner-optout', 'global-disable', 'cancel']) {
    await scenario(binary, `${mode} invalidates queued work without a model call`, async (f, a) => {
      await f.holdOwner(); const task = await f.queuedTask(mode);
      if (mode === 'owner-optout') {
        const p = await f.ok('bob', '/api/collaboration-profile');
        await f.ok('bob', '/api/collaboration-profile', 'PUT', { expected_revision: p.revision,
          public: p.public, private: p.private, permissions: { ...p.permissions, auto_prepare_drafts: false } });
      } else if (mode === 'cancel') {
        await f.ok('alice', `/api/tasks/${task}/actions`, 'POST', { action: 'cancel', expected_revision: 1, reason: 'Fixture no longer needs this task.' });
      } else { f.enabled = false; }
      await f.restart();
      await eventually('invalidated intent is skipped', () => f.intent(task).status === 'skipped');
      await pause(3500);
      assert.equal(a.count('draft_delivery'), 0);
      assert.equal(f.intent(task).status, 'skipped');
      assert.equal(f.rows('SELECT * FROM draft_turns WHERE task_id=?', [task]).length, 0);
      await f.isolation(task);
      // Re-enabling permission must not resurrect terminal skipped intent.
      if (mode === 'global-disable') f.enabled = true;
      if (mode === 'owner-optout') {
        const p = await f.ok('bob', '/api/collaboration-profile');
        await f.ok('bob', '/api/collaboration-profile', 'PUT', { expected_revision: p.revision,
          public: p.public, private: p.private, permissions: { ...p.permissions, auto_prepare_drafts: true } });
      }
      await f.restart(); await pause(1800); assert.equal(a.count('draft_delivery'), 0);
    });
  }
}
async function selfTest() {
  const adapter = new Adapter(); await adapter.start();
  const f = new DurableFixture('/unused', 1, adapter);
  try {
    const env = f.environment();
    assert.equal(env.HOME, f.directory); assert.equal(env.AGENCT_ADAPTER_URL, adapter.url);
    assert.equal(env.OPENAI_API_KEY, undefined); assert.equal(env.HTTPS_PROXY, undefined);
    const rejected = await fetch(adapter.url, { method: 'POST', body: '{}' }); assert.equal(rejected.status, 403);
    const request = kind => fetch(adapter.url, { method: 'POST', headers: { authorization: `Bearer ${adapter.token}` }, body: JSON.stringify(kind === 'chat' ? { message: 'held chat' } : { kind }) });
    const held = request('chat');
    await eventually('self-test held chat', () => adapter.count('chat') === 1);
    assert(!adapter.calls[0].res.writableEnded); adapter.reply(adapter.calls[0]);
    assert.equal((await (await held).json()).backend, 'dsh');
    assert.equal((await (await request('draft_delivery')).json()).proposal, null);
    assert.equal(adapter.count('draft_delivery'), 1);
    console.log('PASS harness self-test (not evidence of Hub behavior)');
  } finally { await f.dispose(); await adapter.stop(); }
}
async function main() {
  if (process.argv[2] === '--help') { console.log('Usage: node scripts/durable-draft-acceptance.cjs [--self-test]\nSet AGENCT_TEST_HUB_BINARY to the Hub build. Requires Node 22 and Python 3; no providers or dependencies.'); return; }
  if (process.argv[2] === '--self-test') return selfTest();
  assert.equal(process.argv.length, 2, 'Unknown argument; use --help');
  const binary = path.resolve(process.env.AGENCT_TEST_HUB_BINARY || path.join(root, 'hub/target/debug/agenct-hub'));
  fs.accessSync(binary, fs.constants.X_OK);
  await acceptance(binary);
  console.log('DURABLE DISPATCH ACCEPTANCE PASSED. Fake adapter only; no live-model or UI claims.');
}
if (require.main === module) main().catch(error => { console.error(error.stack); process.exitCode = 1; });
module.exports = { Adapter, DurableFixture, acceptance, selfTest };
