// Exercise the shipped UI functions, keeping selections across page changes.
const fs=require('fs'),vm=require('vm'),assert=require('assert');
const source=fs.readFileSync(require('path').join(__dirname,'../ui/index.html'),'utf8');
const functionSource=name=>source.split('\n').find(l=>l.trimStart().startsWith('function '+name+'(')).trim();
const state={active:'junk',scanning:false,category:'cache',query:'owned',data:{selectable_total:151},entries:[],selected:new Set(),excludedIds:new Set(),allSelected:false};
const nodes=new Map(),posts=[];const $=id=>{if(!nodes.has(id))nodes.set(id,{value:'',disabled:false});return nodes.get(id)};
const context=vm.createContext({state,$,post:(cmd,payload)=>posts.push({cmd,...payload}),renderEntries(){},toast(){},Set});
vm.runInContext(['toggleSelection','selectAll','moveSelected','spaceCleanup','updateSelection'].map(functionSource).join('\n'),context);
vm.runInContext('selectAll();toggleSelection("page2-item",false);updateSelection()',context);
assert(state.allSelected);assert.equal(state.excludedIds.size,1);assert.equal($('selection-note').textContent,'已选当前筛选 150 项可处理项');
state.entries=[{id:'page3-item'}];vm.runInContext('moveSelected()',context);
assert.deepEqual(JSON.parse(JSON.stringify(posts.pop())),{cmd:'move',module:'junk',selection:{category:'cache',query:'owned',exclude_ids:['page2-item']}});
vm.runInContext('toggleSelection("page2-item",true)',context);assert.equal(state.excludedIds.size,0);
state.active='space';state.spaceRequestId=7;state.spacePath='/fixture';state.spaceShowHidden=true;
vm.runInContext('toggleSelection("/fixture/A",false);spaceCleanup()',context);
let p=posts.pop();assert.equal(p.selection.exclude_ids[0],'/fixture/A');
vm.runInContext('spaceCleanup(["/fixture/right-click"])',context);p=posts.pop();assert(!p.selection);assert.equal(p.ids[0],'/fixture/right-click');
state.scanning=true;let n=posts.length;vm.runInContext('spaceCleanup()',context);assert.equal(posts.length,n);
console.log('PASS all-except-one stays across pages; bulk and context target scopes are separate; scanning blocks mutation plans');
