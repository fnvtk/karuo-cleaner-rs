//! Goal planning is read-only. A shortfall never grants permission to delete more data.
use crate::*;
use std::sync::OnceLock;
fn latest()->&'static Mutex<Value>{static V:OnceLock<Mutex<Value>>=OnceLock::new();V.get_or_init(||Mutex::new(json!({"event":"goal_analysis","phase":"idle"})))}
fn active()->&'static Mutex<Option<Arc<AtomicBool>>>{static V:OnceLock<Mutex<Option<Arc<AtomicBool>>>>=OnceLock::new();V.get_or_init(Default::default)}
fn publish(proxy:&EventLoopProxy<UiEvent>,v:Value){*latest().lock().unwrap()=v.clone();emit(proxy,v)}
pub fn status(proxy:&EventLoopProxy<UiEvent>){emit(proxy,latest().lock().unwrap().clone())}
pub fn target(total:u64,available:u64,percent:u8)->Value{let required=(u128::from(total)*u128::from(percent)/100).min(u64::MAX as u128)as u64;json!({"percent":percent,"total_bytes":total,"available_bytes":available,"target_bytes":required,"shortfall_bytes":required.saturating_sub(available),"target_reached":available>=required})}
pub fn context()->Value{let disk=disk();let p=guardian::goal().map(|g|g.percent).unwrap_or(10);target(disk["total"].as_u64().unwrap_or(0),disk["available"].as_u64().unwrap_or(0),p)}
pub fn fit(entries:Vec<Entry>,needed:u64)->Vec<Entry>{let mut bytes=0u64;entries.into_iter().take_while(|e|{if bytes>=needed{return false}bytes=bytes.saturating_add(e.size);true}).collect()}
pub fn start(proxy:EventLoopProxy<UiEvent>){
 let target=context();let percent=target["percent"].as_u64().unwrap_or(10);let token=Arc::new(AtomicBool::new(false));if let Some(old)=active().lock().unwrap().replace(token.clone()){old.store(true,Ordering::SeqCst)}
 publish(&proxy,json!({"event":"goal_analysis","phase":"scan","percent":percent,"target":target}));
 thread::spawn(move||{let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||->Result<Value,String>{
  let preferences=guardian::load()?;let restored=guardian::recovered()?;
  let scan=engine::scan_junk_incremental(&token,|_|{},|_|{});if token.load(Ordering::SeqCst){return Err("目标已切换".into())}
  let mut safe=Vec::new();let mut review=Vec::new();
  for e in scan.entries{if restored.iter().any(|p|Path::new(&e.path).starts_with(p))||safety::validate_junk_entry(&e).is_err(){continue}match e.kind.as_str(){"cache" if preferences.clean_cache=>safe.push(e),"log" if preferences.clean_logs=>safe.push(e),"developer_cache"=>review.push(e),_=>{}}}
  safe.sort_by(|a,b|b.size.cmp(&a.size));let safe_bytes=safe.iter().fold(0u64,|n,e|n.saturating_add(e.size));let review_bytes=review.iter().fold(0u64,|n,e|n.saturating_add(e.size));let needed=target["shortfall_bytes"].as_u64().unwrap_or(0);let chosen=fit(safe,needed);
  if token.load(Ordering::SeqCst){return Err("目标已切换".into())}
  publish(&proxy,json!({"event":"goal_analysis","phase":"ai","percent":percent,"target":target}));
  let prompt=format!("目标保持{percent}%可用空间。当前可用{}字节，还差{needed}字节；已验证旧缓存日志共{safe_bytes}字节，另有{review_bytes}字节工具下载缓存须手动确认。请按目标缺口分析方案，不能假称达到目标或删除资料；有安全候选时返回cleanup且scope=recommended。用一句话说明差距及下一步。",target["available_bytes"]);
  let answer=analysis::chat(&chosen,"smart",&prompt,&[],json!({"space_goal":target,"goal_analysis":true}));
  if token.load(Ordering::SeqCst){return Err("目标已切换".into())}
  let mut proposal=answer["proposals"].as_array().into_iter().flatten().find(|p|p["kind"]=="cleanup").cloned().unwrap_or(Value::Null);
  if proposal.is_null()&&answer["source"]!="ai"&&!chosen.is_empty(){let bytes=chosen.iter().fold(0u64,|n,e|n.saturating_add(e.size));proposal=json!({"id":analysis::retain_cleanup_review(chosen),"kind":"cleanup","bytes":bytes});}
  let review_id=if needed>safe_bytes&&!review.is_empty(){analysis::retain_cleanup_review(review)}else{String::new()};
  Ok(json!({"event":"goal_analysis","phase":"done","percent":percent,"target":target,"safe_bytes":safe_bytes,"review_bytes":review_bytes,"remaining_after_safe":needed.saturating_sub(safe_bytes),"source":answer["source"],"reply":answer["reply"],"error":answer["error"],"usage":answer["usage"],"proposal":proposal,"review_proposal_id":review_id,"scanned":scan.scanned,"executed":false}))
 }));
 if token.load(Ordering::SeqCst){return}
 match result{Ok(Ok(v))=>publish(&proxy,v),Ok(Err(e))=>publish(&proxy,json!({"event":"goal_analysis","phase":"failed","percent":percent,"error":e})),Err(_)=>publish(&proxy,json!({"event":"goal_analysis","phase":"failed","percent":percent,"error":"目标分析中断，请重试"}))}
 });
}
#[cfg(test)]mod tests{use super::*;
 #[test]fn target_is_real_available_space_not_a_deletion_quota(){let a=target(1000,150,10);assert_eq!(a["shortfall_bytes"],0);assert_eq!(target(1000,150,20)["shortfall_bytes"],50);assert_eq!(target(1000,150,30)["shortfall_bytes"],150);assert_eq!(target(u64::MAX,0,30)["target_bytes"],(u128::from(u64::MAX)*30/100)as u64);}
 #[test]fn fitting_never_invents_files_to_reach_goal(){assert!(fit(vec![],100).is_empty());}
}
