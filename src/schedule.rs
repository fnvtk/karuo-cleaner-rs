//! 用户级定时任务。默认关闭；后台只处理有限、可恢复的旧缓存和日志。
use crate::{actions::TrashService, engine, safety};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs::{self, OpenOptions}, io::Write, os::unix::{fs::{MetadataExt, OpenOptionsExt}, io::AsRawFd}, path::{Path, PathBuf}, process::Command, sync::atomic::AtomicBool, time::{Instant, SystemTime, UNIX_EPOCH}};

const LABEL: &str = "com.karuo.cleaner.schedule";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config { pub enabled: bool, pub frequency: String, pub hour: u8, pub minute: u8, pub mode: String }
impl Default for Config { fn default() -> Self { Self { enabled:false, frequency:"weekly".into(), hour:10, minute:0, mode:"scan".into() } } }
impl Config { fn validate(&self)->Result<(),String> { if !["daily","weekly"].contains(&self.frequency.as_str()) || self.hour>23 || self.minute>59 || !["scan","trash"].contains(&self.mode.as_str()) { Err("定时参数无效，请重新选择时间和方式".into()) } else { Ok(()) } } }
fn home()->PathBuf { PathBuf::from(std::env::var_os("HOME").unwrap_or_default()) }
fn support()->PathBuf { home().join("Library/Application Support/KaruoCleaner") }
fn config_path()->PathBuf { support().join("schedule.json") }
fn report_path()->PathBuf { support().join("schedule-last-run.json") }
fn agent_path()->PathBuf { home().join("Library/LaunchAgents").join(format!("{LABEL}.plist")) }
fn domain()->String { format!("gui/{}",unsafe{libc::geteuid()}) }
fn now()->u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }

/// 不跟随设置文件或父目录中的链接，已有文件仅接受本用户的普通单链接文件。
pub fn safe_path(path:&Path)->Result<(),String> {
    if !path.is_absolute() {return Err("设置路径不是绝对路径".into())}
    let mut part=PathBuf::new();
    for component in path.components() { part.push(component.as_os_str()); match fs::symlink_metadata(&part) {
        Ok(m) if m.file_type().is_symlink()=>return Err("设置路径包含符号链接，已停止".into()),
        Ok(m) if part==path && (!m.is_file() || m.uid()!=unsafe{libc::geteuid()} || m.nlink()!=1)=>return Err("设置文件所有权或类型无效".into()),
        Ok(m) if part!=path && !m.is_dir()=>return Err("设置父路径不是目录".into()),
        Ok(_)=>{}, Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{}, Err(e)=>return Err(e.to_string())
    }} Ok(())
}
pub fn atomic_json(path:&Path,value:&impl Serialize)->Result<(),String> { let bytes=serde_json::to_vec_pretty(value).map_err(|e|e.to_string())?; atomic_bytes(path,&bytes) }
fn atomic_bytes(path:&Path,bytes:&[u8])->Result<(),String> {
    safe_path(path)?; let parent=path.parent().ok_or("设置目录缺失")?;
    fs::create_dir_all(parent).map_err(|e|e.to_string())?; safe_path(path)?;
    let temp=parent.join(format!(".karuo-write-{}-{}",std::process::id(),now_nanos()));
    let result=(||{let mut f=OpenOptions::new().create_new(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&temp).map_err(|e|e.to_string())?;f.write_all(bytes).map_err(|e|e.to_string())?;f.sync_all().map_err(|e|e.to_string())?;fs::rename(&temp,path).map_err(|e|e.to_string())})();
    if result.is_err(){let _=fs::remove_file(&temp);} result
}
fn now_nanos()->u128 {SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()}
pub fn load()->Result<Config,String> {
    let path=config_path();safe_path(&path)?;
    match fs::read(path) { Ok(b)=>{ if b.len()>8192{return Err("定时设置过大".into())} let c:Config=serde_json::from_slice(&b).map_err(|_|"定时设置损坏，已停止执行")?;c.validate()?;Ok(c) },Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(Config::default()),Err(e)=>Err(e.to_string()) }
}
fn installed()->bool { Command::new("/bin/launchctl").args(["print",&format!("{}/{LABEL}",domain())]).output().is_ok_and(|o|o.status.success()) }
pub fn status(error:Option<String>)->Value {
    let (config,load_error)=match load(){Ok(c)=>(c,None),Err(e)=>(Config::default(),Some(e))};
    let last_run=if safe_path(&report_path()).is_ok(){fs::read(report_path()).ok().filter(|b|b.len()<128*1024).and_then(|b|serde_json::from_slice::<Value>(&b).ok())}else{None};
    let installed=installed();let error=error.or(load_error).or_else(|| if config.enabled&&!installed{Some("配置已启用，但系统任务未加载；请重新保存设置".into())}else{None});
    json!({"event":"schedule","config":config,"last_run":last_run,"installed":installed,"error":error})
}
fn plist_bytes(c:&Config,exe:&Path)->Result<Vec<u8>,String> {
    c.validate()?;let mut interval=plist::Dictionary::new();interval.insert("Hour".into(),(c.hour as u64).into());interval.insert("Minute".into(),(c.minute as u64).into());
    if c.frequency=="weekly"{interval.insert("Weekday".into(),1u64.into());}
    let mut d=plist::Dictionary::new();d.insert("Label".into(),LABEL.into());d.insert("ProgramArguments".into(),plist::Value::Array(vec![exe.to_string_lossy().into_owned().into(),"--scheduled-run".into()]));d.insert("StartCalendarInterval".into(),interval.into());d.insert("ProcessType".into(),"Background".into());d.insert("LowPriorityIO".into(),true.into());d.insert("Nice".into(),10i64.into());
    let mut bytes=vec![];plist::Value::Dictionary(d).to_writer_xml(&mut bytes).map_err(|e|e.to_string())?;Ok(bytes)
}
fn bootout(){let _=Command::new("/bin/launchctl").args(["bootout",&format!("{}/{LABEL}",domain())]).output();}
pub fn save(c:Config)->Result<(),String> {
    c.validate()?;let path=agent_path();safe_path(&path)?;safe_path(&config_path())?;
    let previous=fs::read(&path).ok();
    if let Some(b)=&previous{let v=plist::Value::from_reader_xml(b.as_slice()).map_err(|_|"已有定时任务文件损坏，未覆盖")?;if v.as_dictionary().and_then(|d|d.get("Label")).and_then(|v|v.as_string())!=Some(LABEL){return Err("已有任务不属于本应用，未覆盖".into())}}
    let previous_config=fs::read(config_path()).ok();
    if !c.enabled { bootout(); if installed(){return Err("系统未能停止定时任务，设置未保存".into())} atomic_json(&config_path(),&c)?; if path.exists(){fs::remove_file(&path).map_err(|e|e.to_string())?;}return Ok(()) }
    let exe=PathBuf::from("/Applications/毕方AI.app/Contents/MacOS/KaruoCleaner");
    if !exe.is_file(){return Err("请先将应用安装到 Applications 后启用定时功能".into())}
    let bytes=plist_bytes(&c,&exe)?;bootout();atomic_bytes(&path,&bytes)?;
    let result=(||{atomic_json(&config_path(),&c)?;let out=Command::new("/bin/launchctl").arg("bootstrap").arg(domain()).arg(&path).output().map_err(|e|e.to_string())?;if !out.status.success()||!installed(){return Err(format!("系统拒绝加载定时任务：{}",String::from_utf8_lossy(&out.stderr).trim()))}Ok(())})();
    if result.is_err(){bootout();match previous_config{Some(b)=>{let _=atomic_bytes(&config_path(),&b);},None=>{let _=fs::remove_file(config_path());}}match previous{Some(b)=>{let _=atomic_bytes(&path,&b);let _=Command::new("/bin/launchctl").arg("bootstrap").arg(domain()).arg(&path).output();},None=>{let _=fs::remove_file(path);}}} result
}

