const fs=require('fs'),vm=require('vm'),assert=require('assert'),{webcrypto}=require('crypto');
const source=fs.readFileSync(require('path').join(__dirname,'../ui/index.html'),'utf8');
const action=source.split('\n').find(l=>l.trimStart().startsWith('function billingAction('));
const nodes={},posts=[];let connected=true;
const ctx=vm.createContext({billingPending:false,crypto:{getRandomValues:webcrypto.getRandomValues.bind(webcrypto)},Uint32Array,Array,$:id=>nodes[id]||(nodes[id]={}),post:(cmd,args)=>{posts.push({cmd,...args});return connected}});
vm.runInContext(action,ctx);vm.runInContext('billingAction("trial")',ctx);assert.equal(posts.length,1);assert.match(posts[0].request_id,/^[0-9a-f]{32}$/);vm.runInContext('billingAction("trial")',ctx);assert.equal(posts.length,1);ctx.billingPending=false;vm.runInContext('billingAction("purchase",{plan:"annual"})',ctx);assert.notEqual(posts[0].request_id,posts[1].request_id);assert.equal(posts[1].plan,'annual');ctx.billingPending=false;connected=false;vm.runInContext('billingAction("restore")',ctx);assert.equal(ctx.billingPending,false);assert(nodes['billing-status'].textContent.includes('本机连接不可用'));
console.log('PASS billing UI works without crypto.randomUUID, prevents duplicate clicks and releases pending state on disconnected transport');
