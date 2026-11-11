const assert = require('node:assert/strict');
const model = require('./project-state-model.js');
const project = { creator: 'alice', status: 'active' };
for (const kind of Object.keys(model.kinds)) {
  const item = { kind, created_by: 'carol', assignee: kind === 'assignment' ? 'bob' : null, status: 'proposed' };
  const named = kind === 'assignment' ? 'bob' : kind === 'blocker' ? 'carol' : 'alice';
  assert.equal(model.confirmer(project, item), named);
  for (const user of ['alice', 'bob', 'carol', 'outsider']) {
    assert.equal(model.permissions(project, item, user).confirm, user === named);
    assert.equal(model.permissions(project, item, user).revise, user === 'alice' || user === 'carol');
    assert.deepEqual(model.permissions({ ...project, status: 'archived' }, item, user), { confirm: false, revise: false, withdraw: false });
    assert.deepEqual(model.permissions(project, { ...item, status: 'withdrawn' }, user), { confirm: false, revise: false, withdraw: false });
    assert.equal(model.permissions(project, { ...item, status: 'confirmed' }, user).confirm, false);
  }
  assert.equal(model.status(item), '待本人确认');
  assert.equal(model.status({ ...item, status: 'confirmed', confirmed_by: named, confirmation_valid: true }), '已由 ' + named + ' 确认');
  assert.equal(model.status({ ...item, status: 'confirmed', confirmed_by: named, confirmation_valid: false }), '确认已失效，需重新核对');
}
console.log('Project state presentation permissions and named confirmations passed');

const source = { role: 'user', status: 'complete', content: '只选这一条，不自动共享', created_at: '2026-11-11 10:11:12' };
const human = model.chatCandidate(source, 'alice');
assert.equal(human.content, source.content);
assert.equal(human.source_kind, 'human_statement');
assert(human.source_note.includes('alice') && human.source_note.includes(source.created_at + ' UTC'));
assert(!human.source_note.includes(source.content));
assert(Object.isFrozen(human));
assert.equal(model.chatCandidate({ ...source, role: 'assistant' }, 'alice').source_kind, 'model_suggestion');
for (const status of ['pending', 'failed', 'interrupted', 'uncertain', undefined]) assert.equal(model.chatCandidate({ ...source, status }, 'alice'), null);
for (const role of ['system', 'tool', '', undefined]) assert.equal(model.chatCandidate({ ...source, role }, 'alice'), null);
for (const content of ['', ' \n ', null, 123]) assert.equal(model.chatCandidate({ ...source, content }, 'alice'), null);
for (const content of ['[MOCK · no model] fixture', '[系统提示] failed']) assert.equal(model.chatCandidate({ ...source, role: 'assistant', content }, 'alice'), null);
for (const created_at of [null, 'not a timestamp', 'secret injected into source']) {
  const candidate = model.chatCandidate({ ...source, created_at }, 'alice');
  assert(candidate.source_note.includes('时间未记录'));
  assert(!candidate.source_note.includes('secret'));
}
assert.equal(model.boundedText('汉'.repeat(4000), 4000), true);
assert.equal(model.boundedText('😀'.repeat(4000), 4000), true);
assert.equal(model.boundedText('x'.repeat(4001), 4000), false);
assert.equal(model.boundedText('x'.repeat(1001), 1000), false);
assert.equal(model.boundedText('\n \t', 4000), false);
for (const code of [0, 7, 11, 13, 31, 127, 159]) assert.equal(model.boundedText('text' + String.fromCharCode(code), 4000), false);
assert.equal(model.boundedText('safe\ntext\ttab', 4000), true);
console.log('Private chat candidate eligibility, locked provenance, literal content and server character limits passed');
