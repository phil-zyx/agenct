const assert=require('node:assert/strict'),H=require('./dist/engine.js');let count=0;function test(n,fn){fn();count++;console.log('PASS',n)}function ready(s='success'){let t=H.create('1','x','y',s);for(let a of ['accept','start','publish'])H.act(t,a,'Bob');return t;}
test('role protection',()=>assert.throws(()=>H.act(H.create('1','x','y'),'accept','访客')));
test('duplicate acceptance rejected',()=>{let t=H.create('1','x','y');H.act(t,'accept','Bob');assert.throws(()=>H.act(t,'accept','Bob'));});
test('full workflow and SHA binding',()=>{let t=ready();H.act(t,'verify','Bob');H.act(t,'approve','Alice');assert.equal(t.status,'completed');assert.equal(t.head,t.verified)});
test('stale SHA invalidates acceptance',()=>{let t=ready();H.act(t,'verify','Bob');H.act(t,'stale','Bob');assert.throws(()=>H.act(t,'approve','Alice'));});
test('CI failure blocked',()=>{let t=ready('ci_fail');H.act(t,'verify','Bob');assert.equal(t.status,'failed');assert.throws(()=>H.act(t,'approve','Alice'));});
test('timeout',()=>assert.equal(ready('timeout').status,'failed'));
test('unknown result reconciled not republished',()=>{let t=ready('unknown');assert.equal(t.status,'unknown');H.act(t,'reconcile','Bob');assert.equal(t.status,'verifying');assert.throws(()=>H.act(t,'reconcile','Bob'));});
test('cancel terminal',()=>{let t=ready();H.act(t,'cancel','Alice');assert.throws(()=>H.act(t,'verify','Bob'));});
test('return leads new attempt',()=>{let t=ready();H.act(t,'verify','Bob');H.act(t,'return','Alice');H.act(t,'start','Bob');assert.equal(t.attempt,2);assert.equal(t.verified,null)});console.log(count+' tests passed');
