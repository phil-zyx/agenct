const assert=require('node:assert/strict'),fs=require('node:fs'),vm=require('node:vm');
class Element {constructor(){this.children=[];this.textContent='';} append(e){this.children.push(e);}replaceChildren(...e){this.children=e;this.textContent='';}}
const walk=e=>[e,...e.children.flatMap(walk)];
function fixture(){
 const conversation=new Element(),calls=[],timers=new Map();let epoch=1,user='alice',hold=false,release,timer=0;
 let task={id:'shared',owner:'bob',requester:'alice',revision:2,status:'ready_for_acceptance'};
 let data={task:{...task},events:[{seq:1,actor:'alice',action:'created',at:'date',from_status:null,to_status:'awaiting_owner',revision:0,delivery_seq:null,provenance:'recorded'},{seq:2,actor:'bob',action:'accept',at:'date',from_status:'awaiting_owner',to_status:'working',revision:1,delivery_seq:null,provenance:'recorded'},{seq:3,actor:'bob',action:'deliver',at:'date',from_status:'working',to_status:'ready_for_acceptance',revision:2,delivery_seq:1,provenance:'recorded'}],deliveries:[{seq:1,author:'bob',content:'<script>shared only</script>',created_at:'date',task_revision:2}],audit:{schema_version:1,legacy_history_incomplete:false,legacy_notice:null}};
 const ctx=()=>({user,epoch,signal:new AbortController().signal});
 const sandbox=vm.createContext({document:{createElement:()=>new Element()},AbortController,setTimeout:(fn,ms)=>{const id=++timer;timers.set(id,{fn,ms});return id;},clearTimeout:id=>timers.delete(id)});
 for(const f of ['task-timeline-model.js','task-timeline-ui.js'])vm.runInContext(fs.readFileSync(__dirname+'/'+f,'utf8'),sandbox);
 const ui=sandbox.createTaskTimelineUI({api:async(c,p,m='GET')=>{calls.push({p,m});if(hold)await new Promise(r=>release=r);return structuredClone({...data,timeline:{events:data.events,deliveries:data.deliveries,audit:data.audit}});},current:c=>c.epoch===epoch&&c.user===user,article:t=>{const e=new Element();e.textContent=t;return e;},conversation:()=>conversation});
 const render=()=>{ui.update(ctx(),[task]);const e=new Element();ui.attach(e,ctx(),task);conversation.append(e);return e;};
 return {calls,conversation,render,ui,open:async()=>{const e=render();await e.children[0]?.onclick();},data:fn=>fn(data),change:p=>{task={...task,...p};ui.update(ctx(),[task]);},hold:()=>hold=true,release:()=>{hold=false;release();},timeout:()=>[...timers.values()][0].fn(),logout:()=>{epoch++;ui.reset();conversation.replaceChildren();},user:v=>user=v,text:()=>walk(conversation).map(e=>e.textContent).join('\n')};
}
(async()=>{
 const f=fixture();await f.open();assert(f.text().includes('<script>shared only</script>'));assert(f.text().includes('交付版本 1'));assert(f.calls.every(c=>c.m==='GET'&&c.p==='tasks/shared'));assert(!f.text().includes('draft-secret'));f.change({revision:3});assert(!f.text().includes('shared only'));assert(f.text().includes('旧时间线已隐藏'));
 const owner=fixture();owner.user('bob');await owner.open();assert(owner.text().includes('交付版本 1'));
 const stranger=fixture();stranger.user('mallory');assert.equal(stranger.render().children.length,0);assert.equal(stranger.calls.length,0);
 const leaked=fixture();leaked.data(d=>d.deliveries[0].source_draft_id='draft-secret');await leaked.open();assert(leaked.text().includes('未加载'));assert(!leaked.text().includes('draft-secret'));assert(!leaked.text().includes('shared only'));
 const actor=fixture();actor.data(d=>d.events[0].actor='mallory');await actor.open();assert(actor.text().includes('未加载'));
 const wrong=fixture();wrong.data(d=>d.task.id='other');await wrong.open();assert(wrong.text().includes('未加载'));
 const legacy=fixture();legacy.data(d=>{d.events=[];d.audit.legacy_history_incomplete=true;});await legacy.open();assert(legacy.text().includes('无法恢复'));assert(legacy.text().includes('无法核实对应历史事件'));
 const late=fixture();late.hold();const p=late.open();late.logout();late.release();await p;assert.equal(late.conversation.children.length,0);
 const race=fixture();race.hold();const r=race.open();race.change({revision:3});race.release();await r;assert(!race.text().includes('shared only'));
 const timed=fixture();timed.hold();const t=timed.open();timed.timeout();await t;assert(timed.text().includes('读取超时'));timed.release();
 console.log('PASS timeline UI participant scope, public-only DTO, stale/identity fences, legacy notice and timeout');
})().catch(e=>{console.error(e);process.exitCode=1;});
