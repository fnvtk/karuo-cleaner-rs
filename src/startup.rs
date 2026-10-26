//! 真实用户LaunchAgents列表；仅修改launchd启停覆盖，不删除配置或结束进程。
use serde::Serialize;
use serde_json::{json,Value};
use std::{collections::HashMap,fs,path::PathBuf,process::{Command,Stdio},time::{Duration,Instant}};
use std::os::unix::fs::MetadataExt;
#[derive(Clone,Serialize)]
pub struct Item{pub id:String,pub label:String,pub path:String,pub enabled:bool,pub manageable:bool,pub reason:String,pub source:String}
fn home()->PathBuf{PathBuf::from(std::env::var_os("HOME").unwrap_or_default())}
fn domain()->String{format!("gui/{}",unsafe{libc::geteuid()})}
fn command(args:&[String])->Result<String,String>{
    let mut child=Command::new("/bin/launchctl").args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|_|"无法调用系统登录项接口")?;let started=Instant::now();
    loop{match child.try_wait(){Ok(Some(_))=>break,Ok(None) if started.elapsed()<Duration::from_secs(5)=>std::thread::sleep(Duration::from_millis(20)),_=>{let _=child.kill();let _=child.wait();return Err("系统登录项接口未及时响应，未修改设置".into())}}}
    let o=child.wait_with_output().map_err(|_|"无法读取系统登录项响应")?;if !o.status.success(){return Err("系统拒绝更改此登录项，设置保持原状；可在系统设置中处理".into())}Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}
fn overrides(text:&str)->HashMap<String,bool>{text.lines().filter_map(|line|{let (label,state)=line.trim().split_once(" => ")?;let enabled=match state.trim().trim_end_matches(';'){"enabled"|"false"=>true,"disabled"|"true"=>false,_=>return None};Some((label.trim_matches('"').to_string(),enabled))}).collect()}
pub(crate) fn protected_label(label:&str)->bool{let s=label.to_lowercase();s.starts_with("com.apple.")||["karuo.cleaner","keychain","security","antivirus","malware","firewall","endpoint","authguard","payguard","timemachine","time_machine","backup","vpn","tailscale","wireguard","ssh","frpc","codex-config-guard","persistent-chat"].iter().any(|x|s.contains(x))}
fn records()->Result<Vec<Item>,String>{
    let root=home().join("Library/LaunchAgents");let overrides=overrides(&command(&["print-disabled".into(),domain()])?);let entries=match fs::read_dir(&root){Ok(e)=>e,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(vec![]),Err(_)=>return Err("无法读取用户登录项目录".into())};let mut items=vec![];
    for entry in entries.flatten(){let path=entry.path();if path.extension().is_none_or(|s|s!="plist"){continue}let id=path.to_string_lossy().into_owned();let mut item=Item{id:id.clone(),label:path.file_stem().unwrap_or_default().to_string_lossy().into_owned(),path:id,enabled:false,manageable:false,reason:String::new(),source:"用户LaunchAgent".into()};
        let parsed=(||{let m=fs::symlink_metadata(&path).map_err(|_|"配置元数据读取失败")?;if m.file_type().is_symlink()||!m.is_file()||m.uid()!=unsafe{libc::geteuid()}||m.nlink()!=1||m.len()>1024*1024{return Err("配置链接、所有权或文件类型受保护")}
            if fs::canonicalize(&path).map_err(|_|"配置真实路径不可读")?!=path{return Err("登录项路径包含链接，受保护")}
            let v=plist::Value::from_file(&path).map_err(|_|"登录项配置无法解析")?;let d=v.as_dictionary().ok_or("登录项配置不是字典")?;let label=d.get("Label").and_then(|v|v.as_string()).ok_or("登录项没有有效Label")?;if label.is_empty()||label.len()>256||label.contains('/')||label.chars().any(char::is_control){return Err("登录项Label无效")}
            item.label=label.into();item.enabled=overrides.get(label).copied().unwrap_or(!d.get("Disabled").and_then(|v|v.as_boolean()).unwrap_or(false));
            if protected_label(label){item.reason="系统、安全、备份、网络或本应用关键项默认保护；请到系统设置核对".into()}else{item.manageable=true;item.reason="可调整下次自动启动；不结束当前进程，不删除plist".into()}Ok(())})();
        if let Err(e)=parsed{item.reason=e.into()}items.push(item);
    }items.sort_by(|a,b|a.label.to_lowercase().cmp(&b.label.to_lowercase()));Ok(items)
}
pub fn list()->Value{match records(){Ok(items)=>{let total=items.len();json!({"event":"startup","items":items,"total":total,"mode":"user_launch_agents","notice":"这里列出真实用户LaunchAgents，不再把数量当作可选列表。系统管理的现代登录项请在macOS系统设置中管理；启停不结束当前进程。"})},Err(e)=>json!({"event":"startup","items":[],"total":0,"error":e})}}
pub fn set(ids:&[String],enabled:bool)->Value{
    let mut errors=vec![];let current=match records(){Ok(v)=>v,Err(e)=>return json!({"event":"startup","error":e})};let mut changed=0;
    for id in ids{let Some(item)=current.iter().find(|x|&x.id==id)else{errors.push("此项不属于当前用户登录项列表".to_owned());continue};if !item.manageable{errors.push(format!("{}：{}",item.label,item.reason));continue}
        match command(&[if enabled{"enable".into()}else{"disable".into()},format!("{}/{}",domain(),item.label)]){Ok(_)=>changed+=1,Err(e)=>errors.push(format!("{}：{e}",item.label))}
    }let mut v=list();v["changed"]=json!(changed);if !errors.is_empty(){v["error"]=json!(errors.join("；"))}v
}
#[cfg(test)]mod tests{use super::*;
    #[test]fn launchd_state_is_not_guessed_from_count(){let m=overrides("\"com.example.a\" => disabled\n\"com.example.b\" => enabled\n");assert_eq!(m.get("com.example.a"),Some(&false));assert_eq!(m.get("com.example.b"),Some(&true));}
    #[test]fn sensitive_login_items_remain_protected(){for s in ["com.apple.test","com.example.vpn","com.karuo.timemachine.guardian","com.alipay.agent.payguard","com.karuo.cleaner.schedule"]{assert!(protected_label(s))}assert!(!protected_label("com.example.image-editor"));}
}
