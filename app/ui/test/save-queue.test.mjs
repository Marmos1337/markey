import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
const context={};vm.createContext(context);vm.runInContext(readFileSync(new URL('../save-queue.js',import.meta.url),'utf8'),context);
let backend={enabled:true,excluded_apps:[],sensitivity:'balanced'};
let release;const delayed=new Promise(resolve=>release=resolve);let writes=0;const submitted=[];
const writer=context.RekeyConfigWriter({
 readState:async()=>structuredClone(backend),toConfig:s=>s,onState:()=>{},
 writeState:async config=>{submitted.push(structuredClone(config));if(++writes===1)await delayed;backend=structuredClone(config);},
});
const off=writer.enqueue({enabled:false});
const add=writer.enqueue(s=>({excluded_apps:[...s.excluded_apps,'com.apple.TextEdit']}));
const sensitivity=writer.enqueue({sensitivity:'eager'});
await new Promise(resolve=>setImmediate(resolve));assert.equal(writes,1);release();await Promise.all([off,add,sensitivity]);
assert.equal(backend.enabled,false);assert.deepEqual(backend.excluded_apps,['com.apple.TextEdit']);assert.equal(backend.sensitivity,'eager');
assert(submitted.every(s=>s.enabled===false));
await Promise.all([writer.enqueue(s=>({excluded_apps:[...s.excluded_apps,'org.mozilla.firefox']})),writer.enqueue(s=>({excluded_apps:s.excluded_apps.filter(id=>id!=='com.apple.TextEdit')}))]);
assert.deepEqual(backend.excluded_apps,['org.mozilla.firefox']);assert.equal(backend.enabled,false);
// A cloud merge lands while a save is still queued: the save sees the merge, and the reload that follows shows the result of both.
let seen=null;let hold;const gate=new Promise(resolve=>hold=resolve);let store={exceptions:['a'],sensitivity:'balanced'};
const later=context.RekeyConfigWriter({readState:async()=>structuredClone(store),toConfig:s=>s,onState:s=>{seen=s;},writeState:async config=>{await gate;store=structuredClone(config);}});
const edit=later.enqueue({sensitivity:'eager'});store.exceptions=['a','from-cloud'];const reload=later.reload();
await new Promise(resolve=>setImmediate(resolve));hold();await Promise.all([edit,reload]);
assert.deepEqual(seen,{exceptions:['a','from-cloud'],sensitivity:'eager'});
// A cloud merge lands between the read and the write: Rust refuses, the change is made again on the merged settings.
let shared={exceptions:['a'],sensitivity:'balanced'},refused=0,sent=null;
const racing=context.RekeyConfigWriter({readState:async()=>structuredClone(shared),toConfig:s=>s,onState:()=>{},
 writeState:async config=>{if(refused++===0){shared.exceptions=['a','from-cloud'];throw 'stale-config';}sent=structuredClone(config);shared=structuredClone(config);}});
await racing.enqueue({sensitivity:'eager'});
assert.deepEqual(sent,{exceptions:['a','from-cloud'],sensitivity:'eager'});
// It gives up after a few tries and reports the failure instead of spinning.
const stuck=context.RekeyConfigWriter({readState:async()=>({}),toConfig:s=>s,onState:()=>{},writeState:async()=>{throw 'stale-config';}});
await assert.rejects(stuck.enqueue({}),e=>e==='stale-config');
console.log('PASS: delayed saves preserve OFF, exclusion additions/removals, and subsequent settings');
