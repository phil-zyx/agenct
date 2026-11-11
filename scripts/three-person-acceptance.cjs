#!/usr/bin/env node
'use strict';
// Real loopback HTTP + temporary SQLite + explicit Mock runtime. No provider calls.
// This checks prerequisites, not a shared-project UI or three-device trial.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { createHash, randomBytes } = require('node:crypto');
const { spawn } = require('node:child_process');
const { once } = require('node:events');

const root = path.resolve(__dirname, '..');
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
const goal = 'SHARED-GOAL: prepare a reviewed release note';
const participants = ['alice', 'bob', 'carol'];
const privateMarker = who => `PRIVATE-${who}-must-not-be-shared`;
const pass = name => console.log(`PASS [prerequisite] ${name}`);

async function listen(server) {
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  return server.address().port;
}
async function close(server) {
  const closed = new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
  server.closeAllConnections();
  await closed;
}
async function freePort() {
  const server = http.createServer();
  const port = await listen(server);
  await close(server);
  return port;
}

class Fixture {
  constructor(binary, port) {
    this.binary = binary;
    this.port = port;
    this.origin = `http://127.0.0.1:${port}`;
    this.directory = fs.mkdtempSync(path.join(os.tmpdir(), 'agenct-three-person-'));
    this.tokens = Object.fromEntries([...participants, 'eve'].map(who => [who, randomBytes(32).toString('hex')]));
  }
  async start() {
    const users = Object.entries(this.tokens).map(([id, token]) => ({ id,
      token_sha256: createHash('sha256').update(token).digest('hex') }));
    // Deliberately do not inherit AGENCT_* variables or credentials from a live service.
    this.child = spawn(this.binary, [], { cwd: root, stdio: 'ignore', env: {
      PATH: process.env.PATH, HOME: process.env.HOME, LANG: 'C.UTF-8',
      AGENCT_BIND: `127.0.0.1:${this.port}`, AGENCT_DATABASE: path.join(this.directory, 'hub.sqlite3'),
      AGENCT_RUNTIME: 'mock', AGENCT_AUTO_DRAFT: '0', AGENCT_USERS_JSON: JSON.stringify(users)
    } });
    let failure;
    this.child.once('error', error => { failure = error; });
    for (let attempt = 0; attempt < 200; attempt++) {
      if (failure) throw failure;
      assert.equal(this.child.exitCode, null, 'Temporary Hub exited before becoming ready');
      try {
        const response = await this.api('alice', '/api/me');
        if (response.status === 200) {
          assert.equal(response.value.user, 'alice');
          assert.equal(response.value.runtime, 'mock');
          return;
        }
      } catch {}
      await pause(50);
    }
    throw new Error('Temporary mock Hub did not become ready');
  }
  async stop(signal = 'SIGINT') {
    const child = this.child;
    if (!child || !child.pid) { this.child = null; return; }
    if (child.exitCode === null && child.signalCode === null) {
      const exited = once(child, 'exit');
      const timer = setTimeout(() => child.kill('SIGKILL'), 5000);
      child.kill(signal);
      try { await exited; } finally { clearTimeout(timer); }
    }
    this.child = null;
  }
  async restart() { await this.stop('SIGKILL'); await this.start(); }
  async dispose() { await this.stop(); fs.rmSync(this.directory, { recursive: true, force: true }); }
  async api(who, route, method = 'GET', body, origin = this.origin) {
    const response = await fetch(origin + route, { method,
      headers: { authorization: `Bearer ${this.tokens[who] || 'invalid-test-token'}`, 'content-type': 'application/json' },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(10000) });
    const text = await response.text();
    let value = null;
    try { value = text ? JSON.parse(text) : null; } catch { value = { invalid_json: true }; }
    return { status: response.status, value };
  }
  async expect(status, who, route, method = 'GET', body) {
    const response = await this.api(who, route, method, body);
    assert.equal(response.status, status, `${method} ${route} as ${who}: expected ${status}, received ${response.status}`);
    return response.value;
  }
  async ok(who, route, method = 'GET', body) { return this.expect(200, who, route, method, body); }
  // A real HTTP proxy cuts the connection either before forwarding the write,
  // or after the Hub has returned success. Never treat a failed fetch as a write failure.
  async loseResponse(who, route, body, afterCommit) {
    let forwarded = false, upstreamStatus;
    const proxy = http.createServer((incoming, outgoing) => {
      if (!afterCommit) { incoming.resume(); outgoing.destroy(); return; }
      forwarded = true;
      const upstream = http.request(this.origin + incoming.url, { method: incoming.method,
        headers: { ...incoming.headers, host: `127.0.0.1:${this.port}` } }, response => {
        upstreamStatus = response.statusCode;
        response.resume();
        response.once('end', () => outgoing.destroy());
      });
      upstream.once('error', () => outgoing.destroy());
      incoming.pipe(upstream);
    });
    const port = await listen(proxy);
    try {
      await assert.rejects(() => this.api(who, route, 'POST', body, `http://127.0.0.1:${port}`));
      assert.equal(forwarded, afterCommit);
      if (afterCommit) assert.equal(upstreamStatus, 200, 'Cut must happen after a successful Hub response');
    } finally { await close(proxy); }
  }
}

