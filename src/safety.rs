//! 共享清理保护规则：扫描只做廉价元数据筛选，真正移动前才检查占用。
use crate::model::Entry;
use std::{fs, path::{Path, PathBuf}, time::{Duration, SystemTime, UNIX_EPOCH}};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::macos::fs::MetadataExt as MacMetadataExt;

fn home() -> Option<PathBuf> { std::env::var_os("HOME").map(PathBuf::from) }
fn data_alias(path: &Path) -> PathBuf {
    path.strip_prefix("/System/Volumes/Data").map(|p| Path::new("/").join(p)).unwrap_or_else(|_| path.to_owned())
}
/// 用户保护名单读取失败必须向上传播，不能降级成“没有保护项”。
pub fn load_ignored_paths() -> Result<Vec<PathBuf>, String> {
    let path=home().ok_or("无法确定用户目录，无法读取保护名单")?.join("Library/Application Support/KaruoCleaner/ignored-paths.json");
    let mut cursor=PathBuf::new();
    for part in path.components() {
        cursor.push(part.as_os_str());
        match fs::symlink_metadata(&cursor) {
            Ok(m) if m.file_type().is_symlink() => return Err("保护名单路径包含符号链接".into()),
            Ok(_) => {},
            Err(e) if e.kind()==std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("无法读取保护名单路径：{e}")),
        }
    }
    let file=fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path).map_err(|e|format!("无法打开保护名单：{e}"))?;
    let metadata=file.metadata().map_err(|e|e.to_string())?;
    if !metadata.is_file() || metadata.nlink()!=1 || metadata.uid()!=unsafe{libc::geteuid()} || metadata.len()>1_048_576 { return Err("保护名单身份或体积无效".into()); }
    let values:Vec<String>=serde_json::from_reader(file).map_err(|e|format!("保护名单格式无效：{e}"))?;
    values.into_iter().map(|text| {
        if text.split('/').any(|part| part=="." || part=="..") { return Err("保护名单必须为绝对规范路径".into()); }
        let p=PathBuf::from(text);
        if !p.is_absolute() || p.components().any(|c|matches!(c,std::path::Component::ParentDir|std::path::Component::CurDir)) { return Err("保护名单必须为绝对规范路径".into()); }
        // 允许已移除目录继续留在名单；仍存在的路径必须与 canonical 一致。
        match fs::canonicalize(&p) {
            Ok(canonical) if canonical!=p => Err("保护名单包含非规范路径或链接".into()),
            Err(e) if e.kind()!=std::io::ErrorKind::NotFound => Err(format!("无法核验保护名单路径：{e}")),
            _ => Ok(p),
        }
    }).collect()
}

pub fn matches_ignored(path:&Path, ignored:&[PathBuf])->bool {
    let path=data_alias(path);
    ignored.iter().any(|root| path.starts_with(data_alias(root)))
}

/// 必须在 canonical 路径上调用；系统后代和 Data 卷别名使用同一规则。
pub fn is_protected(path: &Path) -> bool {
    let p = data_alias(path);
    if p == Path::new("/") || ["/System", "/Library", "/bin", "/sbin", "/usr", "/private", "/dev", "/Volumes"].iter().any(|r| p.starts_with(r)) { return true; }
    if ["/Applications", "/Users"].iter().any(|r| p == Path::new(r)) { return true; }
    if let Some(h) = home() {
        let h = data_alias(&h);
        if p == h { return true; }
        for root in ["Documents", "Downloads", "Desktop", "Pictures", "Movies", "Music"] {
            if p == h.join(root) { return true; }
        }
        let library = h.join("Library");
        if p.starts_with(&library) && !p.starts_with(library.join("Caches")) && !p.starts_with(library.join("Logs")) { return true; }
    }
    let npx=home().is_some_and(|h|p.starts_with(data_alias(&h.join(".npm/_npx"))));
    p.components().any(|c| matches!(c.as_os_str().to_str(), Some(".Trash" | ".Trashes" | ".ssh" | ".gnupg" | ".git" | "CloudStorage" | "Mobile Documents")) || c.as_os_str()=="node_modules"&&!npx || c.as_os_str().to_string_lossy().ends_with(".photoslibrary"))
}

