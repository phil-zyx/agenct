const assert = require('node:assert/strict');
const fs = require('node:fs'), vm = require('node:vm');
const { webcrypto } = require('node:crypto');
class Element { constructor(){this.children=[];this.textContent='';this.value='';} append(e){this.children.push(e);} replaceChildren(...e){this.children=e;this.textContent='';} setAttribute(){} }
const walk=e=>[e,...e.children.flatMap(walk)];
function fixture() {
 const conversation=new Element(), calls=[], storage=new Map(), timers=new Map(); let timerId=0; let epoch=1, user='bob', error=null, stale=false, truncated=false, hold=false, release, consent=true;
 let task={id:'task1',owner:'bob',requester:'alice',revision:1,status:'working'};
 const context=()=>({user,epoch,browserMode:false,signal:new AbortController().signal});
 const sandbox=vm.createContext({document:{createElement:()=>new Element()},crypto:webcrypto,TextEncoder,AbortController,setTimeout:(fn,ms)=>{const id=++timerId;timers.set(id,{fn,ms});return id;},clearTimeout:id=>timers.delete(id),confirm:()=>consent,sessionStorage:{getItem:k=>storage.get(k),setItem:(k,v)=>storage.set(k,v),removeItem:k=>storage.delete(k)}});
 vm.runInContext(fs.readFileSync(__dirname+'/draft-ui.js','utf8'),sandbox);
 const response=id=>({kind:'draft_delivery',task_id:task.id,request_id:id,status:'complete',current_stale:stale,response:{draft:{content:'Generated <script> text',task_revision:1,stale:false},task_context:{id:task.id,summary_truncated:truncated,result_truncated:false},runtime_profile:{revision:3}}});
 const ui=sandbox.createDraftUI({current:c=>c.user===user&&c.epoch===epoch,article:t=>{const e=new Element();e.textContent=t;return e;},conversation:()=>conversation,api:async(c,p,m='GET',b)=>{
 calls.push({p,m,b}); if(m==='POST'){if(hold)await new Promise(r=>release=r); if(error)throw error;return response(b.request_id);}
 if(p.includes('/turns/'))return response(p.split('/').at(-1));
 if(p==='profile')return {revision:3,agents:[{id:'writer',name:'Writer',enabled:true}]};
 return {kind:'draft_delivery',task_id:task.id,items:[],next_cursor:null};}});
 const render=()=>{ui.update(context(),[task]);const e=new Element();ui.attach(e,context(),task);conversation.append(e);return e;};
 const buttons=t=>walk(conversation).filter(e=>e.textContent===t);
 return {ui,calls,storage,conversation,buttons,confirm:v=>consent=v,timeout:()=>{const t=[...timers.values()].find(t=>t.ms===90000);assert(t);t.fn();},task:()=>task,open:async()=>{const e=render();await e.children[0]?.onclick();},click:async t=>buttons(t).at(-1).onclick(),posts:()=>calls.filter(c=>c.m==='POST'),error:e=>error=e,stale:()=>stale=true,truncated:()=>truncated=true,change:p=>{task={...task,...p};ui.update(context(),[task]);},hold:()=>hold=true,release:()=>{hold=false;release();},logout:()=>{epoch++;ui.reset();conversation.replaceChildren();},render};
}
(async()=>{
 const f=fixture();await f.open();assert.equal(f.posts().length,0);await f.click('让我的 Agent 起草（不交付）');assert.equal(f.posts().length,1);assert(f.calls.every(c=>!c.p.endsWith('/actions')));
 assert.equal(f.ui.reviewedText(f.task()),'');await f.click('使用此草稿填入人工交付');const input=walk(f.conversation).find(e=>e.value==='Generated <script> text');input.value='Human edited';assert.equal(f.ui.reviewedText(f.task()),'Human edited');assert.equal(f.posts().length,1);f.confirm(false);await f.click('重新读取私密草稿');assert.equal(f.ui.reviewedText(f.task()),'Human edited');f.confirm(true);f.change({revision:2});assert.equal(f.ui.reviewedText(f.task()),'');
 const s=fixture();s.stale();await s.open();await s.click('让我的 Agent 起草（不交付）');assert.equal(s.buttons('使用此草稿填入人工交付').length,0,'Live stale overrides saved stale=false');
 const trunc=fixture();trunc.truncated();await trunc.open();await trunc.click('让我的 Agent 起草（不交付）');assert(walk(trunc.conversation).some(e=>e.textContent.includes('模型未读取完整任务')));
 const other=fixture();other.change({owner:'alice'});assert.equal(other.render().children.length,0);
 const g=fixture();await g.open();const old=g.buttons('让我的 Agent 起草（不交付）')[0];g.error(Object.assign(Error('conflict'),{httpStatus:409}));await old.onclick();await old.onclick();assert.equal(g.posts().length,1);assert.equal(g.storage.size,1);await g.click('核对起草请求');assert.equal(g.posts().length,1);assert.equal(g.storage.size,0);
 for(const code of ['draft_task_stale_before_reservation','draft_task_state_before_reservation','draft_role_unavailable_before_reservation','draft_busy_before_reservation']){const k=fixture();await k.open();k.error(Object.assign(Error('known'),{httpStatus:409,code}));await k.click('让我的 Agent 起草（不交付）');assert.equal(k.storage.size,0);}
 const r=fixture();await r.open();await r.click('选择起草角色（可选）');await r.click('起草角色：Writer');await r.click('让我的 Agent 起草（不交付）');assert.equal(r.posts()[0].b.agent_id,'writer');assert.equal(r.posts()[0].b.expected_profile_revision,3);
 const busy=fixture();await busy.open();await busy.click('选择起草角色（可选）');await busy.click('起草角色：Writer');busy.error(Object.assign(Error('busy'),{httpStatus:409,code:'draft_busy_before_reservation'}));await busy.click('让我的 Agent 起草（不交付）');busy.error(null);await busy.click('让我的 Agent 起草（不交付）');assert.equal(busy.posts()[1].b.agent_id,'writer');
 const invalid=fixture();await invalid.open();await invalid.click('选择起草角色（可选）');await invalid.click('起草角色：Writer');invalid.error(Object.assign(Error('role'),{httpStatus:409,code:'draft_role_unavailable_before_reservation'}));await invalid.click('让我的 Agent 起草（不交付）');invalid.error(null);await invalid.click('让我的 Agent 起草（不交付）');assert.equal(invalid.posts().length,1);await invalid.click('明确使用默认管家起草');await invalid.click('让我的 Agent 起草（不交付）');assert.equal(invalid.posts().length,2);assert.equal(invalid.posts()[1].b.agent_id,undefined);
 const changed=fixture();await changed.open();changed.change({revision:2});await changed.click('让我的 Agent 起草（不交付）');assert.equal(changed.posts().length,0);
 const timed=fixture();await timed.open();timed.hold();const awaiting=timed.click('让我的 Agent 起草（不交付）');timed.timeout();await awaiting;assert.equal(timed.storage.size,1);assert(timed.buttons('核对起草请求').length);await timed.click('让我的 Agent 起草（不交付）');assert.equal(timed.posts().length,1);timed.release();
 const late=fixture();await late.open();late.hold();const p=late.click('让我的 Agent 起草（不交付）');late.logout();late.release();await p;assert.equal(late.conversation.children.length,0);
 console.log('Recipient draft UI privacy, manual-review, recovery, role, stale and identity tests passed');
})().catch(e=>{console.error(e);process.exitCode=1;});
