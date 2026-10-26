//! 独立演示钱包：不创建订单、不修改真实账户，也不影响本地清理。
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs, path::Path, sync::Mutex, time::{SystemTime, UNIX_EPOCH}};
const DAY:u64=86400;
const TRIAL_DAYS:u64=7;
const TRIAL_CREDITS:i64=100_000;
const YEAR_PRICE:u64=198;
const LIFETIME_PRICE:u64=398;
const YEAR_CREDITS:i64=1_000_000;
const LIFETIME_CREDITS:i64=2_000_000;
const CREDITS_PER_YUAN:i64=100_000;
const INPUT_RATE:i64=1;
const OUTPUT_RATE:i64=3;
static LOCK:Mutex<()>=Mutex::new(());
#[derive(Default,Serialize,Deserialize)]
struct Wallet { demo:bool, trial_start:Option<u64>, plan:Option<String>, purchased_at:Option<u64>, milli_credits:i64, reminded:bool, receipts:BTreeSet<String>, records:Vec<Value> }
impl Wallet {
 fn full(&self,now:u64)->bool {self.plan.as_deref()==Some("lifetime")||(self.plan.as_deref()==Some("annual")&&self.purchased_at.is_some_and(|t|now<t+365*DAY))||self.trial_start.is_some_and(|t|now<t+TRIAL_DAYS*DAY)}
 fn view(&self,now:u64)->Value {let remaining=self.trial_start.map(|t|(t+TRIAL_DAYS*DAY).saturating_sub(now)).unwrap_or(0);json!({"event":"billing","demo":self.demo,"full":self.full(now),"plan":self.plan,"trial_started":self.trial_start.is_some(),"trial_days_left":remaining.div_ceil(DAY),"credits":self.milli_credits as f64/1000.0,"records":self.records,"prices":{"annual":YEAR_PRICE,"lifetime":LIFETIME_PRICE,"credits_per_yuan":CREDITS_PER_YUAN/1000},"notice":"演示模式，不会真实扣款。自填 API 费用由接口提供方收取。"})}
 fn apply(&mut self,r:&Value,now:u64)->Result<(),String>{
  let action=r["action"].as_str().unwrap_or("status");
  if action=="status"||action=="restore" {return Ok(())}
  if action=="mode" {self.demo=r["enabled"].as_bool().ok_or("请选择演示模式状态")?;return Ok(())}
  let id=r["request_id"].as_str().filter(|s|!s.is_empty()&&s.len()<=160).ok_or("缺少演示操作编号")?;
  if self.receipts.contains(id){return Ok(())}
  let detail=match action{
   "trial"=>{if self.trial_start.is_some(){return Err("此演示钱包已领取试用".into())}self.trial_start=Some(now);self.milli_credits+=TRIAL_CREDITS;json!({"kind":"trial","credits":100})},
   "purchase"=>{let plan=r["plan"].as_str().ok_or("请选择套餐")?;let (price,credits)=match plan{"annual"=>(YEAR_PRICE,YEAR_CREDITS),"lifetime"=>(LIFETIME_PRICE,LIFETIME_CREDITS),_=>return Err("未知套餐".into())};self.plan=Some(plan.into());self.purchased_at=Some(now);self.milli_credits=self.milli_credits.checked_add(credits).ok_or("演示余额过大")?;json!({"kind":"purchase","plan":plan,"yuan":price,"credits":credits/1000})},
   "usage"=>{if !self.demo{return Err("正常模式不记演示扣分".into())}if !self.full(now){return Err("演示试用已结束，请查看套餐".into())}let input=r["usage"]["prompt_tokens"].as_i64().filter(|n|*n>=0);let output=r["usage"]["completion_tokens"].as_i64().filter(|n|*n>=0);let cost=if r["valid_result"]==true{match(input,output){(Some(i),Some(o))=>Some(i.checked_mul(INPUT_RATE).and_then(|v|o.checked_mul(OUTPUT_RATE).and_then(|o|v.checked_add(o))).ok_or("用量超出支持范围")?),_=>None}}else{Some(0)};if let Some(cost)=cost{if cost>self.milli_credits{return Err("演示积分不足，请充值后再发起分析".into())}self.milli_credits-=cost}json!({"kind":"usage","credits":cost.map(|c|-(c as f64)/1000.0),"usage_known":cost.is_some(),"valid_result":r["valid_result"]==true})},
   "topup"=>{let yuan=r["yuan"].as_u64().filter(|n|*n>=1&&*n<=1_000_000).ok_or("自定义金额须为1至1000000元的整数")?;let credits= (yuan as i64)*CREDITS_PER_YUAN;self.milli_credits=self.milli_credits.checked_add(credits).ok_or("演示余额过大")?;json!({"kind":"topup","yuan":yuan,"credits":credits/1000})},
   _=>return Err("未知演示操作".into())
  };self.receipts.insert(id.into());self.records.push(json!({"request_id":id,"at":now,"detail":detail}));Ok(())
 }
}
fn now()->u64{SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()}
fn load(path:&Path)->Result<Wallet,String>{crate::schedule::safe_path(path)?;match fs::read(path){Ok(b)=>serde_json::from_slice(&b).map_err(|e|format!("演示钱包读取失败：{e}")),Err(e)if e.kind()==std::io::ErrorKind::NotFound=>Ok(Wallet::default()),Err(e)=>Err(e.to_string())}}
pub fn handle(r:&Value)->Value{let result=(||{let _guard=LOCK.lock().map_err(|_|"演示钱包暂不可用")?;let path=crate::home().join("Library/Application Support/KaruoCleaner/billing-demo.json");let mut w=load(&path)?;let time=now();w.apply(r,time)?;let mut reminder=false;if w.demo&&!w.reminded&&w.trial_start.is_some_and(|t|time>=t+6*DAY&&time<t+7*DAY){w.reminded=true;reminder=true}if r["action"].as_str().unwrap_or("status")!="status"||reminder{crate::schedule::atomic_json(&path,&w)?}let mut v=w.view(time);v["reminder"]=json!(reminder);Ok::<Value,String>(v)})();match result{Ok(v)=>v,Err(e)=>json!({"event":"billing","error":e})}}
#[cfg(test)]mod tests{
 use super::*;
 #[test]fn trial_expires_and_cannot_repeat(){let mut w=Wallet::default();w.apply(&json!({"action":"trial","request_id":"t"}),100).unwrap();assert!(w.full(100+6*DAY));assert!(!w.full(100+7*DAY));assert_eq!(w.milli_credits,TRIAL_CREDITS);assert!(w.apply(&json!({"action":"trial","request_id":"t2"}),200).is_err());}
 #[test]fn purchases_and_topups_are_idempotent_after_reload(){let mut w=Wallet::default();let r=json!({"action":"purchase","plan":"annual","request_id":"p"});w.apply(&r,100).unwrap();let mut w:Wallet=serde_json::from_slice(&serde_json::to_vec(&w).unwrap()).unwrap();w.apply(&r,200).unwrap();assert_eq!(w.milli_credits,YEAR_CREDITS);assert!(!w.full(100+365*DAY));w.apply(&json!({"action":"topup","yuan":10,"request_id":"c"}),300).unwrap();assert_eq!(w.milli_credits,2_000_000);assert!(w.apply(&json!({"action":"topup","yuan":0,"request_id":"bad"}),300).is_err());}
 #[test]fn normal_mode_is_default_and_wallet_is_only_demo(){let w=Wallet::default();assert!(!w.demo);assert_eq!(w.view(0)["credits"],0.0);}
 #[test]fn usage_fractional_failure_unknown_and_replay(){let mut w=Wallet::default();w.apply(&json!({"action":"trial","request_id":"trial"}),100).unwrap();w.demo=true;let r=json!({"action":"usage","request_id":"u","valid_result":true,"usage":{"prompt_tokens":1,"completion_tokens":1}});w.apply(&r,101).unwrap();assert_eq!(w.milli_credits,99_996);w.apply(&r,102).unwrap();assert_eq!(w.milli_credits,99_996);for id in ["unknown","failed"]{w.apply(&json!({"action":"usage","request_id":id,"valid_result":id=="unknown","usage":null}),103).unwrap()}assert_eq!(w.milli_credits,99_996);let mut saved:Wallet=serde_json::from_slice(&serde_json::to_vec(&w).unwrap()).unwrap();saved.apply(&r,104).unwrap();assert_eq!(saved.milli_credits,99_996);}
 #[test]fn insufficient_and_normal_mode_never_debit(){let mut w=Wallet::default();w.trial_start=Some(100);w.demo=true;let r=json!({"action":"usage","request_id":"u","valid_result":true,"usage":{"prompt_tokens":1,"completion_tokens":1}});assert!(w.apply(&r,101).is_err());assert_eq!(w.milli_credits,0);assert!(!w.receipts.contains("u"));w.demo=false;assert!(w.apply(&r,101).is_err());assert_eq!(w.milli_credits,0);}

}
