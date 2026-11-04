const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const { webcrypto } = require('node:crypto');
class Element {
  constructor() { this.children=[]; this.textContent=''; }
  append(el) { this.children.push(el); }
  replaceChildren(...els) { this.children=els; this.textContent=''; }
}
const walk = el => [el,...el.children.flatMap(walk)];
function fixture() {
  const conversation = new Element(), calls=[];
  let user='bob', epoch=1, hold=false, release, fail=false, readFail=false, postStatus='queued';
  let task={id:'t',owner:'bob',requester:'alice',revision:1,status:'working',summary:'<safe task>'};
  let data={scope:{task_id:'t',owner:'bob',task_revision:1,repository:'agenct',base_commit:'a'.repeat(40)},jobs:[],execution_enabled:false,publication_enabled:false};
  const sandbox=vm.createContext({document:{createElement:()=>new Element()},crypto:webcrypto});
  vm.runInContext(fs.readFileSync(path.join(__dirname,'worker-ui.js'),'utf8'),sandbox);
  const ctx=()=>({user,epoch});
  const ui=sandbox.createWorkerUI({
    current:c=>c.user===user && c.epoch===epoch,
    article:text=>{const el=new Element();el.textContent=text;return el;},conversation:()=>conversation,
    api:async(c,route,method='GET',body)=>{
      calls.push({c,route,method,body});
      if(hold) await new Promise(r=>{release=r;});
      if((fail&&method==='POST')||(readFail&&method==='GET'))throw Error('unknown outcome');
      if(method==='POST') {
        if(route.endsWith('/cancel')) return {id:route.split('/').at(-2),status:'cancelled',execution_enabled:false,publication_enabled:false};
        return {job:{id:'job-1',owner:'bob',status:postStatus,request:{task_id:task.id,task_revision:body.expected_revision,repository:body.repository,base_commit:body.base_commit,request_id:body.request_id}},execution_enabled:false,publication_enabled:false};
      }
      return JSON.parse(JSON.stringify(data));
    }
  });
  const render=()=>{ui.update(ctx(),[task]);const root=new Element();ui.attach(root,ctx(),task);conversation.append(root);return root;};
  render();
  return {ui,calls,conversation,render,ctx,
    button:text=>walk(conversation).find(e=>e.textContent===text),
    open:async()=>{const root=render();await root.children[0]?.onclick();},
    hold:()=>{hold=true;},release:()=>{hold=false;release();},fail:(value=true)=>{fail=value;},postStatus:value=>{postStatus=value;},readFail:()=>{readFail=true;},
    logout:()=>{epoch++;ui.reset();conversation.replaceChildren();},
    change:patch=>{task={...task,...patch};ui.update(ctx(),[task]);},
    response:patch=>{data={...data,...patch};},
    posts:()=>calls.filter(c=>c.method==='POST')
  };
}
(async()=>{
  const f=fixture();await f.open();assert.equal(f.posts().length,0);
  assert(walk(f.conversation).some(e=>e.textContent.includes('不运行代码')));
  assert(walk(f.conversation).some(e=>e.textContent.includes('<safe task>')));
  f.hold();const confirm=f.button('确认仅入队，不执行');const saving=confirm.onclick();await confirm.onclick();assert.equal(f.posts().length,1);
  assert.equal(f.posts()[0].body.expected_revision,1);assert.equal(f.posts()[0].body.confirm_queue_only,true);
  assert.equal(f.posts()[0].body.base_commit,'a'.repeat(40));
  f.release();await saving;await confirm.onclick();assert.equal(f.posts().length,1,'Detached confirmation cannot be reused');
  assert(walk(f.conversation).some(e=>e.textContent.includes('尚未执行')));

  const other=fixture();other.change({owner:'alice'});assert.equal(other.render().children.length,0);
  const stale=fixture();await stale.open();const old=stale.button('确认仅入队，不执行');stale.change({revision:2});await old.onclick();assert.equal(stale.posts().length,0);
  const closed=fixture();await closed.open();const oldClosed=closed.button('确认仅入队，不执行');await closed.button('关闭队列卡').onclick();await oldClosed.onclick();assert.equal(closed.posts().length,0);
  const logout=fixture();await logout.open();const oldLogout=logout.button('确认仅入队，不执行');logout.logout();await oldLogout.onclick();assert.equal(logout.posts().length,0);
  const inflight=fixture();await inflight.open();inflight.hold();const p=inflight.button('确认仅入队，不执行').onclick();inflight.logout();inflight.release();await p;assert.equal(inflight.conversation.children.length,0);
  const read=fixture();read.hold();const reading=read.open();read.logout();read.release();await reading;assert.equal(read.conversation.children.length,0);

  const uncertain=fixture();await uncertain.open();uncertain.fail();await uncertain.button('确认仅入队，不执行').onclick();
  assert.equal(uncertain.posts().length,1);assert(walk(uncertain.conversation).some(e=>e.textContent.includes('不会自动重试')));
  const requestId=uncertain.posts()[0].body.request_id;await uncertain.button('重新读取队列').onclick();assert.equal(uncertain.posts().length,1);
  await uncertain.button('确认仅入队，不执行').onclick();assert.equal(uncertain.posts()[1].body.request_id,requestId);

  const job={id:'job-1',status:'queued',attempt:0,request:{repository:'agenct',base_commit:'a'.repeat(40)}};
  const cancel=fixture();cancel.response({jobs:[job]});await cancel.open();assert(!cancel.button('确认仅入队，不执行'));
  cancel.hold();const cancelButton=cancel.button('取消此队列记录');const cancelling=cancelButton.onclick();await cancelButton.onclick();assert.equal(cancel.posts().length,1);assert.equal(cancel.posts()[0].route,'tasks/t/worker/job-1/cancel');cancel.release();await cancelling;
  const recovered=fixture();await recovered.open();recovered.fail();await recovered.button('确认仅入队，不执行').onclick();
  const oldId=recovered.posts()[0].body.request_id;
  const receipt={...job,owner:'bob',request:{...job.request,task_id:'t',task_revision:1,request_id:oldId}};
  recovered.fail(false);recovered.response({jobs:[receipt]});await recovered.button('重新读取队列').onclick();
  await recovered.button('取消此队列记录').onclick();
  recovered.response({jobs:[{...receipt,status:'cancelled'}]});await recovered.button('重新读取队列').onclick();
  assert(!recovered.button('确认仅入队，不执行'));assert(recovered.button('确认新请求仅入队，不执行'));
  assert.equal(recovered.posts().length,2,'Reread never re-enqueues automatically');
  await recovered.button('确认新请求仅入队，不执行').onclick();
  assert.notEqual(recovered.posts()[2].body.request_id,oldId,'New terminal-state consent must use a new ID');
  const terminalReplay=fixture();await terminalReplay.open();terminalReplay.postStatus('cancelled');await terminalReplay.button('确认仅入队，不执行').onclick();
  assert(walk(terminalReplay.conversation).some(e=>e.textContent.includes('未重新入队')));
  assert(!walk(terminalReplay.conversation).some(e=>e.textContent.includes('已登记队列')));
  const mismatched=fixture();mismatched.response({scope:{task_id:'t',owner:'bob',task_revision:0,repository:'agenct',base_commit:'a'.repeat(40)}});await mismatched.open();assert(!mismatched.button('确认仅入队，不执行'));
  const noScope=fixture();noScope.response({scope:null});await noScope.open();assert(!noScope.button('确认仅入队，不执行'));
  const unsafe=fixture();unsafe.response({execution_enabled:true});await unsafe.open();assert(!unsafe.button('确认仅入队，不执行'));
  const failedRead=fixture();failedRead.readFail();await failedRead.open();assert(failedRead.button('重新读取队列'));assert.equal(failedRead.posts().length,0);
  console.log('PASS queue UI: owner scope, explicit consent, no auto mutation, stale revision/epoch, close, duplicate prevention, uncertain receipt ID reuse, cancel and fail-closed mode');
})().catch(error=>{console.error(error);process.exitCode=1;});