/// 廉价、统一的列表保护解释；不打开文件或触发云端下载。
/// 这不是删除授权：执行时仍必须复查元数据、身份、占用和名单。
pub fn path_protection_reason(path:&Path)->Option<&'static str>{
    let name=path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
    if [".db",".sqlite",".sqlite3",".db-wal",".db-shm","-wal","-shm"].iter().any(|s|name.ends_with(s)) {
        return Some("数据库或运行数据，请通过所属应用管理");
    }
    if [".keychain",".keychain-db",".pem",".key",".p12",".pfx"].iter().any(|s|name.ends_with(s))||name.contains("credential")||name.contains("password") {
        return Some("密钥、凭证或密码资料，已保护");
    }
    if name.ends_with(".icloud"){return Some("云端占位文件，已保护")}
    if is_protected(path){return Some("系统、应用资料、云盘或项目目录，已保护")}
    None
}
pub fn selection_protection_reason(path:&Path,ignored:&[PathBuf])->Option<&'static str>{
    if matches_ignored(path,ignored){Some("已加入保护名单，此路径及内部项目保留")}
    else{path_protection_reason(path)}
}
pub fn annotate_protection(value:&mut serde_json::Value,ignored:&Result<Vec<PathBuf>,String>){
    let path=Path::new(value["path"].as_str().unwrap_or(""));
    let reason=match ignored{Ok(paths)=>selection_protection_reason(path,paths),Err(_)=>Some("保护名单暂时无法读取，暂不可选择")};
    value["protected"]=serde_json::json!(reason.is_some());
    value["protection_reason"]=serde_json::json!(reason);
}

/// 不读取文件内容，不触发云端下载。数据库/链接/云端占位不作为自动垃圾候选。
pub fn junk_metadata_safe(path: &Path, metadata: &fs::Metadata) -> bool {
    metadata.is_file() && !metadata.file_type().is_symlink() && metadata.nlink() == 1
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.st_flags() & (0x4000_0000 | 0x0000_0002 | 0x0000_0004 | 0x0002_0000 | 0x0004_0000) == 0
        && path_protection_reason(path).is_none()
}

/// 定时清理与手动缓存清理共用：真实 HOME 白名单、年龄和扫描身份再次核验。
/// 此函数只允许 cache/log；不会把大型文件或应用伪装成自动垃圾。
pub fn validate_junk_entry(entry: &Entry) -> Result<(), String> {
    let h = home().ok_or("无法确定用户目录")?;
    let (root, days) = match entry.kind.as_str() { "cache" => (h.join("Library/Caches"), 7), "log" => (h.join("Library/Logs"), 30), "recent_cache" => (h.join("Library/Caches"),1), "developer_cache" => {
        let allowed=[h.join(".npm/_npx"),h.join(".npm/_cacache"),h.join(".cargo/registry/cache")];
        let root=allowed.into_iter().find(|root|Path::new(&entry.path).starts_with(root)&&Path::new(&entry.path)!=root).ok_or("不在开发缓存白名单内")?;let days=if root==h.join(".npm/_npx"){if !npx_package_old(Path::new(&entry.path)){return Err("npx临时包近期使用或来源不明，保留".into())}30}else{7};(root,days)
    }, _ => return Err("自动清理仅允许旧缓存与旧日志".into()) };
    let p = Path::new(&entry.path);
    let root = fs::canonicalize(root).map_err(|e| e.to_string())?;
    let canonical = fs::canonicalize(p).map_err(|e| e.to_string())?;
    if canonical != p || p == root || !p.starts_with(&root) || crate::engine::protected_junk_path(p, &root) { return Err("不在真实用户缓存/日志白名单内".into()); }
    let m = fs::symlink_metadata(p).map_err(|e| e.to_string())?;
    if !junk_metadata_safe(p, &m) { return Err("受保护、云端、硬链接或非普通垃圾文件".into()); }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    if m.mtime() < 0 || (m.mtime() as u64) > now.saturating_sub(days * 86400) { return Err("尚未达到旧缓存/日志保留期限".into()); }
    if m.ino() != entry.inode || m.dev() != entry.device || m.len() != entry.size || Some(m.mtime() as u64) != entry.modified || m.mtime_nsec() != entry.mtime_nsec { return Err("扫描后文件身份、大小或修改时间发生变化".into()); }
    Ok(())
}

#[link(name="proc")]
unsafe extern "C" {
    fn proc_pidinfo(pid:libc::c_int,flavor:libc::c_int,arg:u64,buffer:*mut libc::c_void,size:libc::c_int)->libc::c_int;
    fn proc_listpidspath(kind:u32, typeinfo:u32, path:*const libc::c_char, flags:u32, buffer:*mut libc::c_void, size:libc::c_int)->libc::c_int;
}

