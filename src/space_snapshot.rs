//! 本机私有、流式空间快照。只恢复浏览索引，不恢复删除授权。
use super::{Cache,SpaceChild};
use serde::{Deserialize,Serialize};
use std::{fs::{self,File,OpenOptions},io::{self,Read,Write,BufReader,BufWriter},path::{Path,PathBuf},ffi::OsString,sync::{Arc,Mutex,OnceLock,atomic::{AtomicU64,Ordering}},time::{SystemTime,UNIX_EPOCH}};
use std::os::unix::{ffi::{OsStrExt,OsStringExt},fs::{MetadataExt,OpenOptionsExt}};
static EPOCH:AtomicU64=AtomicU64::new(0);
static WRITER:OnceLock<Mutex<()>>=OnceLock::new();
static COMMIT:OnceLock<Mutex<()>>=OnceLock::new();
static SERIAL:AtomicU64=AtomicU64::new(0);
const MAGIC:&[u8;8]=b"KRSPACE1";
const CHUNK:usize=1024;
fn now()->u64{SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()}
fn support()->PathBuf{PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/Application Support/KaruoCleaner")}
pub fn superseded(error:&str)->bool{error.contains("旧快照")||error.contains("等待新快照")}
pub fn epoch()->u64{EPOCH.load(Ordering::SeqCst)}
pub fn index_path()->PathBuf{support().join("space-index-v1.bin")}
fn state_path()->PathBuf{support().join("space-scan-state.json")}
fn view_path()->PathBuf{support().join("space-view.json")}
#[derive(Serialize,Deserialize)]struct Header{version:u32,scan_id:u64,volume_id:Option<String>,root:Vec<u8>,current:Vec<u8>,scanned:u64,skipped:u64,done:bool,#[serde(default)]cancelled:bool,truncated:bool,error:Option<String>,page:usize,query:String,show_hidden:bool,saved_at:u64,dirs:u64,tombstones:u64}
#[derive(Serialize,Deserialize)]struct Marker{root:Vec<u8>,epoch:u64,started_at:u64,finished:bool}
#[derive(Serialize,Deserialize)]struct View{scan_id:u64,root:Vec<u8>,current:Vec<u8>,page:usize,query:String,show_hidden:bool}
fn private_open(path:&Path)->Result<File,String>{crate::schedule::safe_path(path)?;let f=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path).map_err(|e|e.to_string())?;let m=f.metadata().map_err(|e|e.to_string())?;if !m.is_file()||m.uid()!=unsafe{libc::geteuid()}||m.nlink()!=1||m.mode()&0o077!=0{return Err("空间快照权限或身份无效".into())}Ok(f)}
pub(crate) fn read_json<T:serde::de::DeserializeOwned>(path:&Path)->Result<Option<T>,String>{match fs::symlink_metadata(path){Err(e) if e.kind()==io::ErrorKind::NotFound=>return Ok(None),Err(e)=>return Err(e.to_string()),Ok(_)=>{}}let f=private_open(path)?;if f.metadata().map_err(|e|e.to_string())?.len()>4*1024*1024{return Err("空间快照状态格式过大".into())}serde_json::from_reader(f).map(Some).map_err(|e|e.to_string())}
pub fn begin(root:&Path)->Result<u64,String>{let _guard=COMMIT.get_or_init(||Mutex::new(())).lock().map_err(|_|"快照提交锁不可用")?;let epoch=EPOCH.fetch_add(1,Ordering::SeqCst)+1;crate::schedule::atomic_json(&state_path(),&Marker{root:root.as_os_str().as_bytes().to_vec(),epoch,started_at:now(),finished:false})?;Ok(epoch)}
/// 视图很小，独立于百万条目索引保存，避免每次导航重写索引。
pub fn save_view(c:&Cache)->Result<(),String>{if c.root.as_os_str().is_empty(){return Ok(())}crate::schedule::atomic_json(&view_path(),&View{scan_id:c.scan_id,root:c.root.as_os_str().as_bytes().to_vec(),current:c.current.as_os_str().as_bytes().to_vec(),page:c.page,query:c.query.clone(),show_hidden:c.show_hidden})}
// FNV用于检测截断/意外损坏，不是授权或签名；权限和操作前身份复核单独执行。
fn hash(h:&mut u64,b:&[u8]){for x in b{*h=(*h^(*x as u64)).wrapping_mul(1099511628211)}}
struct Output<W>{inner:W,digest:u64}
impl<W:Write> Write for Output<W>{fn write(&mut self,b:&[u8])->io::Result<usize>{let n=self.inner.write(b)?;hash(&mut self.digest,&b[..n]);Ok(n)}fn flush(&mut self)->io::Result<()>{self.inner.flush()}}
struct Input<R>{inner:R,digest:u64}
impl<R:Read> Read for Input<R>{fn read(&mut self,b:&mut[u8])->io::Result<usize>{let n=self.inner.read(b)?;hash(&mut self.digest,&b[..n]);Ok(n)}}
fn number(w:&mut impl Write,n:u64)->io::Result<()>{w.write_all(&n.to_le_bytes())}
fn get_number(r:&mut impl Read)->io::Result<u64>{let mut b=[0u8;8];r.read_exact(&mut b)?;Ok(u64::from_le_bytes(b))}
fn bytes(w:&mut impl Write,b:&[u8])->io::Result<()>{number(w,b.len()as u64)?;w.write_all(b)}
fn get_bytes(r:&mut impl Read,max:usize)->Result<Vec<u8>,String>{let n=get_number(r).map_err(|e|e.to_string())?;if n>max as u64{return Err("空间快照记录长度无效".into())}let mut b=vec![0;n as usize];r.read_exact(&mut b).map_err(|e|e.to_string())?;Ok(b)}
fn path(b:Vec<u8>)->Result<PathBuf,String>{if b.contains(&0)||b.split(|x|*x==b'/').any(|s|s==b"."||s==b".."){return Err("空间快照路径包含非法组件".into())}let p=PathBuf::from(OsString::from_vec(b));if !p.is_absolute()||p.components().any(|c|matches!(c,std::path::Component::ParentDir|std::path::Component::CurDir)){return Err("空间快照路径不是绝对规范路径".into())}Ok(p)}
fn name(b:Vec<u8>)->Result<OsString,String>{if b.is_empty()||b.contains(&0)||b.contains(&b'/')||b==b"."||b==b".."{return Err("空间快照文件名越界".into())}Ok(OsString::from_vec(b))}
struct Temp(PathBuf);impl Drop for Temp{fn drop(&mut self){let _=fs::remove_file(&self.0);}}
fn write_index(cache:&Arc<Mutex<Cache>>,target:&Path,epoch:u64)->Result<(u64,u64),String>{
 let _writer=WRITER.get_or_init(||Mutex::new(())).lock().map_err(|_|"快照写入锁不可用")?;
 let (header,keys,revision,tombstones)={let c=cache.lock().map_err(|_|"空间索引不可用")?;if !c.done||c.root.as_os_str().is_empty(){return Err("空间扫描尚未结束".into())}let keys:Vec<_>=c.nodes.keys().cloned().collect();(Header{version:1,scan_id:c.scan_id,volume_id:c.volume_id.clone(),root:c.root.as_os_str().as_bytes().to_vec(),current:c.current.as_os_str().as_bytes().to_vec(),scanned:c.scanned,skipped:c.skipped,done:c.done,cancelled:c.cancelled,truncated:c.truncated,error:c.error.clone(),page:c.page,query:c.query.clone(),show_hidden:c.show_hidden,saved_at:now(),dirs:keys.len()as u64,tombstones:c.tombstones.len()as u64},keys,c.revision,c.tombstones.iter().cloned().collect::<Vec<_>>())};
 let check=|c:&Cache|->Result<(),String>{if EPOCH.load(Ordering::SeqCst)!=epoch||c.revision!=revision{return Err("索引已更新，此次旧快照不提交".into())}Ok(())};
 crate::schedule::safe_path(target)?;let parent=target.parent().ok_or("快照目录缺失")?;fs::create_dir_all(parent).map_err(|e|e.to_string())?;crate::schedule::safe_path(target)?;
 let temp=Temp(parent.join(format!(".space-write-{}-{}",std::process::id(),SERIAL.fetch_add(1,Ordering::Relaxed))));
 let f=OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&temp.0).map_err(|e|e.to_string())?;
 let mut out=Output{inner:BufWriter::with_capacity(256*1024,f),digest:14695981039346656037};
 out.write_all(MAGIC).map_err(|e|e.to_string())?;bytes(&mut out,&serde_json::to_vec(&header).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
 for dir in keys{
  let (size,direct,incomplete,count)={let c=cache.lock().map_err(|_|"空间索引不可用")?;check(&c)?;let n=c.nodes.get(&dir).ok_or("目录已变化")?;(n.0,c.direct_sizes.get(&dir).copied().unwrap_or(0),c.incomplete_dirs.contains(&dir),c.children.get(&dir).map_or(0,Vec::len))};
  bytes(&mut out,dir.as_os_str().as_bytes()).map_err(|e|e.to_string())?;for n in [size,direct,incomplete as u64,count as u64]{number(&mut out,n).map_err(|e|e.to_string())?;}
  for offset in (0..count).step_by(CHUNK){
   let part={let c=cache.lock().map_err(|_|"空间索引不可用")?;check(&c)?;c.children[&dir][offset..(offset+CHUNK).min(count)].to_vec()};
   for child in part{bytes(&mut out,child.name.as_bytes()).map_err(|e|e.to_string())?;for n in [child.size,child.is_dir as u64,child.device,child.inode]{number(&mut out,n).map_err(|e|e.to_string())?;}}
  }
 }
 for p in tombstones{bytes(&mut out,p.as_os_str().as_bytes()).map_err(|e|e.to_string())?;}
 let digest=out.digest;out.inner.write_all(&digest.to_le_bytes()).map_err(|e|e.to_string())?;out.inner.flush().map_err(|e|e.to_string())?;out.inner.get_ref().sync_all().map_err(|e|e.to_string())?;
 let _commit=COMMIT.get_or_init(||Mutex::new(())).lock().map_err(|_|"快照提交锁不可用")?;let c=cache.lock().map_err(|_|"空间索引不可用")?;check(&c)?;crate::schedule::safe_path(target)?;fs::rename(&temp.0,target).map_err(|e|e.to_string())?;Ok((header.saved_at,revision))
}
fn load_index(target:&Path)->Result<Cache,String>{
 let f=private_open(target)?;let mut input=Input{inner:BufReader::with_capacity(256*1024,f),digest:14695981039346656037};let mut magic=[0;8];input.read_exact(&mut magic).map_err(|e|e.to_string())?;if &magic!=MAGIC{return Err("空间快照版本无效".into())}
 let h:Header=serde_json::from_slice(&get_bytes(&mut input,4*1024*1024)?).map_err(|e|e.to_string())?;if h.version!=1||!h.done{return Err("空间快照未完整提交".into())}let root=path(h.root)?;let current=path(h.current)?;if !current.starts_with(&root){return Err("快照浏览位置越界".into())}
 let mut c=Cache{scan_id:h.scan_id,volume_id:h.volume_id,root:root.clone(),current,scanned:h.scanned,skipped:h.skipped,done:h.done,cancelled:h.cancelled,truncated:h.truncated,error:h.error,page:h.page,query:h.query,show_hidden:h.show_hidden,restored:true,saved_at:h.saved_at,..Cache::default()};
 for _ in 0..h.dirs{
  let dir=path(get_bytes(&mut input,1024*1024)?)?;if !dir.starts_with(&root)||c.nodes.contains_key(&dir){return Err("快照目录越界或重复".into())}
  let size=get_number(&mut input).map_err(|e|e.to_string())?;let direct=get_number(&mut input).map_err(|e|e.to_string())?;let incomplete=get_number(&mut input).map_err(|e|e.to_string())?;if incomplete>1{return Err("快照目录状态无效".into())}let count=get_number(&mut input).map_err(|e|e.to_string())?;
  let mut children=Vec::new();let mut names=std::collections::HashSet::new();
  for _ in 0..count{
   let name=name(get_bytes(&mut input,1024)?)?;if !names.insert(name.clone()){return Err("快照文件名重复".into())}
   let size=get_number(&mut input).map_err(|e|e.to_string())?;let is_dir=get_number(&mut input).map_err(|e|e.to_string())?;if is_dir>1{return Err("快照文件类型无效".into())}let device=get_number(&mut input).map_err(|e|e.to_string())?;let inode=get_number(&mut input).map_err(|e|e.to_string())?;
   children.push(SpaceChild{name,size,is_dir:is_dir==1,device,inode});
  }
  c.nodes.insert(dir.clone(),(size,true));c.direct_sizes.insert(dir.clone(),direct);if incomplete==1{c.incomplete_dirs.insert(dir.clone());}c.children.insert(dir,children);
 }
 for _ in 0..h.tombstones{let p=path(get_bytes(&mut input,1024*1024)?)?;if p==root||!p.starts_with(&root)||!c.tombstones.insert(p){return Err("快照移除记录越界或重复".into())}}
 let expected=input.digest;let mut b=[0;8];input.inner.read_exact(&mut b).map_err(|e|e.to_string())?;if u64::from_le_bytes(b)!=expected{return Err("空间快照校验失败，原文件已保留".into())}let mut trailing=[0;1];if input.inner.read(&mut trailing).map_err(|e|e.to_string())?!=0{return Err("空间快照存在多余记录".into())}
 if !c.nodes.contains_key(&root)||!c.nodes.contains_key(&c.current){return Err("快照根目录或浏览位置缺失".into())}
 let mut linked=std::collections::HashSet::new();
 for (dir,children) in &c.children{for child in children{if child.is_dir{let joined=dir.join(&child.name);let Some((key,_))=c.nodes.get_key_value(&joined)else{return Err("快照子目录缺失".into())};linked.insert(key.as_path());}}}
 if c.nodes.keys().any(|dir|dir!=&root&&!linked.contains(dir.as_path())){return Err("快照目录父子关系无效".into())}
 Ok(c)
}
pub fn save(cache:&Arc<Mutex<Cache>>,epoch:u64)->Result<u64,String>{let (saved,revision)=write_index(cache,&index_path(),epoch)?;let _commit=COMMIT.get_or_init(||Mutex::new(())).lock().map_err(|_|"快照提交锁不可用")?;if EPOCH.load(Ordering::SeqCst)!=epoch{return Err("新扫描已开始，旧快照不标记完成".into())}let mut c=cache.lock().map_err(|_|"空间索引不可用")?;if c.revision!=revision{return Err("空间内容已经变化，等待新快照".into())}crate::schedule::atomic_json(&state_path(),&Marker{root:c.root.as_os_str().as_bytes().to_vec(),epoch,started_at:saved,finished:true})?;c.saved_at=saved;c.snapshot_notice=None;Ok(saved)}
pub fn request_save(cache:Arc<Mutex<Cache>>){let epoch=EPOCH.load(Ordering::SeqCst);std::thread::spawn(move||{let eligible=cache.lock().is_ok_and(|c|c.done&&!c.root.as_os_str().is_empty());if eligible{let result=save(&cache,epoch);if let Ok(mut c)=cache.lock(){match result{Ok(_)=>{},Err(e) if superseded(&e)=>{},Err(e)=>c.snapshot_notice=Some(format!("空间结果未保存：{e}"))}}}});}
pub fn load()->Result<Option<Cache>,String>{
 let marker:Option<Marker>=read_json(&state_path())?;
 let mut c=match fs::symlink_metadata(index_path()){Err(e) if e.kind()==io::ErrorKind::NotFound=>{if let Some(m)=marker.filter(|m|!m.finished){let root=path(m.root)?;let mut c=Cache{root:root.clone(),current:root.clone(),done:true,truncated:true,restored:true,snapshot_notice:Some("上次扫描中断，没有可恢复的已保存结果，请重新扫描。".into()),..Cache::default()};c.nodes.insert(root.clone(),(0,true));c.children.insert(root,Vec::new());return Ok(Some(c))}return Ok(None)},Err(e)=>return Err(e.to_string()),Ok(_)=>load_index(&index_path())?};
 if marker.is_some_and(|m|!m.finished){c.snapshot_notice=Some("上次新扫描未完成，当前显示此前已保存的结果，请重新扫描。".into())}
 if let Some(v)=read_json::<View>(&view_path())?{if v.root==c.root.as_os_str().as_bytes()&&v.scan_id==c.scan_id{let current=path(v.current)?;if c.nodes.contains_key(&current){c.current=current;c.page=v.page;c.query=v.query;c.show_hidden=v.show_hidden;}}}
 Ok(Some(c))
}

