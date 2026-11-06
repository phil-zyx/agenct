const assert = require('node:assert/strict');
const { prepareTaskTimeline: prepare } = require('./task-timeline-model.js');
const task = { id: 'task-1', revision: 8 };
const audit = { schema_version: 1, legacy_history_incomplete: false, legacy_notice: null };
const delivery = (seq, revision) => ({ seq, author: 'bob', content: `version ${seq} <script>alert(1)</script>`, created_at: '2026-11-06 01:02:03', task_revision: revision });
const transitions = [
  ['created', null, 'awaiting_owner', null], ['accept', 'awaiting_owner', 'working', null],
  ['deliver', 'working', 'ready_for_acceptance', 1], ['return', 'ready_for_acceptance', 'awaiting_owner', null],
  ['accept', 'awaiting_owner', 'working', null], ['deliver', 'working', 'ready_for_acceptance', 2],
  ['return', 'ready_for_acceptance', 'awaiting_owner', null], ['accept', 'awaiting_owner', 'working', null],
  ['deliver', 'working', 'ready_for_acceptance', 3]
];
const events = transitions.map(([action, from_status, to_status, delivery_seq], revision) => ({
  seq: revision + 1, actor: ['created', 'return'].includes(action) ? 'alice' : 'bob', action,
  at: '2026-11-06 01:02:03', from_status, to_status, revision, delivery_seq, provenance: 'recorded'
}));
const deliveries = [delivery(1, 2), delivery(2, 5), delivery(3, 8)];
const result = prepare(task, events, deliveries, audit);
assert.equal(result.rows.length, 9);
assert.equal(result.rows.filter(row => row.startsWith('退回')).length, 2);
for (const value of deliveries) assert(result.rows.some(row => row.includes(value.content)));
assert.equal(result.legacyDeliveries.length, 0);
assert(result.rows[2].includes('2026-11-06 01:02:03'), 'Do not guess timestamp timezone');
assert(result.rows[2].includes('<script>'), 'Pure model preserves literal text; DOM integration must use textContent');
const rejects = (e = events, d = deliveries, a = audit) => assert.throws(() => prepare(task, e, d, a));
rejects(events, [{ ...deliveries[0], status: 'draft' }, ...deliveries.slice(1)]);
rejects(events, [{ ...deliveries[0], source_draft_id: 'secret' }, ...deliveries.slice(1)]);
rejects(events, [{ ...deliveries[0], id: 'private-storage-id' }, ...deliveries.slice(1)]);
rejects(events, [deliveries[0], deliveries[0], deliveries[2]]);
rejects(events, deliveries.slice(1));
rejects(events.map((event, i) => i === 2 ? { ...event, revision: 3 } : event));
rejects([...events].reverse());
rejects(events.map((event, i) => ({ ...event, seq: event.seq + (i > 1 ? 1 : 0) })));
rejects([], [delivery(2, 2)], { ...audit, legacy_history_incomplete: true });
rejects(events.map((event, i) => i === 2 ? { ...event, action: 'draft_saved' } : event));
rejects(events.map((event, i) => i === 2 ? { ...event, provenance: 'legacy' } : event));
const legacyEvent = { seq: 1, actor: 'alice', action: 'created', at: 'unknown', from_status: null, to_status: null, revision: null, delivery_seq: null, provenance: 'legacy' };
const legacy = prepare(task, [legacyEvent], [delivery(1, 2)], { ...audit, legacy_history_incomplete: true });
assert.match(legacy.notice, /无法恢复/);
assert.equal(legacy.rows.length, 1);
assert.equal(legacy.legacyDeliveries.length, 1);
assert.match(legacy.legacyDeliveries[0], /无法核实对应历史事件/);
assert.deepEqual(deliveries, [delivery(1, 2), delivery(2, 5), delivery(3, 8)], 'Presentation must not mutate shared input');
console.log('PASS task timeline presentation contract: retained versions/returns, draft-field rejection, legacy gaps, validation and literal text');

const cancelled = { ...task, revision: 1 };
const cancellation = { seq: 2, actor: 'alice', action: 'cancel', at: 'now', from_status: 'awaiting_owner', to_status: 'canceled', revision: 1, delivery_seq: null, provenance: 'recorded', reason: '<script>literal reason</script>' };
const cancelTimeline = prepare(cancelled, [events[0], cancellation], [], audit);
assert.match(cancelTimeline.rows[1], /取消原因（双方可见）：<script>literal reason<\/script>/);
assert.match(prepare(cancelled, [events[0], { ...cancellation, reason: undefined }], [], audit).rows[1], /旧记录未保存/);
for (const reason of ['', '   ', 'x'.repeat(501), 123]) assert.throws(() => prepare(cancelled, [events[0], { ...cancellation, reason }], [], audit));
assert.throws(() => prepare(task, events.map((event,i) => i === 0 ? { ...event, reason: 'must not attach to creation' } : event), deliveries, audit));
