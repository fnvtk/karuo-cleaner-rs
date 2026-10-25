//! 空间图清理计划：只接受索引快照，永不整体移走目录。
use crate::model::Entry;
use serde_json::{json,Value};
use std::{collections::{HashMap,HashSet},fs,io::Read,os::unix::fs::MetadataExt,path::PathBuf,sync::{Mutex,OnceLock}};
#[derive(Clone)]struct Plan{entries:Vec<Entry>,protected:usize}
fn plans()-> &'static Mutex<HashMap<String,Plan>>{static P:OnceLock<Mutex<HashMap<String,Plan>>>=OnceLock::new();P.get_or_init(Default::default)}
pub fn build_plan(candidates:Vec<(PathBuf,u64,u64)>,root:PathBuf)->Result<Value,String>{let ignored=crate::safety::load_ignored_paths()?;let mut entries=Vec::new();let mut protected=0;let mut seen=HashSet::new();for (p,dev,ino) in candidates{if !seen.insert(p.clone()){continue}let Ok(m)=fs::symlink_metadata(&p)else{protected+=1;continue};if p==root||!p.starts_with(&root)||m.dev()!=dev||m.ino()!=ino||fs::canonicalize(&p).ok().as_deref()!=Some(&p)||!crate::safety::junk_metadata_safe(&p,&m)||crate::safety::matches_ignored(&p,&ignored){protected+=1;continue}entries.push(Entry{id:format!("cleanup:{dev}:{ino}"),path:p.to_string_lossy().into(),name:p.file_name().unwrap_or_default().to_string_lossy().into(),kind:"file".into(),size:m.len(),modified:Some(m.mtime().max(0)as u64),mtime_nsec:m.mtime_nsec(),scope_root:root.to_string_lossy().into(),is_dir:false,inode:ino,device:dev,detail:"空间图手工选择；操作前再次核验，文件夹本身保留".into()});}let mut b=[0u8;24];fs::File::open("/dev/urandom").and_then(|mut f|f.read_exact(&mut b)).map_err(|_|"无法生成清理计划")?;let id=b.iter().map(|b|format!("{b:02x}")).collect::<String>();plans().lock().map_err(|_|"计划锁失败")?.insert(id.clone(),Plan{entries,protected});page_plan(&id,0)}
pub fn page_plan(id:&str,page:usize)->Result<Value,String>{let ps=plans().lock().map_err(|_|"计划锁失败")?;let p=ps.get(id).ok_or("清理计划不存在")?;let total=p.entries.len();let pages=total.div_ceil(50).max(1);let page=page.min(pages-1);Ok(json!({"event":"cleanup_plan","plan_id":id,"items":p.entries.iter().skip(page*50).take(50).collect::<Vec<_>>(),"total":total,"total_bytes":p.entries.iter().fold(0u64,|n,e|n.saturating_add(e.size)),"protected_count":p.protected,"page":page,"page_count":pages,"notes":["仅包含扫描时已索引并重新核验的普通文件；未读取到的文件不授权","系统、保护名单、数据库、密钥、链接与云端占位保留","不会整体删除目录；移入废纸篓前再次检查占用和文件身份"]}))}
pub fn approved_entries(id:&str,ids:&[String],all:bool)->Result<Vec<Entry>,String>{approved_entries_excluding(id,ids,all,&[])}
pub fn approved_entries_excluding(id:&str,ids:&[String],all:bool,excluded:&[String])->Result<Vec<Entry>,String>{let ps=plans().lock().map_err(|_|"计划锁失败")?;let p=ps.get(id).ok_or("清理计划不存在")?;if !all&&ids.iter().any(|id|!p.entries.iter().any(|e|&e.id==id)){return Err("选择包含计划外文件".into())}let excluded:HashSet<_>=excluded.iter().collect();Ok(p.entries.iter().filter(|e|(all||ids.contains(&e.id))&&!excluded.contains(&e.id)).cloned().collect())}
pub fn analyze_plan(id:&str)->Result<Value,String>{let entries=approved_entries(id,&[],true)?;let mut value=crate::analysis::analyze(&entries,"space");value["plan_id"]=json!(id);Ok(value)}
#[cfg(test)]mod tests{use super::*;#[test]fn unknown_plan_cannot_approve_paths(){assert!(approved_entries("unknown",&["/System".into()],true).is_err())}#[test]fn protection_rejects_system_file(){assert!(crate::safety::is_protected(std::path::Path::new("/System/file")));}}

/// 仅用于后台持有的扫描快照；不得把模型返回的路径转换为新授权。
pub fn build_entries(entries:Vec<Entry>)->Result<Value,String>{
 let ignored=crate::safety::load_ignored_paths()?;let mut accepted=Vec::new();let mut protected=0;let mut seen=HashSet::new();
 for e in entries {let p=PathBuf::from(&e.path);let root=PathBuf::from(&e.scope_root);let Ok(m)=fs::symlink_metadata(&p)else{protected+=1;continue};
 if !seen.insert(p.clone()){continue}
 if e.is_dir||!matches!(e.kind.as_str(),"file"|"large"|"old"|"cache"|"log"|"recent_cache"|"developer_cache")||p==root||!p.starts_with(&root)||fs::canonicalize(&p).ok().as_deref()!=Some(&p)||m.ino()!=e.inode||m.dev()!=e.device||m.len()!=e.size||Some(m.mtime().max(0)as u64)!=e.modified||m.mtime_nsec()!=e.mtime_nsec||!crate::safety::junk_metadata_safe(&p,&m)||crate::safety::matches_ignored(&p,&ignored){protected+=1;continue}
 if matches!(e.kind.as_str(),"cache"|"log"|"recent_cache"|"developer_cache")&&crate::safety::validate_junk_entry(&e).is_err(){protected+=1;continue}accepted.push(e);
 }
 let mut b=[0u8;24];fs::File::open("/dev/urandom").and_then(|mut f|f.read_exact(&mut b)).map_err(|_|"无法生成清理计划")?;let id=b.iter().map(|b|format!("{b:02x}")).collect::<String>();plans().lock().map_err(|_|"计划锁失败")?.insert(id.clone(),Plan{entries:accepted,protected});page_plan(&id,0)
}

#[cfg(test)]mod exclusion_tests {
 use super::*;
 #[test]fn plan_all_except_one_keeps_other_pages(){
  let entries=(0..151).map(|n|Entry{id:format!("id-{n}"),name:format!("f{n}"),path:format!("/fixture/f{n}"),size:1,modified:None,kind:"file".into(),detail:String::new(),is_dir:false,scope_root:"/fixture".into(),inode:n,device:1,mtime_nsec:0}).collect();
  let id="test-exclusion-plan";plans().lock().unwrap().insert(id.into(),Plan{entries,protected:0});
  let found=approved_entries_excluding(id,&[],true,&["id-51".into()]).unwrap();assert_eq!(found.len(),150);assert!(!found.iter().any(|e|e.id=="id-51"));assert!(found.iter().any(|e|e.id=="id-150"));plans().lock().unwrap().remove(id);
 }
}
