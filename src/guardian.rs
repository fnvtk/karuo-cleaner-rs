//! 本机清理管家：偏好只能收紧既有自动清理范围，不授予新权限。
use crate::{schedule,model::TrashRecord};
use serde::{Deserialize,Serialize};
use serde_json::{json,Value};
use std::{fs,path::PathBuf};
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences { pub cache_days:u16,pub log_days:u16,pub clean_cache:bool,pub clean_logs:bool }
impl Default for Preferences { fn default()->Self{Self{cache_days:30,log_days:90,clean_cache:true,clean_logs:true}} }
impl Preferences { pub fn validate(&self)->Result<(),String>{if !(14..=3650).contains(&self.cache_days)||!(90..=3650).contains(&self.log_days){Err("临时缓存至少保留14天，运行记录至少保留90天，最长3650天".into())}else{Ok(())}} }
fn support()->PathBuf{PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Application Support/KaruoCleaner")}
fn read<T:serde::de::DeserializeOwned+Default>(path:PathBuf)->Result<T,String>{schedule::safe_path(&path)?;match fs::read(path){Ok(b)=>serde_json::from_slice(&b).map_err(|_|"管家记录无法读取，自动处理暂停".into()),Err(e)if e.kind()==std::io::ErrorKind::NotFound=>Ok(T::default()),Err(e)=>Err(e.to_string())}}
pub fn load()->Result<Preferences,String>{let p:Preferences=read(support().join("guardian.json"))?;p.validate()?;Ok(p)}
pub fn save(p:Preferences)->Result<(),String>{p.validate()?;schedule::atomic_json(&support().join("guardian.json"),&p)}
#[derive(Default,Deserialize)]struct Journal{records:Vec<TrashRecord>}
pub fn recovered()->Result<Vec<PathBuf>,String>{let j:Journal=read(support().join("history-rust.json"))?;Ok(j.records.into_iter().filter(|r|r.restored).map(|r|PathBuf::from(r.original_path)).collect())}
pub fn allows(p:&Preferences,e:&crate::model::Entry,at:u64,restored:&[PathBuf])->bool{
 if restored.iter().any(|r|std::path::Path::new(&e.path).starts_with(r)){return false}
 let days=match e.kind.as_str(){"cache" if p.clean_cache=>p.cache_days,"log" if p.clean_logs=>p.log_days,_=>return false};
 e.modified.is_some_and(|m|at.saturating_sub(m)>=u64::from(days)*86400)
}
pub fn anonymous_context()->Value{match load(){Ok(p)=>json!({"name":"清空AI管家","preferences":p,"recovered_avoidance":"恢复过的项目不再自动清理","authority":"只在已启用的定时授权内工作；习惯只收紧规则；网站尚未绑定"}),Err(e)=>json!({"error":e,"automatic_cleanup":"paused"})}}
pub fn status(error:Option<String>)->Value{let config=load();let restored=recovered();let err=error.or_else(||config.as_ref().err().cloned()).or_else(||restored.as_ref().err().cloned());json!({"event":"guardian","name":"清空AI管家","preferences":config.ok(),"recovered_count":restored.as_ref().map(|v|v.len()).unwrap_or(0),"schedule":schedule::status(None),"binding":{"status":"pending","reason":"BINDING_CREDENTIAL_REQUIRED","verified":false},"error":err})}
#[cfg(test)]mod tests{use super::*;
 fn item()->crate::model::Entry{crate::model::Entry{id:"test".into(),name:"test".into(),path:"/fixture/cache/item".into(),size:1,modified:Some(0),kind:"cache".into(),detail:String::new(),is_dir:false,scope_root:"/fixture".into(),inode:0,device:0,mtime_nsec:0}}
 #[test]fn preferences_cannot_weaken_retention(){let mut p=Preferences::default();p.cache_days=13;assert!(p.validate().is_err());p.cache_days=14;p.log_days=89;assert!(p.validate().is_err());}
 #[test]fn restored_descendants_are_avoided_but_siblings_are_not(){let p=Preferences::default();let e=item();assert!(!allows(&p,&e,100*86400,&["/fixture/cache".into()]));assert!(allows(&p,&e,100*86400,&["/fixture/cached".into()]));}
 #[test]fn unknown_age_and_disabled_categories_are_preserved(){let mut p=Preferences::default();let mut e=item();assert!(!allows(&p,&e,29*86400,&[]));p.clean_cache=false;assert!(!allows(&p,&e,100*86400,&[]));p.clean_cache=true;e.modified=None;assert!(!allows(&p,&e,100*86400,&[]));}
}