#[cfg(test)]mod tests{
 use super::*;
 static TEST_ID:AtomicU64=AtomicU64::new(0);
 fn fixture()->PathBuf{let p=fs::canonicalize(std::env::temp_dir()).unwrap().join(format!("karuo-space-snapshot-{}-{}",std::process::id(),TEST_ID.fetch_add(1,Ordering::SeqCst)));fs::create_dir_all(&p).unwrap();p}
 fn cache(root:&Path,count:usize)->Arc<Mutex<Cache>>{
  let mut c=Cache{root:root.into(),current:root.into(),done:true,scanned:count as u64,..Cache::default()};
  c.nodes.insert(root.into(),(count as u64*10,true));c.direct_sizes.insert(root.into(),count as u64*10);
  c.children.insert(root.into(),(0..count).map(|n|SpaceChild{name:format!("file-{n}").into(),size:10,is_dir:false,device:1,inode:n as u64+1}).collect());Arc::new(Mutex::new(c))
 }
 #[test]fn large_snapshot_roundtrip_preserves_all_pages_and_non_utf8_names(){
  let root=fixture();let cache=cache(&root,5101);{let mut c=cache.lock().unwrap();c.children.get_mut(&root).unwrap()[0].name=OsString::from_vec(vec![b'f',0xff]);c.page=102;c.show_hidden=true;c.truncated=true;c.skipped=3;}
  let target=root.join("index.bin");write_index(&cache,&target,epoch()).unwrap();assert_eq!(fs::metadata(&target).unwrap().mode()&0o777,0o600);
  let mut restored=load_index(&target).unwrap();assert_eq!(restored.children[&root].len(),5101);assert_eq!(restored.children[&root][0].name.as_bytes(),&[b'f',0xff]);assert!(restored.restored&&restored.truncated);assert_eq!(restored.page,102);assert!(restored.show_hidden);let page=restored.view();assert_eq!(page["page_count"],103);assert_eq!(page["entries"].as_array().unwrap().len(),1);fs::remove_dir_all(root).unwrap();
 }
 #[test]fn corrupted_or_truncated_snapshot_never_becomes_a_partial_authorized_index(){
  let root=fixture();let cache=cache(&root,3);let target=root.join("index.bin");write_index(&cache,&target,epoch()).unwrap();let original=fs::read(&target).unwrap();let mut bytes=original.clone();let n=bytes.len();bytes[n-1]^=1;fs::write(&target,&bytes).unwrap();assert!(load_index(&target).is_err());assert_eq!(fs::read(&target).unwrap(),bytes);fs::write(&target,&original[..original.len()/2]).unwrap();assert!(load_index(&target).is_err());fs::remove_dir_all(root).unwrap();
 }
 #[test]fn unsafe_names_and_symlink_snapshot_are_rejected(){
  let root=fixture();let cache=cache(&root,1);cache.lock().unwrap().children.get_mut(&root).unwrap()[0].name="../outside".into();let target=root.join("index.bin");write_index(&cache,&target,epoch()).unwrap();assert!(load_index(&target).is_err());std::os::unix::fs::symlink(&target,root.join("link.bin")).unwrap();assert!(load_index(&root.join("link.bin")).is_err());fs::remove_dir_all(root).unwrap();
 }
 #[test]fn stale_writer_does_not_replace_previous_good_snapshot(){
  let root=fixture();let cache=cache(&root,1);let target=root.join("index.bin");write_index(&cache,&target,epoch()).unwrap();let original=fs::read(&target).unwrap();assert!(write_index(&cache,&target,epoch()+1).is_err());assert_eq!(fs::read(&target).unwrap(),original);fs::remove_dir_all(root).unwrap();
 }
 #[test]fn removed_file_and_restore_tombstone_survive_restart(){
  let root=fixture();let cache=cache(&root,2);cache.lock().unwrap().remove_paths(&[root.join("file-0").to_string_lossy().into()]);let target=root.join("index.bin");write_index(&cache,&target,epoch()).unwrap();let mut c=load_index(&target).unwrap();assert_eq!(c.children[&root].len(),1);assert!(c.tombstones.contains(&root.join("file-0")));c.restore_files(&[(root.join("file-0"),10,1,1)]);assert_eq!(c.children[&root].len(),2);assert_eq!(c.nodes[&root].0,20);fs::remove_dir_all(root).unwrap();
 }
 #[test]fn stop_without_waiting_for_read_worker_preserves_partial_state(){let root=fixture();let cache=cache(&root,5);{let mut c=cache.lock().unwrap();c.done=false;c.stop_partial();}let target=root.join("index.bin");write_index(&cache,&target,epoch()).unwrap();let c=load_index(&target).unwrap();assert!(c.done&&c.cancelled&&c.truncated&&c.restored);assert_eq!(c.children[&root].len(),5);fs::remove_dir_all(root).unwrap();}
 #[test]fn wide_directory_hierarchy_roundtrips_without_sibling_search(){let root=fixture();let cache=cache(&root,0);{let mut c=cache.lock().unwrap();for n in 0..2000{let name=OsString::from(format!("dir-{n}"));let p=root.join(&name);c.children.get_mut(&root).unwrap().push(SpaceChild{name,size:0,is_dir:true,device:1,inode:n+1});c.nodes.insert(p.clone(),(0,true));c.direct_sizes.insert(p.clone(),0);c.children.insert(p,Vec::new());}}let target=root.join("index.bin");write_index(&cache,&target,epoch()).unwrap();let c=load_index(&target).unwrap();assert_eq!(c.nodes.len(),2001);assert_eq!(c.children[&root].len(),2000);fs::remove_dir_all(root).unwrap();}
 #[test]fn reloaded_index_does_not_authorize_replaced_file(){
  let root=PathBuf::from(std::env::var_os("HOME").unwrap()).join(format!(".karuo-space-identity-{}-{}",std::process::id(),TEST_ID.fetch_add(1,Ordering::SeqCst)));fs::create_dir_all(&root).unwrap();let file=root.join("ordinary.bin");fs::write(&file,b"before").unwrap();let cache=Arc::new(Mutex::new(Cache::default()));super::super::scan(root.clone(),Arc::new(std::sync::atomic::AtomicBool::new(false)),cache.clone(), |_|{});let target=root.join("index.bin");write_index(&cache,&target,epoch()).unwrap();let c=load_index(&target).unwrap();let candidates=c.cleanup_candidates(&[file.to_string_lossy().into()],None).unwrap();assert_eq!(candidates.len(),1);let good=crate::cleanup::build_plan(candidates.clone(),root.clone()).unwrap();assert_eq!(good["total"],1);fs::rename(&file,root.join("old.bin")).unwrap();fs::write(&file,b"new-file").unwrap();let rejected=crate::cleanup::build_plan(candidates,root.clone()).unwrap();assert_eq!(rejected["total"],0);assert_eq!(fs::read(&file).unwrap(),b"new-file");fs::remove_dir_all(root).unwrap();
 }
}