/// macOS libproc 按文件引用查占用，不枚举/解析所有挂载点，避免失联网络卷拖慢 lsof。
/// 使用全部进程且 flags=0（不排除 O_EVTONLY），API错误、缓冲不足和3秒超时失败闭合。
pub fn ensure_not_open(path: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    // proc_listpidspath can skip an entire process if another descriptor closes
    // during its scan. Check our descriptors by inode first, ignoring only EBADF.
    let target=fs::metadata(path).map_err(|e|format!("无法核验文件占用：{e}"))?;
    let needed=unsafe{proc_pidinfo(std::process::id() as i32,1,0,std::ptr::null_mut(),0)};
    if needed<=0{return Err("无法读取本进程文件句柄，已跳过".into())}
    let mut descriptors=vec![0u64;(needed as usize/8)+64];
    let bytes=unsafe{proc_pidinfo(std::process::id() as i32,1,0,descriptors.as_mut_ptr().cast(),(descriptors.len()*8) as i32)};
    if bytes<=0||bytes as usize>=descriptors.len()*8||bytes%8!=0{return Err("本进程文件句柄未完整读取，已跳过".into())}
    for item in descriptors.iter().take(bytes as usize/8){
        let fd=(*item as u32) as i32;
        let mut stat=std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe{libc::fstat(fd,stat.as_mut_ptr())}==0{
            let stat=unsafe{stat.assume_init()};
            if stat.st_dev as u64==target.dev()&&stat.st_ino==target.ino(){return Err("文件正在被进程使用，已跳过".into())}
        }else if std::io::Error::last_os_error().raw_os_error()!=Some(libc::EBADF){return Err("文件句柄核验失败，已跳过".into())}
    }
    let path=std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_|"文件路径包含无效字符")?;
    let (send,receive)=std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result=(|| {
            let needed=unsafe { proc_listpidspath(1,0,path.as_ptr(),0,std::ptr::null_mut(),0) };
            if needed<0 { return Err(format!("文件占用检查失败，已跳过：{}",std::io::Error::last_os_error())); }
            // SDK规定初次查询返回当前进程所需的最小缓冲，额外余量应对进程出生。
            let bytes=(needed as usize).saturating_mul(2).max(65536);
            if bytes>16*1024*1024 { return Err("文件占用检查缓冲异常，已跳过".into()); }
            let mut pids=vec![0i32;bytes/4];
            let count=unsafe { proc_listpidspath(1,0,path.as_ptr(),0,pids.as_mut_ptr().cast(),(pids.len()*4) as libc::c_int) };
            if count<0 { return Err(format!("文件占用检查失败，已跳过：{}",std::io::Error::last_os_error())); }
            if count as usize>=pids.len()*4 || count%4!=0 { return Err("文件占用检查结果不完整，已跳过".into()); }
            if count>0 { return Err("文件正在被进程使用，已跳过".into()); }
            Ok(())
        })();
        let _=send.send(result);
    });
    receive.recv_timeout(Duration::from_secs(3)).map_err(|_|"文件占用检查超时或失败，已跳过".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    #[test] fn ignore_matches_descendants_and_data_alias_only_on_component_boundary() {
        let roots=vec![PathBuf::from("/Users/example/keep")];
        assert!(matches_ignored(Path::new("/Users/example/keep"),&roots));
        assert!(matches_ignored(Path::new("/Users/example/keep/a"),&roots));
        assert!(matches_ignored(Path::new("/System/Volumes/Data/Users/example/keep/a"),&roots));
        assert!(!matches_ignored(Path::new("/Users/example/keeper/a"),&roots));
        assert!(matches_ignored(Path::new("/Users/example/keep/a"),&[PathBuf::from("/System/Volumes/Data/Users/example/keep")]));
    }
    #[test] fn protected_system_descendants_include_data_alias() {
        assert!(is_protected(Path::new("/Library/Preferences/a.plist")));
        assert!(is_protected(Path::new("/System/Volumes/Data/Library/Preferences/a.plist")));
        assert!(is_protected(Path::new("/System/Volumes/Data/private/var/db/a")));
        assert!(!is_protected(Path::new("/Applications/Example.app")));
    }
    #[test] fn database_and_hardlinks_are_never_junk() {
        let root = home().unwrap().join(format!(".karuo-safety-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&root).unwrap();
        let db=root.join("important.sqlite");fs::write(&db,b"fixture").unwrap();
        assert!(!junk_metadata_safe(&db,&fs::symlink_metadata(&db).unwrap()));
        let file=root.join("old.cache");let link=root.join("other.cache");
        fs::write(&file,b"fixture").unwrap();
        assert!(junk_metadata_safe(&file,&fs::symlink_metadata(&file).unwrap()));
        fs::hard_link(&file,&link).unwrap();
        assert!(!junk_metadata_safe(&file,&fs::symlink_metadata(&file).unwrap()));
        fs::remove_dir_all(root).unwrap();
    }
    #[test] fn open_fixture_is_not_movable() {
        let path=home().unwrap().join(format!(".karuo-open-test-{}-{}",std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let file=fs::OpenOptions::new().write(true).create_new(true).open(&path).unwrap();
        let start=Instant::now();
        let churn=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let child=churn.clone();
        let worker=std::thread::spawn(move||{while child.load(std::sync::atomic::Ordering::Relaxed){drop(fs::File::open("/dev/null").unwrap());}});
        let results:Vec<_>=(0..32).map(|_|ensure_not_open(&path)).collect();
        churn.store(false,std::sync::atomic::Ordering::Relaxed);worker.join().unwrap();
        for result in results{let error=result.unwrap_err();assert!(error.contains("正在被进程使用"),"应检出本进程句柄，实际：{error}");}
        eprintln!("自有打开文件占用检查：{}ms",start.elapsed().as_millis());
        drop(file);
        let start=Instant::now();
        ensure_not_open(&path).expect("关闭句柄后的自有普通文件应允许清理");
        eprintln!("自有关闭文件占用检查：{}ms",start.elapsed().as_millis());
        fs::remove_file(path).unwrap();
    }
    #[test] fn protected_library_and_cloud_are_not_junk() {
        let h=home().unwrap();
        assert!(is_protected(&h.join("Library/Application Support/database")));
        assert!(is_protected(&h.join("Library/CloudStorage/cloud/file")));
        assert!(!is_protected(&h.join("Library/Caches/example/old.cache")));
    }
}

