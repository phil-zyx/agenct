const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
class Element {
  constructor(tag) { this.tag = tag; this.children = []; this.textContent = ''; this.attributes = {}; }
  append(...elements) { this.children.push(...elements); }
  replaceChildren(...elements) { this.children = elements; }
  setAttribute(key, value) { this.attributes[key] = value; }
  focus() {}
}
const walk = node => [node, ...node.children.flatMap(walk)];
(async () => {
  const scope = vm.createContext({ document: { createElement: tag => new Element(tag) } });
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'collaboration-profile-ui.js'), 'utf8'), scope);
  const host = new Element('main'), calls = [];
  let epoch = 1, edits = 0, release, delayed = false;
  const profile = { revision: 1, public: { team: '研发', responsibilities: '<script>不会执行</script>' }, private: { work_context: '本人的背景', response_preferences: '先写结论' }, permissions: { auto_reply_public: false, auto_prepare_drafts: true } };
  const ui = scope.createCollaborationProfileUI({ host: () => host, current: context => context.epoch === epoch,
    onEdit: () => { edits++; }, api: async (context, route, method = 'GET') => {
      calls.push({ route, method }); if (delayed) await new Promise(done => { release = done; }); return profile;
    } });
  await ui.open({ epoch: 1 });
  assert.equal(walk(host).filter(node => ['input', 'textarea', 'select', 'form'].includes(node.tag)).length, 0);
  assert(walk(host).some(node => node.textContent === '<script>不会执行</script>'));
  assert(walk(host).some(node => node.textContent === '先写结论'));
  assert(walk(host).some(node => node.textContent === '未允许'));
  const edit = walk(host).find(node => node.textContent === '通过聊天修改'); edit.onclick();
  assert.equal(edits, 1); assert.equal(host.children.length, 0);
  assert(calls.every(call => call.method === 'GET'), 'read-only drawer must never save settings');
  assert.equal(ui.hasUnsaved(), false);
  delayed = true; const pending = ui.open({ epoch: 1 }); epoch = 2; ui.reset(); release(); await pending;
  assert.equal(host.children.length, 0, 'old account response must not restore private facts');
  edit.onclick(); assert.equal(edits, 1, 'old account control must not act');
  console.log('PASS personal facts are read-only; editing routes to chat; late identity responses stay cleared');
})().catch(error => { console.error(error); process.exitCode = 1; });
