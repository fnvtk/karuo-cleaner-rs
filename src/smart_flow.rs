//! 首页单步清理：只处理新扫描的旧缓存/日志；空间读数与移入废纸篓体积分开。
use crate::*;
use std::sync::OnceLock;
use std::time::{Duration,Instant};
fn snapshot()->PathBuf{home().join("Library/Application Support/KaruoCleaner/smart-clean-last.json")}
fn current()->&'static Mutex<Value>{static S:OnceLock<Mutex<Value>>=OnceLock::new();S.get_or_init(||{let saved=if schedule::safe_path(&snapshot()).is_ok(){std::fs::read(snapshot()).ok().and_then(|b|serde_json::from_slice::<Value>(&b).ok()).filter(|v|v["event"]=="smart_clean"&&v["running"]==false&&matches!(v["phase"].as_str(),Some("done"|"released"|"restored"|"failed")))}else{None};Mutex::new(saved.unwrap_or_else(||json!({"event":"smart_clean","phase":"idle","running":false})))})}
fn publish(proxy:&EventLoopProxy<UiEvent>,value:Value){if value["running"]==false&&value["phase"]!="idle"{let _=schedule::atomic_json(&snapshot(),&value);}*current().lock().unwrap()=value.clone();emit(proxy,value)}
pub fn status(proxy:&EventLoopProxy<UiEvent>){emit(proxy,current().lock().unwrap().clone())}
pub fn start(state:Arc<Mutex<State>>,proxy:EventLoopProxy<UiEvent>,busy:Arc<AtomicBool>){
 if busy.compare_exchange(false,true,Ordering::SeqCst,Ordering::SeqCst).is_err(){message(&proxy,"正在处理文件，请稍后再整理。",true);return}
 publish(&proxy,json!({"event":"smart_clean","phase":"scan","running":true,"scanned":0,"processed":0,"total":0}));
 thread::spawn(move||{let _guard=BusyReset(busy);let started=Instant::now();let before=disk()["available"].as_u64();
 let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||->Result<(),String>{
 let restored=guardian::recovered()?;
 let cancel=AtomicBool::new(false);let last=std::cell::Cell::new(Instant::now());
 let scan=engine::scan_junk_incremental(&cancel,|_|{},|r|{if last.get().elapsed()>Duration::from_millis(300){publish(&proxy,json!({"event":"smart_clean","phase":"scan","running":true,"scanned":r.scanned,"elapsed_ms":started.elapsed().as_millis() as u64}));last.set(Instant::now())}});
 let mut entries:Vec<_>=scan.entries.into_iter().filter(|e|matches!(e.kind.as_str(),"cache"|"log")&&!restored.iter().any(|p|Path::new(&e.path).starts_with(p))).collect();
 let mut ai_advice=String::new();let mut ai_source="rules";
 if !entries.is_empty()&&analysis::config_status()["configured"]==true {
 publish(&proxy,json!({"event":"smart_clean","phase":"ai","running":true,"total":entries.len(),"elapsed_ms":started.elapsed().as_millis() as u64}));
 let review=analysis::chat(&entries,"smart","请审核这些本机已验证的旧缓存和旧日志，安全时生成cleanup清单且scope=recommended。重要资料保留；用一句话说明。",&[],json!({}));
 if review["source"]=="ai" {ai_source="ai";ai_advice=review["reply"].as_str().unwrap_or("").chars().take(100).collect();let selected=review["proposals"].as_array().into_iter().flatten().find(|p|p["kind"]=="cleanup").and_then(|p|p["id"].as_str()).and_then(|id|analysis::proposal(id).ok());match selected {Some(analysis::Proposal::Cleanup(chosen))=>entries.retain(|e|chosen.iter().any(|c|c.id==e.id)),_=>entries.clear()}}
 else {ai_advice="AI暂不可用，已采用本机保护规则。".into()}
 }
 let total=entries.len();let service=service(&state);let mut tick=Instant::now();
 publish(&proxy,json!({"event":"smart_clean","phase":"check","running":true,"total":total,"processed":0,"scanned":scan.scanned,"elapsed_ms":started.elapsed().as_millis() as u64}));
 let report=service.move_entries_with_progress(&entries,|processed,count,bytes,phase|{if tick.elapsed()>Duration::from_millis(250)||processed==count{publish(&proxy,json!({"event":"smart_clean","phase":phase,"running":true,"processed":processed,"total":count,"moved_bytes":bytes,"elapsed_ms":started.elapsed().as_millis() as u64}));tick=Instant::now()}});
 let paths:Vec<_>=report.records.iter().map(|r|r.original_path.clone()).collect();let bytes=report.records.iter().fold(0u64,|n,r|n.saturating_add(r.size));let ids:Vec<_>=report.records.iter().map(|r|r.id.clone()).collect();
 {let mut data=state.lock().unwrap();if !report.records.is_empty(){data.last_moved=report.records.clone()}data.approved.retain(|_,e|!paths.iter().any(|p|e.path==*p||Path::new(&e.path).starts_with(p)));data.sessions.remove_paths(&paths);data.space_cache.lock().unwrap().remove_paths(&paths);space::snapshot::request_save(data.space_cache.clone());let _=data.sessions.save(&home().join("Library/Application Support/KaruoCleaner/scan-sessions.json"));}
 let after=disk()["available"].as_u64();let delta=before.zip(after).map(|(b,a)|i128::from(a)-i128::from(b));
 publish(&proxy,json!({"event":"smart_clean","phase":"done","running":false,"total":total,"processed":total,"moved_count":report.records.len(),"moved_bytes":bytes,"record_ids":ids,"skipped":scan.skipped+report.failures.len() as u64,"elapsed_ms":started.elapsed().as_millis() as u64,"available_before":before,"available_after":after,"available_delta":delta,"ai_source":ai_source,"ai_advice":ai_advice,"advice":if bytes>0{"旧缓存和日志已整理，重要资料保留。"}else{"暂未找到可安全整理的旧缓存和日志。"},"errors":report.failures.iter().take(4).collect::<Vec<_>>()}));
 ready(&proxy);Ok(())}));
 let error=match result{Ok(Ok(()))=>None,Ok(Err(e))=>Some(e),Err(_)=>Some("本次整理中断，已保留恢复记录。".into())};
 if let Some(error)=error{publish(&proxy,json!({"event":"smart_clean","phase":"failed","running":false,"error":error,"elapsed_ms":started.elapsed().as_millis() as u64}))}
 });
}