#[cfg(test)]mod presentation_tests{
 use super::*;
 #[test]fn protected_types_share_presentation_and_cleanup_rules(){
  for name in ["data.sqlite","data.sqlite-wal","identity.pfx","private.pem","passwords.txt","file.icloud"]{
   let p=Path::new("/tmp").join(name);assert!(path_protection_reason(&p).is_some(),"{name}");
  }
  assert!(path_protection_reason(Path::new("/System/Volumes/Data/private/var/file")).is_some());
  assert!(path_protection_reason(Path::new("/tmp/ordinary.log")).is_none());
 }
 #[test]fn user_protection_is_component_bounded_and_fail_closed(){
  let paths=vec![PathBuf::from("/tmp/project")];
  assert!(selection_protection_reason(Path::new("/tmp/project/file"),&paths).is_some());
  assert!(selection_protection_reason(Path::new("/tmp/project2/file"),&paths).is_none());
  let mut v=serde_json::json!({"path":"/tmp/ordinary.log"});annotate_protection(&mut v,&Err("broken".into()));
  assert_eq!(v["protected"],true);assert!(v["protection_reason"].as_str().unwrap().contains("无法读取"));
 }
}

/// npx packages can be loaded lazily; an open-file check alone is insufficient.
pub fn ensure_npx_idle()->Result<(),String>{let out=std::process::Command::new("/bin/ps").args(["-axo","comm="]).output().map_err(|_|"无法核实工具任务，保留npx缓存")?;if !out.status.success(){return Err("无法核实工具任务，保留npx缓存".into())}if download_task_running(&String::from_utf8_lossy(&out.stdout)){return Err("Node或包管理任务仍在运行，保留npx缓存".into())}Ok(())}
fn download_task_running(list:&str)->bool{list.lines().any(|line|matches!(std::path::Path::new(line.trim()).file_name().and_then(|n|n.to_str()),Some("node"|"nodejs"|"npm"|"npx"|"pnpm"|"yarn"|"bun")))}
#[cfg(test)]mod download_tests{use super::*;#[test]fn running_tool_names_are_exact(){assert!(download_task_running("/opt/homebrew/bin/node\n/bin/zsh"));assert!(download_task_running("npm"));assert!(!download_task_running("/Applications/Node Editor.app/Contents/MacOS/Node Editor\n/bin/zsh"));}}

pub fn npx_package_old(path:&Path)->bool{let Some(h)=home()else{return false};let root=h.join(".npm/_npx");let Ok(relative)=path.strip_prefix(&root)else{return false};let Some(component)=relative.components().next()else{return false};let package=root.join(component.as_os_str());let Ok(m)=fs::symlink_metadata(&package)else{return false};let now=SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==unsafe{libc::geteuid()}&&m.mtime()>=0&&(m.mtime()as u64)<=now.saturating_sub(30*86400)}