/// 自动模式额外收紧：仅pip/Go/Homebrew可重建缓存或90天旧日志；单次50项/128MB。
fn automatic_candidate(e:&crate::model::Entry,at:u64)->bool {
    if e.size>32*1024*1024 || e.is_dir{return false}
    let age=at.saturating_sub(e.modified.unwrap_or(at));
    if e.kind=="log"{return Path::new(&e.path).starts_with(home().join("Library/Logs")) && age>=90*86400}
    if e.kind!="cache" || age<14*86400{return false}
    let root=home().join("Library/Caches");let Ok(rel)=Path::new(&e.path).strip_prefix(&root)else{return false};
    rel.components().next().and_then(|c|c.as_os_str().to_str()).is_some_and(|name|["pip","go-build","Homebrew"].contains(&name))
}
fn build_active()->Result<bool,String> {
    let o=Command::new("/bin/ps").args(["-axo","comm="]).output().map_err(|e|e.to_string())?;if !o.status.success(){return Err("无法确认构建进程，保留所有候选".into())}
    Ok(String::from_utf8_lossy(&o.stdout).lines().any(|s|Path::new(s.trim()).file_name().and_then(|n|n.to_str()).is_some_and(|n|["cargo","rustc","go","pip","pip3","brew","clang","cc1"].contains(&n))))
}
pub fn run()->Result<Value,String> {
    let c=load()?;if !c.enabled{return Ok(json!({"status":"disabled","moved":0}))}
    let lock_path=support().join("scheduled-run.lock");safe_path(&lock_path)?;fs::create_dir_all(support()).map_err(|e|e.to_string())?;
    let lock=OpenOptions::new().read(true).write(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(lock_path).map_err(|e|e.to_string())?;
    let m=lock.metadata().map_err(|e|e.to_string())?;if m.uid()!=unsafe{libc::geteuid()}||m.nlink()!=1||!m.is_file(){return Err("后台任务锁无效".into())}
    if unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0{return Err("另一个定时任务正在运行，本次跳过".into())}
    let started=Instant::now();let result=engine::scan_junk(&AtomicBool::new(false),|_|{});
    let mut report=json!({"at":now(),"mode":c.mode,"status":"scanned","scanned":result.scanned,"candidates":result.entries.len(),"moved":0,"bytes":0,"errors":[],"notes":result.notes,"truncated":result.truncated});
    if c.mode=="trash" {
        let (preferences,recovered)=match crate::guardian::load().and_then(|p|crate::guardian::recovered().map(|r|(p,r))){Ok(v)=>v,Err(e)=>{report["status"]=json!("paused");report["errors"]=json!([e]);atomic_json(&report_path(),&report)?;return Ok(report)}};
        report["preferences"]=json!(preferences);
        report["recovered_avoided"]=json!(recovered.len());
        let mut errors=vec![];let mut moved=0;let mut bytes=0u64;
        match build_active(){Ok(false)=>{
            let service=TrashService::new(vec![home().join("Library/Caches"),home().join("Library/Logs")],support().join("history-rust.json"));
            for e in result.entries.iter().filter(|e|automatic_candidate(e,now()) && crate::guardian::allows(&preferences,e,now(),&recovered)).take(50){
                if load().map_or(true,|current| !current.enabled || current.mode!="trash"){errors.push("设置已关闭或无法确认，本次停止处理".into());break}
                if started.elapsed().as_secs()>60 || bytes.saturating_add(e.size)>128*1024*1024{errors.push("本次已到后台预算上限，其余项目保留".to_string());break}
                match crate::guardian::load().and_then(|p|crate::guardian::recovered().map(|r|crate::guardian::allows(&p,e,now(),&r))){Ok(true)=>{},Ok(false)=>continue,Err(e)=>{errors.push(e);break}}
                if let Err(error)=safety::validate_junk_entry(e){errors.push(error);continue}
                let r=service.move_entries(std::slice::from_ref(e));moved+=r.records.len();bytes=bytes.saturating_add(r.records.iter().map(|x|x.size).sum::<u64>());errors.extend(r.failures);
            }
        },Ok(true)=>errors.push("检测到构建或安装进程，本次保留所有候选".into()),Err(e)=>errors.push(e)}
        report["status"]=json!(if errors.is_empty(){"completed"}else{"partial"});report["moved"]=json!(moved);report["bytes"]=json!(bytes);report["errors"]=json!(errors);
    }
    report["elapsed_ms"]=json!(started.elapsed().as_millis());atomic_json(&report_path(),&report)?;Ok(report)
}

#[cfg(test)] mod tests { use super::*;
    #[test]fn schedule_default_is_disabled_and_scan_only(){let c=Config::default();assert!(!c.enabled);assert_eq!(c.mode,"scan");assert!(c.validate().is_ok());}
    #[test]fn invalid_schedule_is_rejected(){for c in [Config{hour:24,..Config::default()},Config{minute:60,..Config::default()},Config{mode:"purge".into(),..Config::default()},Config{frequency:"secondly".into(),..Config::default()}]{assert!(c.validate().is_err())}}
    #[test]fn calendar_uses_safe_executable_and_weekly_monday(){let c=Config{enabled:true,..Config::default()};let bytes=plist_bytes(&c,Path::new("/Applications/毕方AI.app/Contents/MacOS/KaruoCleaner")).unwrap();let v=plist::Value::from_reader_xml(bytes.as_slice()).unwrap();let d=v.as_dictionary().unwrap();assert_eq!(d["ProgramArguments"].as_array().unwrap()[1].as_string(),Some("--scheduled-run"));assert_eq!(d["StartCalendarInterval"].as_dictionary().unwrap()["Weekday"].as_unsigned_integer(),Some(1));assert!(d.get("RunAtLoad").is_none());}
    #[test]fn automatic_cleanup_never_targets_personal_files_or_recent_cache(){
        let at=200*86400;let mut e=crate::model::Entry{id:"fixture".into(),name:"cache".into(),path:home().join("Library/Caches/pip/fixture").to_string_lossy().into(),size:10,modified:Some(at-15*86400),kind:"cache".into(),detail:String::new(),is_dir:false,scope_root:String::new(),inode:0,device:0,mtime_nsec:0};
        assert!(automatic_candidate(&e,at));e.modified=Some(at-13*86400);assert!(!automatic_candidate(&e,at));e.modified=Some(0);
        e.path=home().join("Documents/pip/fixture").to_string_lossy().into();assert!(!automatic_candidate(&e,at));
        e.path=home().join("Library/Caches/pip-credentials/fixture").to_string_lossy().into();assert!(!automatic_candidate(&e,at));
        e.path=home().join("Library/Caches/pip/fixture").to_string_lossy().into();e.size=33*1024*1024;assert!(!automatic_candidate(&e,at));
        e.size=10;e.kind="file".into();assert!(!automatic_candidate(&e,at));e.kind="log".into();assert!(!automatic_candidate(&e,at));e.path=home().join("Library/Logs/fixture.log").to_string_lossy().into();e.modified=Some(at-89*86400);assert!(!automatic_candidate(&e,at));e.modified=Some(at-90*86400);assert!(automatic_candidate(&e,at));
    }

}