pub fn released(proxy:&EventLoopProxy<UiEvent>,delta:i128,remaining:Vec<String>,errors:&[String]){let mut v=current().lock().unwrap().clone();v["phase"]=json!(if remaining.is_empty(){"released"}else{"done"});v["record_ids"]=json!(remaining);v["available_delta"]=json!(delta);v["ai_advice"]=json!("");v["advice"]=json!(if errors.is_empty(){"永久释放已确认"}else{"部分文件保留，请查看清理历史。"});publish(proxy,v)}

pub fn reconcile(proxy:&EventLoopProxy<UiEvent>,records:&[TrashRecord]){let mut v=current().lock().unwrap().clone();if v["phase"]!="done"{return}let old:Vec<String>=v["record_ids"].as_array().into_iter().flatten().filter_map(|id|id.as_str().map(str::to_owned)).collect();let remaining:Vec<_>=old.iter().filter(|id|records.iter().any(|r|r.id==**id&&!r.restored)).cloned().collect();if remaining.len()!=old.len(){v["record_ids"]=json!(remaining);if v["record_ids"].as_array().is_some_and(|v|v.is_empty()){v["phase"]=json!("restored");v["ai_advice"]=json!("");v["advice"]=json!("本批文件已撤回。");}publish(proxy,v)}}