async function prerequisites(f) {
  for (const who of participants) {
    assert.equal((await f.ok(who, '/api/me')).user, who);
    const profile = await f.ok(who, '/api/collaboration-profile');
    await f.ok(who, '/api/collaboration-profile', 'PUT', { expected_revision: profile.revision,
      public: { team: 'acceptance fixture', responsibilities: who, help_with: 'release note', request_requirements: 'shared scope' },
      private: { work_context: privateMarker(who), response_preferences: `PRIVATE-PREFERENCE-${who}` },
      permissions: { auto_reply_public: false, auto_prepare_drafts: false } });
    await f.ok(who, '/api/messages', 'POST', { request_id: `private-${who}`, message: privateMarker(who) });
    for (const reader of [...participants, 'eve'].filter(reader => reader !== who)) {
      assert(!JSON.stringify(await f.ok(reader, `/api/contacts/${who}/profile`)).includes('PRIVATE-'));
      assert(!JSON.stringify(await f.ok(reader, '/api/messages')).includes(privateMarker(who)));
    }
  }
  await f.expect(401, 'unknown', '/api/me');
  pass('three independent authenticated identities; private chats and preferences remain private');

  const proposals = {};
  for (const owner of ['bob', 'carol']) {
    const body = { request_id: `delegate-${owner}`, owner, summary: `${goal}; ${owner}'s contribution` };
    proposals[owner] = await f.ok('alice', '/api/proposals', 'POST', body);
    assert.deepEqual(await f.ok('alice', '/api/proposals', 'POST', body), proposals[owner]);
    await f.expect(409, 'alice', '/api/proposals', 'POST', { ...body, owner: owner === 'bob' ? 'carol' : 'bob' });
  }
  for (const who of [...participants, 'eve']) assert.deepEqual(await f.ok(who, '/api/tasks'), []);
  for (const who of ['bob', 'carol', 'eve']) assert.deepEqual(await f.ok(who, '/api/proposals'), []);
  pass('same textual objective does not publish private proposals or create a shared project');

  const tasks = {};
  for (const owner of ['bob', 'carol']) {
    const route = `/api/proposals/${proposals[owner].id}/actions`;
    const body = { action: 'confirm', expected_revision: 0, details: `${goal}; explicitly shared with ${owner} only` };
    const first = await f.ok('alice', route, 'POST', body);
    assert.deepEqual(await f.ok('alice', route, 'POST', body), first);
    tasks[owner] = first.task_id;
  }
  assert.equal((await f.ok('alice', '/api/tasks')).length, 2);
  assert.equal((await f.ok('bob', '/api/tasks')).length, 1);
  assert.equal((await f.ok('carol', '/api/tasks')).length, 1);
  assert.deepEqual(await f.ok('eve', '/api/tasks'), []);
  const task = owner => `/api/tasks/${tasks[owner]}`;
  for (const [owner, others] of [['bob', ['carol', 'eve']], ['carol', ['bob', 'eve']]]) {
    for (const outsider of others) {
      await f.expect(404, outsider, task(owner));
      await f.expect(404, outsider, `${task(owner)}/actions`, 'POST', { action: 'accept', expected_revision: 0 });
    }
    await f.expect(409, 'alice', `${task(owner)}/actions`, 'POST', { action: 'accept', expected_revision: 0 });
    await f.expect(422, owner, `${task(owner)}/actions`, 'POST', { action: 'accept', expected_revision: 0, actor: 'alice' });
    await f.ok(owner, `${task(owner)}/actions`, 'POST', { action: 'accept', expected_revision: 0 });
  }
  pass('two explicitly shared contributions retain participant ACLs and reject forged actors');

  const draftBody = { request_id: 'bob-private-draft', expected_revision: 1 };
  const draft = await f.ok('bob', `${task('bob')}/drafts`, 'POST', draftBody);
  assert.equal(draft.status, 'complete');
  assert.equal(draft.response.backend, 'mock');
  assert.deepEqual(await f.ok('bob', `${task('bob')}/drafts`, 'POST', draftBody), draft);
  for (const outsider of ['alice', 'carol', 'eve']) {
    await f.expect(404, outsider, `${task('bob')}/drafts`);
    await f.expect(404, outsider, `${task('bob')}/drafts/turns/${draftBody.request_id}`);
  }
  const shared = await f.ok('alice', task('bob'));
  assert.equal(shared.task.result, null);
  assert.equal(shared.deliveries.length, 0);
  assert(!JSON.stringify(shared).includes('PRIVATE-'));
  pass('drafting is owner-private and cannot count as a delivery');

  await f.ok('bob', `${task('bob')}/actions`, 'POST', { action: 'deliver', expected_revision: 1, result: 'Reviewed contribution v1' });
  await f.expect(409, 'alice', `${task('bob')}/actions`, 'POST', { action: 'approve', expected_revision: 1 });
  await f.ok('alice', `${task('bob')}/actions`, 'POST', { action: 'return', expected_revision: 2 });
  const returned = await f.ok('alice', task('bob'));
  assert.equal(returned.task.status, 'awaiting_owner');
  assert.equal(returned.task.result, null);
  assert.equal(returned.deliveries.length, 1);
  assert.equal(returned.deliveries[0].content, 'Reviewed contribution v1');
  assert.equal((await f.ok('carol', task('carol'))).task.revision, 1);
  assert.equal((await f.ok('bob', `${task('bob')}/drafts/turns/${draftBody.request_id}`)).current_stale, true);
  await f.expect(409, 'bob', `${task('bob')}/actions`, 'POST', { action: 'deliver', expected_revision: 1, result: 'Stale delivery must not commit' });
  await f.ok('bob', `${task('bob')}/actions`, 'POST', { action: 'accept', expected_revision: 3 });
  const delivery = { action: 'deliver', expected_revision: 4, result: 'Reviewed contribution v2' };
  const race = await Promise.all([f.api('bob', `${task('bob')}/actions`, 'POST', delivery), f.api('bob', `${task('bob')}/actions`, 'POST', delivery)]);
  assert.deepEqual(race.map(response => response.status).sort(), [200, 409]);
  const delivered = await f.ok('alice', task('bob'));
  assert.equal(delivered.deliveries.length, 2);
  assert.equal(delivered.task.revision, 5);
  assert.deepEqual(delivered.events.map(event => event.actor), ['alice', 'bob', 'bob', 'alice', 'bob', 'bob']);
  await f.ok('alice', `${task('bob')}/actions`, 'POST', { action: 'approve', expected_revision: 5 });
  pass('return preserves old delivery; stale and concurrent writes cannot overwrite or duplicate it');

  const beforeSend = { request_id: 'before-forward', body: 'PAIR-ONLY never forwarded' };
  await f.loseResponse('alice', '/api/contacts/bob/messages', beforeSend, false);
  assert.deepEqual(await f.ok('alice', '/api/contacts/bob/messages'), []);
  const message = { request_id: 'after-commit', body: 'PAIR-ONLY committed before connection was lost' };
  await f.loseResponse('alice', '/api/contacts/bob/messages', message, true);
  const persisted = await f.ok('alice', '/api/contacts/bob/messages');
  assert.equal(persisted.length, 1);
  assert.equal(persisted[0].sender, 'alice');
  assert.equal(persisted[0].author_kind, 'human');
  await f.restart();
  const replay = await f.ok('alice', '/api/contacts/bob/messages', 'POST', message);
  assert.deepEqual(await f.ok('alice', '/api/contacts/bob/messages', 'POST', message), replay);
  assert.deepEqual(await f.ok('alice', '/api/contacts/bob/messages'), persisted);
  await f.expect(409, 'alice', '/api/contacts/bob/messages', 'POST', { ...message, body: 'different content' });
  assert.deepEqual(await f.ok('carol', '/api/contacts/bob/messages'), []);
  assert.deepEqual(await f.ok('eve', '/api/contacts/bob/messages'), []);
  pass('real HTTP cuts before/after commit plus process restart keep message retry at-most-once');

  const lostDelivery = { action: 'deliver', expected_revision: 1, result: 'Review checklist' };
  await f.loseResponse('carol', `${task('carol')}/actions`, lostDelivery, true);
  await f.restart();
  const recovered = await f.ok('alice', task('carol'));
  assert.equal(recovered.task.status, 'ready_for_acceptance');
  assert.equal(recovered.task.revision, 2);
  assert.equal(recovered.deliveries.length, 1);
  await f.expect(409, 'carol', `${task('carol')}/actions`, 'POST', lostDelivery);
  assert.deepEqual(await f.ok('alice', task('carol')), recovered);
  await f.ok('alice', `${task('carol')}/actions`, 'POST', { action: 'approve', expected_revision: 2 });
  for (const owner of ['bob', 'carol']) {
    assert.equal((await f.ok('alice', task(owner))).task.status, 'completed');
    await f.expect(404, 'eve', task(owner));
  }
  assert.equal((await f.ok('bob', `${task('bob')}/drafts`)).items.length, 1);
  for (const who of participants) {
    assert.equal((await f.ok(who, '/api/collaboration-profile')).private.work_context, privateMarker(who));
    for (const route of ['/api/tasks', '/api/activity']) assert(!JSON.stringify(await f.ok(who, route)).includes('PRIVATE-'));
  }
  pass('lost delivery response is resolved by read, not replayed as a fresh action; histories survive restart');
}

