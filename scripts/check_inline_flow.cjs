const fs=require('fs'),vm=require('vm'),assert=require('assert');
const source=fs.readFileSync(require('path').join(__dirname,'../ui/index.html'),'utf8');
const fn=source.split('\n').find(l=>l.trimStart().startsWith('function startSmartFlow('));
const posts=[];const ctx=vm.createContext({smartFlow:{phase:'idle',running:false},smartPending:false,smartReleasePending:false,Date,post:(cmd,payload)=>{posts.push({cmd,...payload});return true},renderSmartFlow(){}});
vm.runInContext(fn,ctx);vm.runInContext('startSmartFlow()',ctx);assert.equal(posts.length,1);assert.equal(posts[0].cmd,'smart_clean');assert(ctx.smartPending);vm.runInContext('startSmartFlow()',ctx);assert.equal(posts.length,1);
ctx.smartPending=false;ctx.smartFlow={phase:'done',running:false,record_ids:['exact-batch']};vm.runInContext('startSmartFlow()',ctx);assert.equal(posts[1].cmd,'purge');assert.deepEqual(posts[1].ids,['exact-batch']);assert.equal(posts[1].smart_flow,true);
assert(!fn.includes('setModule')&&!fn.includes('smart-details'));assert(source.includes('.smart-active #results-panel'));const top=source.split('class="top-actions"')[1].split('</div>')[0];assert(!/id="(?:assistant-global|ai-settings-top|schedule-open)"/.test(top));
const main=fs.readFileSync(require('path').join(__dirname,'../src/main.rs'),'utf8');assert(main.includes('if purge && !inline_confirmed && !confirm("永久清理这些文件？"'));console.log('PASS one in-place action, duplicate clicks blocked, batch-specific release requires explicit confirmation');

assert(source.includes('post("purge_preview",payload)'));assert(source.includes("inline_confirmed:true"));assert(source.includes("inline-cancel"));assert(!fn.includes("smartReleasePending=true"));console.log("PASS release preview cannot auto-confirm permanent deletion");
