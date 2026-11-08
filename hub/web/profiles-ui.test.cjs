const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
class Element {
  constructor() { this.children = []; this.textContent = ''; }
  append(element) { this.children.push(element); }
  replaceChildren(...elements) { this.children = elements; }
}
const walk = el => [el, ...el.children.flatMap(walk)];
function fixture() {
  const conversation = new Element(), calls = [];
  let identity = 'alice', held = false, release, fail = false, failReads = false;
  const profile = { revision: 3, agents: [{ id:'butler', name:'<safe name>', role:'coordinator', model_ref:'server-default', enabled:true }] };
  const context = vm.createContext({ document:{ createElement:() => new Element() }, TextEncoder });
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'profiles-ui.js'),'utf8'), context);
  const ui = context.createProfileUI({
    api: async (ctx, route, method='GET', body) => {
      calls.push({ ctx, route, method, body });
      if (held && method === 'PUT') await new Promise(resolve => { release = resolve; });
      if (failReads && method === 'GET') throw Error('read unavailable');
      if (fail && method === 'PUT') throw Error('uncertain result');
      if (route.endsWith('catalog')) return { roles:['coordinator','researcher'], models:[{id:'server-default'}], max_agents:8 };
      if (method === 'PUT') return { ...body, revision:4 };
      return profile;
    },
    current: ctx => ctx.user === identity,
    article: text => { const e = new Element(); e.textContent = text; return e; },
    host: () => conversation, status: () => {}
  });
  return { ui, calls, conversation,
    button: text => walk(conversation).find(e => e.textContent === text),
    open: () => ui.handle({user:identity}, '/agents'),
    hold: () => { held = true; }, release: () => release(), fail: () => { fail = true; }, readsFail: value => { failReads = value; },
    logout: () => { identity = 'bob'; ui.reset(); conversation.replaceChildren(); }
  };
}
(async () => {
  const f = fixture(); await f.open();
  assert.equal(f.calls.filter(c => c.method === 'PUT').length,0);
  assert(walk(f.conversation).some(e => e.textContent.includes('不启动 Agent')));
  await f.button('添加角色').onclick();
  await f.button('检查更改').onclick();
  assert(walk(f.conversation).some(e => e.textContent.includes('<safe name>')));
  assert.equal(f.calls.filter(c => c.method === 'PUT').length,0);
  await f.button('返回修改').onclick();
  await f.button('检查更改').onclick();
  f.hold(); const confirm = f.button('确认保存个人配置'); const saving = confirm.onclick();
  await confirm.onclick();
  assert.equal(f.calls.filter(c => c.method === 'PUT').length,1);
  const put = f.calls.find(c => c.method === 'PUT');
  assert.equal(put.body.expected_revision,3); assert.equal(put.body.agents.length,2);
  assert.deepEqual(Object.keys(put.body).sort(),['agents','expected_revision']);
  f.release(); await saving;
  assert(walk(f.conversation).some(e => e.textContent.includes('已保存个人配置')));
  assert(f.button('×'));

  const canceled = fixture(); await canceled.open(); await canceled.button('检查更改').onclick();
  const oldConfirm = canceled.button('确认保存个人配置'); await canceled.button('取消，不保存').onclick();
  await oldConfirm.onclick(); assert.equal(canceled.calls.filter(c=>c.method==='PUT').length,0);

  const stale = fixture(); await stale.open(); await stale.button('检查更改').onclick();
  const staleConfirm = stale.button('确认保存个人配置'); stale.logout(); await staleConfirm.onclick();
  assert.equal(stale.calls.filter(c=>c.method==='PUT').length,0);

  const pending = fixture(); await pending.open(); await pending.button('检查更改').onclick(); pending.hold();
  const oldSave = pending.button('确认保存个人配置').onclick(); pending.logout(); pending.release(); await oldSave;
  assert.equal(pending.conversation.children.length,0);

  const uncertain = fixture(); await uncertain.open(); await uncertain.button('检查更改').onclick(); uncertain.fail();
  await uncertain.button('确认保存个人配置').onclick();
  assert(!uncertain.button('确认保存个人配置')); assert(uncertain.button('重新读取配置'));
  assert(walk(uncertain.conversation).some(e=>e.textContent.includes('不会自动重试')));
  assert.equal(uncertain.calls.filter(c=>c.method==='PUT').length,1);
  assert.equal(await uncertain.ui.handle({user:'alice'},'hello'),false);
  uncertain.readsFail(true);
  await uncertain.button('重新读取配置').onclick();
  assert(walk(uncertain.conversation).some(e=>e.textContent.includes('无法读取个人配置')));
  assert(uncertain.button('重新读取配置'));
  assert.equal(uncertain.calls.filter(c=>c.method==='PUT').length,1);
  uncertain.readsFail(false);
  await uncertain.button('重新读取配置').onclick();
  assert(uncertain.button('检查更改'), 'Failed reload can recover into editable card');
  const initialFailure = fixture(); initialFailure.readsFail(true); await initialFailure.open();
  assert(initialFailure.button('重新读取配置'));
  const retry = initialFailure.button('重新读取配置'); initialFailure.logout();
  const beforeRetry = initialFailure.calls.length; await retry.onclick();
  assert.equal(initialFailure.calls.length, beforeRetry, 'Old retry cannot read as a new identity');

  console.log('PASS profile UI consent, cancel, back, duplicate-save, identity isolation, persistence display and uncertain-save recovery');
})().catch(error => { console.error(error); process.exitCode=1; });