async function foundation(f) {
  for (const kind of ['temporary', 'ongoing']) {
    const body = { request_id: `project-${kind}`, name: `Release note ${kind}`, goal, kind };
    await f.loseResponse('alice', '/api/projects', body, true);
    const project = await f.ok('alice', '/api/projects', 'POST', body);
    assert.deepEqual(await f.ok('alice', '/api/projects', 'POST', body), project);
    assert.equal(project.kind, kind);
    assert.equal(project.goal, goal);
    assert.equal(project.creator, 'alice');
    assert.equal(project.revision, 0);
    const route = `/api/projects/${project.id}`;
    const detail = who => f.ok(who, route);
    const current = async () => (await detail('alice')).project.revision;
    const mutate = async (who, suffix, data = {}) => f.ok(who, route + suffix, 'POST', {
      ...data, expected_revision: await current() });
    await f.expect(409, 'alice', '/api/projects', 'POST', { ...body, goal: 'changed goal' });
    assert.equal((await f.ok('alice', '/api/projects')).filter(p => p.id === project.id).length, 1);
    for (const who of ['bob', 'carol', 'eve']) await f.expect(404, who, route);
    for (const owner of ['bob', 'carol']) await mutate('alice', '/members', { owner });
    const bobInvite = (await f.ok('bob', '/api/project-invitations')).find(p => p.id === project.id);
    const carolInvite = (await f.ok('carol', '/api/project-invitations')).find(p => p.id === project.id);
    assert.equal(bobInvite.goal, goal);
    assert.equal(bobInvite.members, undefined);
    assert.equal(bobInvite.tasks, undefined);
    await f.expect(404, 'bob', route);
    await f.loseResponse('bob', route + '/membership', { action: 'accept', expected_revision: bobInvite.revision }, true);
    await f.restart();
    assert.equal((await detail('bob')).project.id, project.id);
    await f.expect(409, 'carol', route + '/membership', 'POST', { action: 'accept', expected_revision: carolInvite.revision });
    await mutate('carol', '/membership', { action: 'accept' });
    const joined = await detail('alice');
    for (const who of participants) {
      const view = await detail(who);
      assert.deepEqual(view.project, joined.project);
      assert.deepEqual(view.members, participants.map(owner => ({ owner, status: 'active' })));
      assert.deepEqual(view.tasks, []);
      assert(!(await f.ok(who, '/api/project-invitations')).some(p => p.id === project.id));
    }
    assert(!(await f.ok('eve', '/api/projects')).some(p => p.id === project.id));
    assert(!(await f.ok('eve', '/api/project-invitations')).some(p => p.id === project.id));
    await f.expect(404, 'bob', route + '/members', 'POST', { owner: 'eve', expected_revision: await current() });
    console.log(`PASS [foundation:${kind}] goal, typed project, lost-create/accept responses, three accepted identities and invitation isolation`);

    const ids = {};
    for (const owner of ['bob', 'carol']) {
      const p = await f.ok('alice', '/api/proposals', 'POST', { owner,
        summary: `PRIVATE-TASK-SUMMARY-${kind}-${owner}`, request_id: `project-task-${kind}-${owner}` });
      const confirmed = await f.ok('alice', `/api/proposals/${p.id}/actions`, 'POST', {
        action: 'confirm', expected_revision: 0, details: `PRIVATE-MATERIAL-${kind}-${owner}` });
      ids[owner] = confirmed.task_id;
      await mutate('alice', '/tasks', { task_id: confirmed.task_id, label: `Shared ${owner} contribution` });
      const other = owner === 'bob' ? 'carol' : 'bob';
      assert(!(await detail(other)).tasks.some(t => t.task_id === confirmed.task_id));
      assert.equal((await detail(owner)).tasks.find(t => t.task_id === confirmed.task_id).status, 'pending');
      const approve = `/tasks/${confirmed.task_id}/actions`;
      await f.expect(404, 'alice', route + approve, 'POST', { action: 'approve', expected_revision: await current() });
      await f.expect(404, other, route + approve, 'POST', { action: 'approve', expected_revision: await current() });
      await mutate(owner, approve, { action: 'approve' });
      for (const who of participants) {
        const view = await detail(who);
        assert(!JSON.stringify(view).includes('PRIVATE-'));
        const link = view.tasks.find(t => t.task_id === confirmed.task_id);
        assert.equal(link.status, 'active');
        assert.equal(link.can_open_task, who === 'alice' || who === owner);
        assert.equal(link.task_status, 'awaiting_owner');
        assert.equal(link.summary, undefined);
        assert.equal(link.result, undefined);
      }
      await f.expect(404, other, `/api/tasks/${confirmed.task_id}`);
      await f.expect(404, other, `/api/tasks/${confirmed.task_id}/drafts`);
      await f.expect(404, other, `/api/tasks/${confirmed.task_id}/actions`, 'POST', { action: 'accept', expected_revision: 0 });
    }
    await f.ok('bob', `/api/tasks/${ids.bob}/actions`, 'POST', { action: 'accept', expected_revision: 0 });
    assert.equal((await detail('carol')).tasks.find(t => t.task_id === ids.bob).task_status, 'working');
    await f.restart();
    assert.equal((await detail('carol')).tasks.length, 2);
    console.log(`PASS [foundation:${kind}] bilateral consent shares explicit labels and progress, never task contents or task action rights`);

    // Either participant can revoke a link; re-publishing requires both sides again.
    await mutate('carol', `/tasks/${ids.carol}/actions`, { action: 'revoke' });
    assert(!(await detail('bob')).tasks.some(t => t.task_id === ids.carol));
    await f.expect(404, 'carol', route + `/tasks/${ids.carol}/actions`, 'POST', { action: 'approve', expected_revision: await current() });
    await mutate('alice', '/tasks', { task_id: ids.carol, label: 'Re-reviewed shared Carol contribution' });
    assert(!(await detail('bob')).tasks.some(t => t.task_id === ids.carol));
    await mutate('carol', `/tasks/${ids.carol}/actions`, { action: 'approve' });
    const beforeLeave = await current();
    await f.loseResponse('carol', route + '/membership', { action: 'leave', expected_revision: beforeLeave }, true);
    await f.restart();
    await f.expect(404, 'carol', route);
    assert(!(await f.ok('carol', '/api/projects')).some(p => p.id === project.id));
    assert(!(await detail('alice')).tasks.some(t => t.task_id === ids.carol));
    assert.equal((await f.ok('carol', `/api/tasks/${ids.carol}`)).task.owner, 'carol');
    await f.expect(404, 'carol', route + '/membership', 'POST', { action: 'accept', expected_revision: beforeLeave });
    await mutate('alice', '/members', { owner: 'carol' });
    await mutate('carol', '/membership', { action: 'accept' });
    assert(!(await detail('carol')).tasks.some(t => t.task_id === ids.carol), 'Rejoining must not silently restore old shared links');
    await mutate('alice', '/members/bob/remove');
    await f.expect(404, 'bob', route);
    assert.equal((await f.ok('bob', `/api/tasks/${ids.bob}`)).task.owner, 'bob');
    assert.deepEqual((await detail('alice')).tasks, []);
    const revision = await current();
    const race = await Promise.all([f.api('alice', route + '/archive', 'POST', { expected_revision: revision }),
      f.api('alice', route + '/archive', 'POST', { expected_revision: revision })]);
    assert.deepEqual(race.map(response => response.status).sort(), [200, 409]);
    await f.restart();
    assert.equal((await detail('carol')).project.status, 'archived');
    assert.equal((await detail('alice')).project.revision, revision + 1);
    await f.expect(409, 'alice', route + '/members', 'POST', { owner: 'eve', expected_revision: revision + 1 });
    await f.expect(404, 'eve', route);
    console.log(`PASS [foundation:${kind}] revoke, leave, rejoin and remove do not restore sharing; concurrent archive and restart preserve state`);
  }
  console.log('FOUNDATION API PASSED. Goal changes, blockers, full project acceptance, OIDC tenant boundaries and browser UI remain outside this suite.');
}

