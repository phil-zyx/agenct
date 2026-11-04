(function(root){
const labels={awaiting_owner:'待负责人接单',queued:'排队中',working:'执行中',verifying:'证据核验',ready:'待派活人验收',completed:'已完成',failed:'执行失败',unknown:'待人工核对',canceled:'已取消'};
const time=()=>new Date().toISOString();
function event(t,message,actor='系统'){t.events.push({at:time(),message,actor});}
function create(id,title,description,scenario='success'){const t={id,title,description,scenario,status:'awaiting_owner',requester:'Alice',owner:'Bob',version:1,attempt:0,head:null,verified:null,cost:0,mr:null,events:[]};event(t,'明确派活已登记 · 合同 v1','Alice');return t;}
function act(t,action,actor){const need=(role)=>{if(actor!==role)throw Error('当前身份无权执行此操作，请切换到'+role);};
if(action==='accept'){need(t.owner);if(t.status!=='awaiting_owner')throw Error('任务已接单或状态已变化');t.status='queued';event(t,'接单授权已绑定合同 v'+t.version,actor);}
else if(action==='start'){need(t.owner);if(!['queued','failed'].includes(t.status))throw Error('此状态不能启动');t.attempt++;t.status='working';t.head='demo-'+t.id+'-a'+t.attempt;t.verified=null;event(t,'签发有效租约 · attempt '+t.attempt+' · 隔离模拟执行');}
else if(action==='publish'){need(t.owner);if(t.status!=='working')throw Error('当前未在执行');t.cost=+(t.cost+.08).toFixed(2);if(t.scenario==='timeout'){t.status='failed';event(t,'执行超时，停止新动作；可修改模拟场景后重试');}else if(t.scenario==='unknown'){t.status='unknown';event(t,'发布回执丢失 · 暂停重试，等待核对外部结果');}else{t.mr=100+Number(t.id);t.status='verifying';event(t,'受控发布模块创建模拟 MR !'+t.mr+' · 无真实 GitLab 写入');}}
else if(action==='verify'){need(t.owner);if(t.status!=='verifying')throw Error('没有可核验产物');if(t.scenario==='ci_fail'){t.status='failed';event(t,'当前 SHA 的模拟 CI 失败，禁止进入待验收');}else{t.verified=t.head;t.status='ready';event(t,'独立模拟核验通过 · 仓库/分支/SHA/CI 一致');}}
else if(action==='approve'){need(t.requester);if(t.status!=='ready'||t.head!==t.verified)throw Error('证据未通过或 SHA 已变化，不能验收');t.status='completed';event(t,'验收通过 · 合同 v'+t.version+' · SHA '+t.head+'；未合并',actor);}
else if(action==='return'){need(t.requester);if(t.status!=='ready')throw Error('只能退回待验收任务');t.status='queued';t.verified=null;event(t,'退回修改 · 新 attempt 需重新核验',actor);}
else if(action==='stale'){need(t.owner);if(t.status!=='ready')throw Error('仅待验收任务支持此演示');t.head+='-new';t.verified=null;t.status='verifying';event(t,'模拟提交更新 · 旧 SHA 核验失效');}
else if(action==='reconcile'){need(t.owner);if(t.status!=='unknown')throw Error('无待核对动作');t.mr=100+Number(t.id);t.status='verifying';event(t,'查询模拟外部记录：MR 已存在，复用原 MR，不重复创建');}
else if(action==='cancel'){if(![t.requester,t.owner].includes(actor))throw Error('无权取消');if(['completed','canceled'].includes(t.status))throw Error('任务已经终止');t.status='canceled';event(t,'撤销后续执行授权；已生成的模拟产物保留',actor);}
else throw Error('未知操作');return t;}
const api={labels,create,act};if(typeof module!=='undefined')module.exports=api;root.Hub=api;
})(typeof window!=='undefined'?window:globalThis);
