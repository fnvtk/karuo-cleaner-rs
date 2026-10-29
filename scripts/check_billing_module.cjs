// 在隔离 DOM 中验证自有模块关闭与恢复，不触碰用户账本。
const fs=require('fs'),vm=require('vm'),assert=require('assert');
const source=fs.readFileSync(require('path').join(__dirname,'../ui/index.html'),'utf8');
const nodes={};const $=id=>nodes[id]||(nodes[id]={dataset:{},replaceChildren(){},append(){},querySelector(){return $('demo-option')}});
const ctx=vm.createContext({$,billingPending:false,document:{querySelectorAll:()=>[$('topup')],createElement:()=>({})},Boolean,Number,String,Date,toast(){}});
vm.runInContext(source.split('\n').find(l=>l.trimStart().startsWith('function renderBilling(')),ctx);
ctx.e={module_disabled:true,credits:0,records:[]};vm.runInContext('renderBilling(e)',ctx);
assert.equal($('billing-sidebar').hidden,true);assert.equal($('billing-trial').disabled,true);assert.equal($('billing-purchase').disabled,true);assert.equal($('assistant-billing').value,'byok');assert.equal($('demo-option').disabled,true);assert.match($('billing-module').textContent,/恢复/);
ctx.e={module_disabled:false,credits:100,trial_started:true,trial_days_left:7,trial_expires_at:1800000000,records:[]};vm.runInContext('renderBilling(e)',ctx);
assert.equal($('billing-sidebar').hidden,false);assert.equal($('billing-purchase').disabled,false);assert.equal($('billing-trial').disabled,true);assert.match($('billing-status').textContent,/到期/);assert.equal($('demo-option').disabled,false);
console.log('PASS module disable/restore UI, trial history and expiration display');
