const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
class Node {
  constructor(tag = 'div', cls = '', text = '') { this.tagName = tag.toUpperCase(); this.children = []; this.textContent = text; this.attrs = {}; this.isConnected = true; this.value = ''; this.className = cls; this.disabled = false; }
  append(...nodes) { for (const n of nodes) { n.parent = this; this.children.push(n); } }
  contains(node) { return node === this || this.children.some(n => n.contains?.(node)); }
  replaceWith(node) { if (this.parent) { this.parent.children[this.parent.children.indexOf(this)] = node; node.parent = this.parent; } this.isConnected = false; }
  setAttribute(k, v) { this.attrs[k] = v; }
  focus() {}
  scrollIntoView() {}
  reportValidity() { return true; }
  querySelectorAll(selector) { return this.children.flatMap(n => [...(n.tagName === 'BUTTON' ? [n] : []), ...(n.querySelectorAll?.(selector) || [])]); }
}
const nodes = new Map(), $ = id => { if (!nodes.has(id)) nodes.set(id, new Node()); return nodes.get(id); };
$('room').classList = { contains: () => true };
const context = vm.createContext({ $, el: (...args) => new Node(...args), user: 'alice', session: () => ({ user: 'alice' }), current: () => true,
  crypto: { randomUUID: () => 'request-one' }, ProjectStateModel: require('./project-state-model.js'), TASK_STATUS: {}, console });
vm.runInContext(fs.readFileSync(__dirname + '/projects-ui.js', 'utf8'), context);
const run = source => vm.runInContext(source, context);
const walk = node => [node, ...node.children.flatMap(walk)];
const button = text => walk($('project-panel')).find(n => n.tagName === 'BUTTON' && n.textContent === text);
const field = label => walk($('project-panel')).find(n => n.attrs['aria-label'] === label);
const state = { project: { id: 'p', name: 'Site', revision: 8, creator: 'alice', status: 'active' }, members: [], items: [{ id: 'g', version: 2, kind: 'goal', content: 'Approved site scope' }], tasks: [{ task_id: 't', status: 'active', task_status: 'accepted', label: 'Public label' }], completion: { project_revision: 8, snapshot_token: 'snapshot-eight', ready: true, goals: [{ item_id: 'g', item_version: 2 }], tasks: [{ task_id: 't', accepted_version: 4, status: 'accepted' }], blockers: [], reasons: [], acceptance: null } };
const reset = () => { $('project-panel').children = []; context.view = structuredClone(state); run('projectView = view; projectDraft = null; projectBusy = false; clearProjectAlignment = () => {};'); };
const preview = label => { field(label).value = 'Explicit shared note'; const form = walk($('project-panel')).find(n => n.tagName === 'FORM'); form.onsubmit({ preventDefault() {} }); };
reset();
context.captured = null;
run('commitProject = async (_draft, path, body) => { captured = { path, body }; }; projectAcceptanceEditor();');
assert.equal(context.captured, null, 'opening editor never publishes');
preview('项目验收说明');
assert.equal(context.captured, null, 'preview never publishes');
const confirm = button('确认项目最终验收');
assert(confirm.disabled, 'explicit acknowledgement required');
confirm.onclick(); assert.equal(context.captured, null);
const ack = field('我已核对版本、共享范围和本人声明'); ack.checked = true; ack.onchange(); assert.equal(confirm.disabled, false);
confirm.onclick();
assert.equal(context.captured.path, 'projects/p/completion');
assert.deepEqual(JSON.parse(JSON.stringify(context.captured.body)), { request_id: 'request-one', expected_revision: 8, expected_snapshot: 'snapshot-eight', goals: state.completion.goals, tasks: state.completion.tasks, note: 'Explicit shared note' });
assert(!JSON.stringify(context.captured).includes('Public label'), 'body carries only reviewed task versions, no source text');
run('projectDraft.preview.isConnected = false'); context.captured = null; confirm.onclick(); assert.equal(context.captured, null, 'detached preview cannot submit');
reset(); run('user = "bob"; projectAcceptanceEditor()'); assert.equal(button('预览声明'), undefined, 'non-creator has no acceptance editor'); run('user = "alice"');
reset(); run('projectView.completion.ready = false; projectAcceptanceEditor()'); assert.equal(button('预览声明'), undefined);
reset(); run('projectView.completion.acceptance = { valid: true }; projectAcceptanceEditor()'); assert.equal(button('预览声明'), undefined, 'no repeated new acceptance for same valid state');
reset(); context.item = { id: 'block', version: 3, content: 'Missing asset', created_by: 'alice', kind: 'blocker', status: 'confirmed', confirmation_valid: true };
run('blockerResolutionEditor(item, "resolve")'); preview('阻塞解除说明');
const resolution = button('确认解除阻塞'); field('我已核对版本、共享范围和本人声明').checked = true; resolution.onclick();
assert.deepEqual(JSON.parse(JSON.stringify(context.captured.body)), { request_id: 'request-one', expected_revision: 8, expected_version: 3, expected_snapshot: 'snapshot-eight', action: 'resolve', note: 'Explicit shared note' });
assert.equal(context.captured.path, 'projects/p/state/block/resolution');
reset(); run('projectView.completion.blockers = [{ item_id: "block", item_version: 3, resolved: false }];');
context.card = new Node(); context.actions = new Node(); run('renderBlockerResolution(card, actions, item)');
assert(context.actions.children.some(n => n.textContent === '解除阻塞'));
run('item.created_by = "bob"'); context.actions = new Node(); run('renderBlockerResolution(card, actions, item)'); assert.equal(context.actions.children.length, 0);
run('item.status = "withdrawn"'); context.actions = new Node(); run('renderBlockerResolution(card, actions, item)'); assert.equal(context.actions.children.length, 0, 'withdrawn blockers cannot appear resolved');
assert(context.card.children.some(n => n.textContent.includes('不代表已解除')));
reset(); run('projectView.completion.acceptance = { id: "old", author: "alice", valid: false, withdrawn: false, goals: [], note: "Historical assertion" }; renderProjectCompletion()');
assert(button('撤回项目验收'), 'author can retract an invalid historical acceptance');
reset(); run('projectView.project.status = \"archived\"; projectView.completion.acceptance = { id: \"old\", author: \"alice\", valid: false, withdrawn: false, goals: [], note: \"Historical assertion\" }; renderProjectCompletion()');
assert(button('撤回项目验收'), 'creator can retract an unwithdrawn acceptance in an archived project');
assert.equal(button('验收项目'), undefined, 'archived project cannot create a new acceptance');
reset(); run('projectView.completion.acceptance = { id: "old", author: "alice", valid: false, withdrawn: true, goals: [], note: "" }; renderProjectCompletion()');
assert.equal(button('撤回项目验收'), undefined, 'withdrawn acceptance has no repeated withdrawal action');
console.log('Project completion UI explicit preview, named roles, exact goal/task binding, and withdrawal distinction passed');

assert(fs.readFileSync(__dirname + '/projects-ui.js', 'utf8').includes("\'未归档\'"), "Active project lifecycle must not contradict an accepted current version");
