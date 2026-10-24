// Regression: a delayed snapshot restore must never replace a newer scan.
const fs=require('fs'),path=require('path'),vm=require('vm'),assert=require('assert');
const source=fs.readFileSync(path.join(__dirname,'../ui/index.html'),'utf8');
const a=source.indexOf('function receive(e)'),b=source.indexOf("\n      $('entry-category').addEventListener",a);
assert(a>=0&&b>a);const receive=source.slice(a,b);
for(const requestId of [1,999]){
 const state={active:'space',scanning:true,activeRequestId:1,data:{fresh:true}};
 vm.runInNewContext(receive+';receive(event)',{state,moduleInfo:{space:{}},saveLocal(){},loadLocal(){throw Error('unexpected switch')},paintResult(){throw Error('late restore must not repaint')},event:{event:'session',module:'space',request_id:requestId,scanning:false,data:{restored:true}}});
 assert(state.scanning&&state.data.fresh&&state.activeRequestId===1);
}
console.log('PASS delayed restore does not overwrite active scan, including identical request ID');
