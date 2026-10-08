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
console.log('PASS: delayed saves preserve OFF, exclusion additions/removals, and subsequent settings');