async function main() {
  const args = process.argv.slice(2);
  if (args.length === 1 && args[0] === '--help') {
    console.log('Usage: node scripts/three-person-acceptance.cjs [--scope prerequisites|foundation|all]\nBuild hub first, or set AGENCT_TEST_HUB_BINARY to the exact build being tested.\nDefault: prerequisites. Foundation requires the separate project implementation.\nReal temporary Hub/SQLite; mock model; no browser. See docs/three-person-acceptance.md.');
    return;
  }
  const scope = args.length === 0 ? 'prerequisites' : args[1];
  assert(args.length === 0 || (args.length === 2 && args[0] === '--scope'), 'Unknown argument; use --help');
  assert(['prerequisites', 'foundation', 'all'].includes(scope), 'Unknown scope; use --help');
  const binary = path.resolve(process.env.AGENCT_TEST_HUB_BINARY || path.join(root, 'hub/target/debug/agenct-hub'));
  fs.accessSync(binary, fs.constants.X_OK);
  const fixture = new Fixture(binary, await freePort());
  const interrupted = () => { void fixture.stop('SIGKILL'); };
  process.once('SIGINT', interrupted); process.once('SIGTERM', interrupted);
  try {
    await fixture.start();
    if (scope !== 'foundation') {
      await prerequisites(fixture);
      console.log('PREREQUISITES PASSED. Shared projects, blockers, goal changes, three-browser UI and real-device trials are NOT certified by this suite.');
    }
    if (scope !== 'prerequisites') await foundation(fixture);
  } finally {
    await fixture.dispose();
    process.removeListener('SIGINT', interrupted); process.removeListener('SIGTERM', interrupted);
  }
}
if (require.main === module) main().catch(error => { console.error(`FAIL: ${error.message}`); process.exitCode = 1; });
module.exports = { Fixture, prerequisites, foundation, goal, participants };
