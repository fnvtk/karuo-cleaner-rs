const fs=require('fs'),vm=require('vm'),assert=require('assert');
const source=fs.readFileSync(require('path').join(__dirname,'../ui/index.html'),'utf8');
const func=n=>source.split('\n').find(l=>l.trimStart().startsWith('function '+n+'(')).trim();
const state={active:'junk',activeRequestId:31,category:'cache',query:'',selected:new Set(),excludedIds:new Set(),allSelected:false};
const nodes={},posts=[],timers=new Map(),conversations=new Map();let timerId=0,connected=true;
const ctx=vm.createContext({optTab:"running",state,conversations,assistantOpen:true,chatSequence:0,Date,Set,$:id=>nodes[id]||(nodes[id]={value:''}),post:(cmd,p)=>{posts.push({cmd,...p});return connected},openAssistant(){},renderConversation(){},clearTimeout:id=>timers.delete(id),setTimeout:fn=>{timers.set(++timerId,fn);return timerId}});
vm.runInContext(['conversation','finishChat','sendChat','invalidateChat','applyProposal'].map(func).join('\n'),ctx);
vm.runInContext('sendChat("分析当前选择")',ctx);let c=conversations.get('junk'),first=c.requestId;assert(c.pending);
vm.runInContext('finishChat(event)',vm.createContext({optTab:"running",...ctx,event:{module:'junk',chat_request_id:'wrong',reply:'late'}}));assert(c.pending);
ctx.event={module:'junk',chat_request_id:first,source:'rules',error:'选择已失效',reply:'未执行清理',proposals:[{kind:'cleanup'}]};vm.runInContext('finishChat(event)',ctx);assert(!c.pending);assert.equal(c.messages.at(-1).proposals.length,0);assert.equal(timers.size,0);
vm.runInContext('sendChat("再试一次")',ctx);let second=c.requestId;assert.notEqual(second,first);ctx.event={module:'junk',chat_request_id:first,reply:'old reply'};vm.runInContext('finishChat(event)',ctx);assert(c.pending&&c.requestId===second);
[...timers.values()][0]();assert(!c.pending);ctx.event={module:'junk',chat_request_id:second,reply:'late after timeout',proposals:[{kind:'cleanup'}]};const count=c.messages.length;vm.runInContext('finishChat(event)',ctx);assert.equal(c.messages.length,count);
connected=false;vm.runInContext('sendChat("连接失败")',ctx);assert(!c.pending);assert.equal(timers.size,0);assert(c.messages.at(-1).text.includes('未发送'));
connected=true;vm.runInContext('sendChat("准备切换")',ctx);c.messages[0].proposals=[{kind:'cleanup'}];vm.runInContext('invalidateChat("junk","已重新扫描")',ctx);assert(!c.pending);assert(c.messages.every(m=>!m.proposals?.length));assert.equal(timers.size,0);
vm.runInContext('applyProposal({kind:"restore",id:"bound-receipt"})',ctx);assert.equal(posts.at(-1).cmd,'undo_last');assert.equal(posts.at(-1).proposal_id,'bound-receipt');
console.log('PASS failed selection, delayed reply, timeout, retry and missing connection all terminate the correct chat without actions');

vm.runInContext('sendChat("帮我清理")',ctx);const cleanupRequest=c.requestId;ctx.event={module:'junk',chat_request_id:cleanupRequest,source:'ai',reply:'已准备清单',proposals:[{kind:'cleanup',id:'verified-proposal'}]};vm.runInContext('finishChat(event)',ctx);assert.equal(posts.at(-1).cmd,'ai_chat');assert.equal(c.messages.at(-1).proposals[0].id,'verified-proposal');vm.runInContext('applyProposal({kind:"cleanup",id:"verified-proposal"})',ctx);assert.equal(posts.at(-1).cmd,'ai_proposal_plan');assert.equal(posts.at(-1).proposal_id,'verified-proposal');assert(!posts.some(p=>p.cmd==='cleanup_apply'||p.cmd==='move'));assert(source.includes('e.proposal_id===stageAllRecommended'));console.log('PASS AI reply stays in conversation; explicit user action opens the exact proposal; no cleanup is executed by the reply');

state.active='optimize';state.selectedPids=new Set([123]);ctx.optTab='running';ctx.selectedStartup=new Set(['/fixture/startup']);
vm.runInContext('sendChat("分析运行应用")',ctx);const running=conversations.get('optimize:running'),runningId=running.requestId;assert.deepEqual([...posts.at(-1).management_ids],[123]);assert.equal(posts.at(-1).subpage,'running');
ctx.optTab='startup';vm.runInContext('sendChat("分析登录项")',ctx);const startup=conversations.get('optimize:startup'),startupId=startup.requestId;assert.equal(posts.at(-1).subpage,'startup');
ctx.event={module:'optimize',subpage:'running',chat_request_id:runningId,source:'ai',reply:'运行应用建议',proposals:[{kind:'management',subpage:'running',id:'running-plan'}]};vm.runInContext('finishChat(event)',ctx);assert(!running.pending);assert(startup.pending);assert.equal(running.messages.at(-1).text,'运行应用建议');assert(!startup.messages.some(m=>m.text==='运行应用建议'));
ctx.event={module:'optimize',subpage:'startup',chat_request_id:startupId,source:'ai',reply:'登录项建议',proposals:[]};vm.runInContext('finishChat(event)',ctx);assert(!startup.pending);assert.equal(startup.messages.at(-1).text,'登录项建议');assert(!posts.some(p=>p.cmd==='management_apply'));
console.log('PASS running and startup requests, replies and pending state remain in their original subpages without execution');
