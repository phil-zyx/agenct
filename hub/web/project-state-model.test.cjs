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
