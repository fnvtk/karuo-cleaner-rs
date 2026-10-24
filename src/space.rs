//! 磁盘空间透镜：只读挂载卷、增量目录索引，与清理授权完全分离。
use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    ffi::{CStr, CString},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use walkdir::WalkDir;

#[derive(Clone, Debug, Serialize)]
pub struct Volume {
    pub id: String,
    pub name: String,
    pub mount: String,
    pub scan_root: String,
    pub total: u64,
    pub available: u64,
    pub used: u64,
    pub internal: bool,
}
pub fn volumes() -> Vec<Volume> {
    let mut mounts = vec![PathBuf::from("/")];
    // MNT_NOWAIT 不刷新远端卷；只列本地磁盘，不唤醒网络挂载。
    unsafe {
        let mut ptr = std::ptr::null_mut();
        let count = libc::getmntinfo(&mut ptr, libc::MNT_NOWAIT);
        if count > 0 && !ptr.is_null() {
            for m in std::slice::from_raw_parts(ptr, count as usize) {
                let path = CStr::from_ptr(m.f_mntonname.as_ptr()).to_string_lossy();
                let dev = CStr::from_ptr(m.f_mntfromname.as_ptr()).to_string_lossy();
                if path.starts_with("/Volumes/")
                    && dev.starts_with("/dev/")
                    && m.f_flags & (libc::MNT_LOCAL as u32) != 0
                {
                    mounts.push(PathBuf::from(path.as_ref()));
                }
            }
        }
    }
    mounts.sort();
    mounts.dedup();
    mounts
        .into_iter()
        .filter_map(|mount| {
            let c = CString::new(mount.to_string_lossy().as_bytes()).ok()?;
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
                return None;
            }
            let total = (st.f_blocks as u64).saturating_mul(st.f_frsize as u64);
            let available = (st.f_bavail as u64).saturating_mul(st.f_frsize as u64);
            let internal = mount == Path::new("/");
            let scan_root = if internal && Path::new("/System/Volumes/Data").is_dir() {
                PathBuf::from("/System/Volumes/Data")
            } else {
                mount.clone()
            };
            let mut name = if internal {
                "Macintosh HD".to_string()
            } else {
                mount.file_name()?.to_string_lossy().into_owned()
            };
            if internal {
                if let Ok(out) = std::process::Command::new("/usr/sbin/diskutil")
                    .args(["info", "-plist", "/"])
                    .output()
                {
                    if let Ok(p) = plist::Value::from_reader_xml(out.stdout.as_slice()) {
                        if let Some(n) = p
                            .as_dictionary()
                            .and_then(|d| d.get("VolumeName"))
                            .and_then(|v| v.as_string())
                        {
                            name = n.into();
                        }
                    }
                }
            }
            Some(Volume {
                id: mount.to_string_lossy().into_owned(),
                name,
                mount: mount.to_string_lossy().into_owned(),
                scan_root: scan_root.to_string_lossy().into_owned(),
                total,
                available,
                used: total.saturating_sub(available),
                internal,
            })
        })
        .collect()
}
#[derive(Default)]
pub struct Cache {
    pub root: PathBuf,
    pub current: PathBuf,
    pub nodes: HashMap<PathBuf, (u64, bool)>,
    pub scanned: u64,
    pub skipped: u64,
    pub done: bool,
    pub truncated: bool,
    pub error: Option<String>,
    page: usize,
    query: String,
    show_hidden: bool,
    /// 已扫描目录的直接子项，view() 不再反复读盘或逐项 lstat。
    pub children: HashMap<PathBuf, Vec<SpaceChild>>,
    /// 每个目录的直接文件空间；扫描收尾时按目录数量自底向上汇总。
    direct_sizes: HashMap<PathBuf, u64>,
    /// 只记录受跳过项影响的目录及其祖先，不把无关目录标为未完成。
    incomplete_dirs: HashSet<PathBuf>,
    tombstones: HashSet<PathBuf>,
}
#[derive(Clone, Debug)]
pub struct SpaceChild {
    name: OsString,
    size: u64,
    is_dir: bool,
    device: u64,
    inode: u64,
}
impl Cache {
    pub fn view(&mut self) -> serde_json::Value {
        let page=self.page; let query=self.query.clone(); let hidden=self.show_hidden;
        self.view_page(page,&query,hidden)
    }
    pub fn view_page(&mut self,page:usize,query:&str,show_hidden:bool)->serde_json::Value {
        self.page=page; self.query=query.to_owned(); self.show_hidden=show_hidden;
        let node_sizes = &self.nodes;
        let incomplete_dirs = &self.incomplete_dirs;
        let scan_stopped = self.done && self.truncated;
        let query_lower=query.to_lowercase();
        // 只给本页50项构建完整JSON，避免巨大目录每次刷新都复制所有路径。
        let mut candidates:Vec<_>=self.children.get(&self.current).into_iter().flatten()
            .filter(|child|(show_hidden||!child.name.to_string_lossy().starts_with('.'))&&child.name.to_string_lossy().to_lowercase().contains(&query_lower))
            .map(|child|{let size=if child.is_dir{node_sizes.get(&self.current.join(&child.name)).map(|n|n.0).unwrap_or(0)}else{child.size};(child,size)}).collect();
        candidates.sort_by(|a,b|b.1.cmp(&a.1).then_with(||a.0.name.cmp(&b.0.name)));
        let item_total=candidates.len();let page_count=item_total.div_ceil(50).max(1);let page=page.min(page_count-1);self.page=page;
        let ignored=crate::safety::load_ignored_paths();
        let entries:Vec<_>=candidates.into_iter().skip(page*50).take(50).map(|(child,size)|{
            let path=self.current.join(&child.name);
            let complete=child.is_dir&&self.done&&!scan_stopped&&!incomplete_dirs.contains(&path);
            let mut value=space_entry(&path,&self.root,size,child.is_dir,child.device,child.inode,!child.is_dir||size>0||complete,!child.is_dir||complete);crate::safety::annotate_protection(&mut value,&ignored);value
        }).collect();
        let total = self.nodes.get(&self.current).map(|x| x.0).unwrap_or(0);
        let complete =
            self.done && !self.truncated && !self.incomplete_dirs.contains(&self.current);
        serde_json::json!({"path":self.current.to_string_lossy().into_owned(),"root":self.root.to_string_lossy().into_owned(),"entries":entries,"total":item_total,"page":page,"page_count":page_count,"page_size":50,"scanned":self.scanned,"skipped":self.skipped,"done":self.done,"truncated":self.truncated,"complete":complete,"size":total,"error":self.error})
    }
    /// 操作成功后从只读索引剔除已移动项目，不重新读取磁盘。
    fn removed(&self,path:&Path)->bool { path.ancestors().any(|p|self.tombstones.contains(p)) }
    pub fn remove_paths(&mut self, paths:&[String]) {
        let removed:HashSet<PathBuf>=paths.iter().map(PathBuf::from).filter(|p|p.starts_with(&self.root)).collect();
        self.tombstones.extend(removed.iter().cloned());
        let mut parents:HashSet<PathBuf>=HashSet::new();let dirs:HashSet<PathBuf>=removed.iter().filter(|p|self.nodes.contains_key(*p)).cloned().collect();
        for p in &removed{if let Some(parent)=p.parent(){parents.insert(parent.to_path_buf());}}
        for parent in parents {if let Some(items)=self.children.get_mut(&parent){let mut bytes=0u64;items.retain(|c|{if removed.contains(&parent.join(&c.name)){if !c.is_dir{bytes=bytes.saturating_add(c.size)}false}else{true}});if let Some(n)=self.direct_sizes.get_mut(&parent){*n=n.saturating_sub(bytes);}}}
        if !dirs.is_empty(){let under=|p:&Path|p.ancestors().any(|a|dirs.contains(a));self.nodes.retain(|p,_|!under(p));self.direct_sizes.retain(|p,_|!under(p));self.children.retain(|p,_|!under(p));self.incomplete_dirs.retain(|p|!under(p));if under(&self.current){self.current=self.root.clone();self.page=0;}}
        let root=self.root.clone();aggregate_directory_sizes(self,&root);
    }
    /// 元数据在锁外读取；只恢复本次曾移走的索引普通文件。
    pub fn restore_files(&mut self,files:&[(PathBuf,u64,u64,u64)]){
        for (p,size,dev,ino) in files{if !p.starts_with(&self.root)||!self.tombstones.remove(p){continue}let Some(parent)=p.parent()else{continue};if !self.children.contains_key(parent){continue}let list=self.children.get_mut(parent).unwrap();if list.iter().any(|c|parent.join(&c.name)==*p){continue}list.push(SpaceChild{name:p.file_name().unwrap_or_default().into(),size:*size,is_dir:false,device:*dev,inode:*ino});*self.direct_sizes.entry(parent.to_path_buf()).or_default()+=size;}
        let root=self.root.clone();aggregate_directory_sizes(self,&root);
    }
    /// 只展开已索引普通文件。调用者释放缓存锁后才做文件身份复核。
    pub fn cleanup_candidates(&self,ids:&[String],selection:Option<(&str,bool)>)->Result<Vec<(PathBuf,u64,u64)>,String>{
        let direct=self.children.get(&self.current).into_iter().flatten();
        let wanted:HashSet<PathBuf>=ids.iter().map(PathBuf::from).collect();let query=selection.map(|(q,_)|q.to_lowercase()).unwrap_or_default();
        let selected:HashSet<PathBuf>=direct.filter(|c|{let p=self.current.join(&c.name);if let Some((_,hidden))=selection {(hidden||!c.name.to_string_lossy().starts_with('.'))&&c.name.to_string_lossy().to_lowercase().contains(&query)}else{wanted.contains(&p)}}).map(|c|self.current.join(&c.name)).collect();
        if selection.is_none()&&!wanted.is_subset(&selected){return Err("选择不属于当前索引目录".into())}
        let mut out=Vec::new();
        // 目录按祖先集合判定一次，避免数百万索引文件乘以数千选择项。
        for (dir,children) in &self.children {
            if !dir.starts_with(&self.current){continue}
            let covered=dir.ancestors().take_while(|p|p.starts_with(&self.current)).any(|p|selected.contains(p));
            if !covered&&dir!=&self.current {continue}
            for c in children {if c.is_dir{continue}let p=dir.join(&c.name);if !self.removed(&p)&&(covered||selected.contains(&p)){out.push((p,c.device,c.inode));}}
        }
        Ok(out)
    }
    pub fn browse(&mut self, path: &Path) -> bool {
        if path == self.root || self.nodes.get(path).is_some_and(|v| v.1) {
            self.current = path.to_path_buf();
            true
        } else {
            false
        }
    }
}
/// 批量合并索引，界面最多每350ms更新，不把数百万文件送进WebView。
pub fn scan(
    root: PathBuf,
    cancel: Arc<AtomicBool>,
    cache: Arc<Mutex<Cache>>,
    publish: impl Fn(serde_json::Value),
) {
    let meta = match fs::symlink_metadata(&root) {
        Ok(meta) => meta,
        Err(error) => {
            let mut c = cache.lock().unwrap();
            *c = Cache {
                root: root.clone(),
                current: root.clone(),
                scanned: 0,
                skipped: 1,
                done: true,
                truncated: true,
                error: Some(format!("无法读取扫描目录元数据：{error}")),
                ..Cache::default()
            };
            c.nodes.insert(root.clone(), (0, true));
            c.direct_sizes.insert(root.clone(), 0);
            c.children.insert(root.clone(), Vec::new());
            let view = c.view();
            drop(c);
            publish(view);
            return;
        }
    };
    let dev = meta.dev();
    let mut seen = HashSet::new();
    let mut last = Instant::now();
    let started = Instant::now();
    let mut batch = Vec::new();
    let mut scanned = 0u64;
    let mut skipped = 0u64;
    let mut truncated = false;
    {
        let mut c = cache.lock().unwrap();
        *c = Cache {
            root: root.clone(),
            current: root.clone(),
            ..Cache::default()
        };
        c.nodes.insert(root.clone(), (0, true));
        c.direct_sizes.insert(root.clone(), 0);
        c.children.insert(root.clone(), Vec::new());
    }
    publish(cache.lock().unwrap().view());
    #[cfg(target_os = "macos")]
    let fast_ok = scan_bulk_macos(
        &root,
        dev,
        &cancel,
        &cache,
        &publish,
        &mut seen,
        &mut scanned,
        &mut skipped,
        &mut truncated,
        &started,
        &mut last,
    );
    #[cfg(not(target_os = "macos"))]
    let fast_ok = false;
    if !fast_ok {
        // 不支持 getattrlistbulk 的文件系统才走兼容路径；它仍保留取消和读取错误处理。
        scan_walkdir_fallback(
            &root,
            dev,
            &cancel,
            &cache,
            &publish,
            &mut seen,
            &mut scanned,
            &mut skipped,
            &mut truncated,
            &started,
            &mut last,
            &mut batch,
        );
        merge(&cache, &root, &mut batch, scanned, skipped);
    }
    finalize_directory_sizes(&cache, &root);
    {
        let mut c = cache.lock().unwrap();
        c.scanned = scanned;
        c.skipped = skipped;
        c.done = true;
        c.truncated = c.truncated || truncated || cancel.load(Ordering::Relaxed);
    }
    publish(cache.lock().unwrap().view());
}

#[cfg(target_os = "macos")]
fn scan_bulk_macos(
    root: &Path,
    root_dev: u64,
    cancel: &AtomicBool,
    cache: &Mutex<Cache>,
    publish: &impl Fn(serde_json::Value),
    seen: &mut HashSet<(u64, u64)>,
    scanned: &mut u64,
    skipped: &mut u64,
    truncated: &mut bool,
    _started: &Instant,
    last: &mut Instant,
) -> bool {
    use std::{
        ffi::OsStr,
        os::unix::{ffi::OsStrExt, io::AsRawFd},
    };
    const BUF_SIZE: usize = 8192;
    const ATTR_CMN_NAME: u32 = 0x0000_0001;
    const ATTR_CMN_DEVID: u32 = 0x0000_0002;
    const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
    const ATTR_CMN_FILEID: u32 = 0x0200_0000;
    const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
    const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
    const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
    const VREG: u32 = 1;
    const VDIR: u32 = 2;
    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct AttrSet {
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }
    unsafe extern "C" {
        fn getattrlistbulk(
            fd: libc::c_int,
            list: *const AttrList,
            buf: *mut libc::c_void,
            size: usize,
            options: u64,
        ) -> libc::c_int;
    }
    let mut stack = vec![root.to_path_buf()];
    let mut dirs = vec![root.to_path_buf()];
    let mut buf = [0u8; BUF_SIZE];
    let attr_list = AttrList {
        bitmapcount: 5,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_NAME
            | ATTR_CMN_DEVID
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    };
    let mut first_bulk_call = true;
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(file) = fs::File::open(&dir) else {
            *skipped += 1;
            mark_incomplete(&mut cache.lock().unwrap(), &dir, root);
            continue;
        };
        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let count = unsafe {
                getattrlistbulk(
                    file.as_raw_fd(),
                    &attr_list,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    0,
                )
            };
            let is_first_bulk_call = first_bulk_call;
            first_bulk_call = false;
            if count < 0 {
                let code = std::io::Error::last_os_error().raw_os_error();
                if is_first_bulk_call
                    && dir == root
                    && matches!(code, Some(libc::EINVAL | libc::ENOTSUP | libc::ENOSYS))
                {
                    return false;
                }
                *skipped += 1;
                mark_incomplete(&mut cache.lock().unwrap(), &dir, root);
                break;
            }
            if count == 0 {
                break;
            }
            let count = count as usize;
            if count > buf.len() / 24 {
                *skipped += 1;
                mark_incomplete(&mut cache.lock().unwrap(), &dir, root);
                break;
            }
            let mut page_items = Vec::with_capacity(count);
            let mut page_dirs = Vec::new();
            let skipped_before_page = *skipped;
            let mut offset = 0usize;
            for _ in 0..count {
                if cancel.load(Ordering::Relaxed)
                {
                    *truncated = true;
                    break;
                }
                let Some(header_end) = offset.checked_add(24) else {
                    *skipped += 1;
                    break;
                };
                if header_end > buf.len() {
                    *skipped += 1;
                    break;
                }
                let Some(entry_len) = read_attr_value::<u32>(&buf, offset, buf.len()) else {
                    *skipped += 1;
                    break;
                };
                let entry_len = entry_len as usize;
                let Some(entry_end) = offset.checked_add(entry_len) else {
                    *skipped += 1;
                    break;
                };
                if entry_len < 24 || entry_end > buf.len() {
                    *skipped += 1;
                    break;
                }
                let base = offset;
                let Some(returned) = read_attr_value::<AttrSet>(&buf, offset + 4, entry_end) else {
                    *skipped += 1;
                    offset = entry_end;
                    continue;
                };
                let required_common =
                    ATTR_CMN_NAME | ATTR_CMN_DEVID | ATTR_CMN_OBJTYPE | ATTR_CMN_FILEID;
                if returned.commonattr & required_common != required_common {
                    *skipped += 1;
                    offset = entry_end;
                    continue;
                }
                let mut field = offset + 24;
                let Some(name_ref) = read_attr_value::<(i32, u32)>(&buf, field, entry_end) else {
                    *skipped += 1;
                    offset = entry_end;
                    continue;
                };
                let name_ref_at = field;
                field += 8;
                let dev_id = if returned.commonattr & ATTR_CMN_DEVID != 0 {
                    let Some(x) = read_attr_value::<u32>(&buf, field, entry_end) else {
                        *skipped += 1;
                        offset = entry_end;
                        continue;
                    };
                    field += 4;
                    x as u64
                } else {
                    0
                };
                let obj_type = if returned.commonattr & ATTR_CMN_OBJTYPE != 0 {
                    let Some(x) = read_attr_value::<u32>(&buf, field, entry_end) else {
                        *skipped += 1;
                        offset = entry_end;
                        continue;
                    };
                    field += 4;
                    x
                } else {
                    0
                };
                let file_id = if returned.commonattr & ATTR_CMN_FILEID != 0 {
                    let Some(x) = read_attr_value::<u64>(&buf, field, entry_end) else {
                        *skipped += 1;
                        offset = entry_end;
                        continue;
                    };
                    field += 8;
                    x
                } else {
                    0
                };
                let link_count = if returned.fileattr & ATTR_FILE_LINKCOUNT != 0 {
                    let Some(x) = read_attr_value::<u32>(&buf, field, entry_end) else {
                        *skipped += 1;
                        offset = entry_end;
                        continue;
                    };
                    field += 4;
                    x
                } else {
                    1
                };
                let alloc_size = if returned.fileattr & ATTR_FILE_ALLOCSIZE != 0 {
                    let Some(x) = read_attr_value::<i64>(&buf, field, entry_end) else {
                        *skipped += 1;
                        offset = entry_end;
                        continue;
                    };
                    x.max(0) as u64
                } else {
                    0
                };
                let name_start = if name_ref.0 >= 0 {
                    name_ref_at.checked_add(name_ref.0 as usize)
                } else {
                    name_ref_at.checked_sub((-(name_ref.0 as i64)) as usize)
                };
                let Some(name_start) = name_start else {
                    *skipped += 1;
                    offset = entry_end;
                    continue;
                };
                let Some(name_end) = name_start.checked_add(name_ref.1 as usize) else {
                    *skipped += 1;
                    offset = entry_end;
                    continue;
                };
                if name_start < base || name_start >= entry_end || name_end > entry_end {
                    *skipped += 1;
                    offset = entry_end;
                    continue;
                }
                let bytes = &buf[name_start..name_end];
                let bytes = bytes.split(|b| *b == 0).next().unwrap_or_default();
                if bytes.is_empty() {
                    *skipped += 1;
                    offset = entry_end;
                    continue;
                }
                let name = OsStr::from_bytes(bytes);
                let child = dir.join(name);
                *scanned += 1;
                if dev_id != root_dev {
                    *skipped += 1;
                    offset = entry_end;
                    continue;
                }
                if obj_type == VDIR {
                    if matches!(
                        name.to_str(),
                        Some(".Spotlight-V100" | ".fseventsd" | ".DocumentRevisions-V100")
                    ) {
                        *skipped += 1;
                        offset = entry_end;
                        continue;
                    }
                    page_items.push(SpaceChild {
                        name: name.to_os_string(),
                        size: 0,
                        is_dir: true,
                        device: dev_id,
                        inode: file_id,
                    });
                    page_dirs.push(child);
                } else if obj_type == VREG {
                    let required_file = ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE;
                    if returned.fileattr & required_file != required_file {
                        *skipped += 1;
                        offset = entry_end;
                        continue;
                    }
                    let size = if link_count > 1 && !seen.insert((dev_id, file_id)) {
                        0
                    } else {
                        alloc_size
                    };
                    page_items.push(SpaceChild {
                        name: name.to_os_string(),
                        size,
                        is_dir: false,
                        device: dev_id,
                        inode: file_id,
                    });
                } else {
                    // 符号链接及特殊文件不跟随、不计入文件空间，但明确计为跳过项。
                    *skipped += 1;
                }
                offset = entry_end;
            }
            {
                let mut c = cache.lock().unwrap();
                if c.removed(&dir) {break;}
                page_items.retain(|item|!c.removed(&dir.join(&item.name)));
                page_dirs.retain(|path|!c.removed(path));
                let page_bytes=page_items.iter().filter(|i|!i.is_dir).fold(0u64,|n,i|n.saturating_add(i.size));
                add_page_sizes(&mut c, &dir, root, page_bytes);
                c.children
                    .entry(dir.clone())
                    .or_default()
                    .extend(page_items);
                for child in page_dirs.drain(..) {
                    c.nodes.entry(child.clone()).or_insert((0, true));
                    c.direct_sizes.entry(child.clone()).or_insert(0);
                    c.children.entry(child.clone()).or_default();
                    stack.push(child.clone());
                    dirs.push(child);
                }
                c.scanned = *scanned;
                c.skipped = *skipped;
                if *skipped > skipped_before_page {
                    mark_incomplete(&mut c, &dir, root);
                }
            }
            if last.elapsed() >= Duration::from_millis(350) {
                publish(cache.lock().unwrap().view());
                *last = Instant::now();
            }
            if *truncated || cancel.load(Ordering::Relaxed) {
                break;
            }
        }
        if *truncated || cancel.load(Ordering::Relaxed) {
            break;
        }
    }
    true
}

fn space_entry(
    path: &Path,
    root: &Path,
    size: u64,
    is_dir: bool,
    device: u64,
    inode: u64,
    measured: bool,
    complete: bool,
) -> serde_json::Value {
    let p = path.to_string_lossy().into_owned();
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let reason=crate::safety::path_protection_reason(path);let protected=reason.is_some();
    serde_json::json!({"id":p,"name":name,"path":p,"size":size,"modified":null,"protected":protected,"protection_reason":reason,
        "kind":if is_dir {"space_folder"} else {"space_file"},
        "detail":"已分配空间；目录汇总将在扫描完成后确认","is_dir":is_dir,
        "scope_root":root.to_string_lossy(),"inode":inode,"device":device,"mtime_nsec":0,"measured":measured,"complete":complete})
}

/// 每个 bulk 页只沿已建立的目录父链传播一次，及时更新可见空间。
/// direct_sizes 保留每个目录的原始直接文件占用，扫描结束时仍会用于完整校准。
fn add_page_sizes(c: &mut Cache, dir: &Path, root: &Path, bytes: u64) {
    if bytes == 0 || !dir.starts_with(root) {
        return;
    }
    if let Some(local) = c.direct_sizes.get_mut(dir) {
        *local = local.saturating_add(bytes);
    }
    let mut current = Some(dir);
    while let Some(path) = current {
        if !path.starts_with(root) {
            break;
        }
        if let Some(node) = c.nodes.get_mut(path) {
            node.0 = node.0.saturating_add(bytes);
        }
        if path == root {
            break;
        }
        current = path.parent();
    }
}

/// 从一个 getattrlistbulk 记录内安全读取定长字段，拒绝越过记录或缓冲区边界。
fn read_attr_value<T: Copy>(buf: &[u8], offset: usize, entry_end: usize) -> Option<T> {
    let end = offset.checked_add(std::mem::size_of::<T>())?;
    if end > entry_end || end > buf.len() {
        return None;
    }
    // 范围已验证；attrlist 字段按 ABI 允许非对齐读取。
    Some(unsafe { std::ptr::read_unaligned(buf.as_ptr().add(offset).cast::<T>()) })
}

/// 从只读直接值生成目录大小快照；反复调用不会累加旧快照。
fn aggregate_directory_sizes(c: &mut Cache, root: &Path) {
    for (path, direct) in &c.direct_sizes {
        if let Some(node) = c.nodes.get_mut(path) {
            node.0 = *direct;
        }
    }
    let mut dirs: Vec<PathBuf> = c.direct_sizes.keys().cloned().collect();
    // 深目录先合并到父目录，只改展示快照，不改 direct_sizes 原值。
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for dir in dirs {
        let total = c.nodes.get(&dir).map(|n| n.0).unwrap_or(0);
        if dir != root {
            if let Some(parent) = dir.parent() {
                if let Some(node) = c.nodes.get_mut(parent) {
                    node.0 = node.0.saturating_add(total);
                }
            }
        }
    }
}

fn finalize_directory_sizes(cache: &Mutex<Cache>, root: &Path) {
    aggregate_directory_sizes(&mut cache.lock().unwrap(), root);
}

fn mark_incomplete(c: &mut Cache, path: &Path, root: &Path) {
    let mut current = Some(path);
    while let Some(dir) = current {
        if !dir.starts_with(root) {
            break;
        }
        c.incomplete_dirs.insert(dir.to_path_buf());
        if dir == root {
            break;
        }
        current = dir.parent();
    }
}

fn scan_walkdir_fallback(
    root: &Path,
    dev: u64,
    cancel: &AtomicBool,
    cache: &Mutex<Cache>,
    publish: &impl Fn(serde_json::Value),
    seen: &mut HashSet<(u64, u64)>,
    scanned: &mut u64,
    skipped: &mut u64,
    truncated: &mut bool,
    _started: &Instant,
    last: &mut Instant,
    batch: &mut Vec<(PathBuf, u64, bool)>,
) {
    let mut stack = vec![WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .same_file_system(true)
        .into_iter()];
    while let Some(mut iter) = stack.pop() {
        let Some(item) = iter.next() else {
            continue;
        };
        stack.push(iter);
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match item {
            Ok(e) => {
                *scanned += 1;
                if e.file_type().is_symlink() {
                    *skipped += 1;
                    if let Some(parent) = e.path().parent() {
                        mark_incomplete(&mut cache.lock().unwrap(), parent, root);
                    }
                    continue;
                }
                match e.metadata() {
                    Ok(m) if m.dev() == dev && m.is_dir() => {
                        let p = e.path().to_path_buf();
                        let mut c = cache.lock().unwrap();
                        if c.removed(&p) {continue;}
                        {
                            c.nodes.entry(p.clone()).or_insert((0, true));
                            c.direct_sizes.entry(p.clone()).or_insert(0);
                            c.children.entry(p.clone()).or_default();
                            if let Some(parent) = p.parent() {
                                c.children.entry(parent.to_path_buf()).or_default().push(
                                    SpaceChild {
                                        name: e.file_name().to_os_string(),
                                        size: 0,
                                        is_dir: true,
                                        device: m.dev(),
                                        inode: m.ino(),
                                    },
                                );
                            }
                        }
                        drop(c);
                        if matches!(
                            e.file_name().to_str(),
                            Some(".Spotlight-V100" | ".fseventsd" | ".DocumentRevisions-V100")
                        ) {
                            *skipped += 1;
                            if let Some(parent) = p.parent() {
                                mark_incomplete(&mut cache.lock().unwrap(), parent, root);
                            }
                            continue;
                        }
                        batch.push((p, 0, true));
                    }
                    Ok(m) if m.dev() == dev && m.is_file() => {
                        let size = if m.nlink() > 1 && !seen.insert((m.dev(), m.ino())) {
                            0
                        } else {
                            m.blocks().saturating_mul(512)
                        };
                        if let Some(parent) = e.path().parent() {
                            let mut c=cache.lock().unwrap();
                            if c.removed(e.path()) {continue;}
                            c.children
                                .entry(parent.to_path_buf())
                                .or_default()
                                .push(SpaceChild {
                                    name: e.file_name().to_os_string(),
                                    size,
                                    is_dir: false,
                                    device: m.dev(),
                                    inode: m.ino(),
                                });
                        }
                        batch.push((e.path().to_path_buf(), size, false));
                    }
                    _ => {
                        *skipped += 1;
                        if let Some(parent) = e.path().parent() {
                            mark_incomplete(&mut cache.lock().unwrap(), parent, root);
                        }
                    }
                }
            }
            Err(error) => {
                *skipped += 1;
                if let Some(path) = error.path() {
                    if let Some(parent) = path.parent() {
                        mark_incomplete(&mut cache.lock().unwrap(), parent, root);
                    }
                }
            }
        }
        if batch.len() >= 512 || last.elapsed() >= Duration::from_millis(350) {
            merge(cache, root, batch, *scanned, *skipped);
            if last.elapsed() >= Duration::from_millis(350) {
                publish(cache.lock().unwrap().view());
                *last = Instant::now();
            }
        }
    }
}
fn merge(
    cache: &Mutex<Cache>,
    root: &Path,
    batch: &mut Vec<(PathBuf, u64, bool)>,
    scanned: u64,
    skipped: u64,
) {
    let mut c = cache.lock().unwrap();
    c.scanned = scanned;
    c.skipped = skipped;
    for (path, size, is_dir) in batch.drain(..) {
        if c.removed(&path) {continue;}
        if is_dir {
            {
                c.nodes.entry(path.clone()).or_insert((0, true));
                c.direct_sizes.entry(path.clone()).or_insert(0);
                c.children.entry(path).or_default();
            }
            continue;
        }
        if let Some(parent) = path.parent() {
            if let Some(local) = c.direct_sizes.get_mut(parent) {
                *local = local.saturating_add(size);
            }
        }
        let mut parent = path.parent();
        while let Some(p) = parent {
            if !p.starts_with(root) {
                break;
            }
            if let Some(n) = c.nodes.get_mut(p) {
                n.0 = n.0.saturating_add(size);
            } else {
                c.nodes.insert(p.to_path_buf(), (size, true));
            }
            if p == root {
                break;
            }
            parent = p.parent();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_selection_is_index_bound_and_expands_only_files() {
        let root=PathBuf::from("/fixture");let dir=root.join("A");let mut c=Cache{root:root.clone(),current:root.clone(),..Cache::default()};
        c.children.insert(root.clone(),vec![SpaceChild{name:"A".into(),size:0,is_dir:true,device:1,inode:1}]);
        c.children.insert(dir.clone(),vec![SpaceChild{name:"file".into(),size:2,is_dir:false,device:1,inode:2}]);
        let found=c.cleanup_candidates(&[dir.to_string_lossy().into()],None).unwrap();assert_eq!(found.len(),1);assert_eq!(found[0].0,dir.join("file"));
        assert!(c.cleanup_candidates(&["/System/arbitrary".into()],None).is_err());
        assert_eq!(c.cleanup_candidates(&[],Some(("A",true))).unwrap().len(),1);
    }
    #[test]
    fn removed_paths_cannot_return_from_queued_batch() {
        let root=PathBuf::from("/fixture");let removed=root.join("A");
        let cache=Mutex::new(Cache{root:root.clone(),current:root.clone(),..Cache::default()});
        {let mut c=cache.lock().unwrap();c.nodes.insert(root.clone(),(0,true));c.direct_sizes.insert(root.clone(),0);c.remove_paths(&[removed.to_string_lossy().into()]);}
        let mut batch=vec![(removed.clone(),0,true),(removed.join("late"),8192,false),(root.join("retained"),4096,false)];
        merge(&cache,&root,&mut batch,3,0);
        let c=cache.lock().unwrap();assert!(!c.nodes.contains_key(&removed));assert_eq!(c.nodes[&root].0,4096);assert!(c.removed(&removed.join("late")));
    }
    #[test]
    fn more_than_5000_items_remain_reachable_in_pages() {
        let root=PathBuf::from("/fixture");
        let mut c=Cache {root:root.clone(),current:root.clone(),..Cache::default()};
        c.children.insert(root,(0..5101).map(|i|SpaceChild{name:format!("item-{i:05}").into(),size:i,is_dir:false,device:1,inode:i}).collect());
        let mut seen=HashSet::new();
        for page in 0..103 { let v=c.view_page(page,"",true);assert_eq!(v["total"],5101);for e in v["entries"].as_array().unwrap(){seen.insert(e["path"].as_str().unwrap().to_owned());} }
        assert_eq!(seen.len(),5101);
        let v=c.view_page(0,"item-05100",true);assert_eq!(v["total"],1);
    }
    #[test]
    fn attr_field_reads_reject_record_and_buffer_overruns() {
        let buf = [0u8; 8];
        assert_eq!(read_attr_value::<u32>(&buf, 4, 8), Some(0));
        assert_eq!(read_attr_value::<u32>(&buf, 5, 8), None);
        assert_eq!(read_attr_value::<u64>(&buf, 0, 7), None);
        assert_eq!(read_attr_value::<u32>(&buf, usize::MAX, 8), None);
    }

    #[test]
    fn invalid_scan_root_publishes_terminal_error_view() {
        let root = std::env::temp_dir().join(format!(
            "karuo-space-missing-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cache = Arc::new(Mutex::new(Cache::default()));
        let published = Mutex::new(None);
        scan(root, Arc::new(AtomicBool::new(false)), cache.clone(), |view| {
            *published.lock().unwrap() = Some(view);
        });
        let view = published.lock().unwrap().clone().unwrap();
        assert_eq!(view["done"], true);
        assert_eq!(view["truncated"], true);
        assert_eq!(view["scanned"], 0);
        assert_eq!(view["skipped"], 1);
        assert!(view["error"].as_str().is_some_and(|e| !e.is_empty()));
        assert!(cache.lock().unwrap().done);
    }

    #[test]
    fn view_serializes_non_utf8_root_paths_lossily() {
        use std::os::unix::ffi::OsStringExt;
        let root = PathBuf::from(OsString::from_vec(b"/tmp/karuo-\xff".to_vec()));
        let mut c = Cache {
            root: root.clone(),
            current: root.clone(),
            ..Cache::default()
        };
        c.nodes.insert(root.clone(), (0, true));
        c.children.insert(root, Vec::new());
        let view = c.view();
        assert!(view["root"].as_str().unwrap().contains('\u{fffd}'));
    }

    #[test]
    fn bulk_page_sizes_match_final_direct_size_aggregation() {
        let root = PathBuf::from("/fixture");
        let a = root.join("A");
        let b = a.join("B");
        let mut c = Cache::default();
        for path in [&root, &a, &b] {
            c.nodes.insert(path.to_path_buf(), (0, true));
            c.direct_sizes.insert(path.to_path_buf(), 0);
        }

        add_page_sizes(&mut c, &root, &root, 100);
        add_page_sizes(&mut c, &a, &root, 40);
        add_page_sizes(&mut c, &b, &root, 15);
        assert_eq!(c.nodes[&root].0, 155);
        assert_eq!(c.nodes[&a].0, 55);
        assert_eq!(c.nodes[&b].0, 15);
        assert_eq!(c.direct_sizes[&root], 100);
        assert_eq!(c.direct_sizes[&a], 40);
        assert_eq!(c.direct_sizes[&b], 15);

        aggregate_directory_sizes(&mut c, &root);
        assert_eq!(c.nodes[&root].0, 155);
        assert_eq!(c.nodes[&a].0, 55);
        assert_eq!(c.nodes[&b].0, 15);
    }

    #[test]
    fn index_aggregates_and_browse_stays_in_index() {
        let root = PathBuf::from("/fixture");
        let cache = Mutex::new(Cache {
            root: root.clone(),
            current: root.clone(),
            ..Cache::default()
        });
        let mut b = vec![
            (root.join("A/f"), 4096, false),
            (root.join("B/g"), 8192, false),
        ];
        merge(&cache, &root, &mut b, 4, 0);
        let mut c = cache.lock().unwrap();
        assert_eq!(c.nodes[&root].0, 12288);
        assert_eq!(c.nodes[&root.join("A")].0, 4096);
        assert!(c.browse(&root.join("A")));
        assert!(!c.browse(Path::new("/etc")));

        let child = root.join("A/B");
        c.nodes.clear();
        c.direct_sizes.clear();
        c.nodes.insert(root.clone(), (0, true));
        c.nodes.insert(root.join("A"), (0, true));
        c.nodes.insert(child.clone(), (0, true));
        c.direct_sizes.insert(root.clone(), 1024);
        c.direct_sizes.insert(root.join("A"), 4096);
        c.direct_sizes.insert(child.clone(), 8192);
        aggregate_directory_sizes(&mut c, &root);
        assert_eq!(c.nodes[&root].0, 13_312);
        aggregate_directory_sizes(&mut c, &root);
        assert_eq!(c.nodes[&root].0, 13_312);
        assert_eq!(c.direct_sizes[&root], 1024);
    }
    #[test]
    fn installed_volumes_have_capacity() {
        let v = volumes();
        assert!(v
            .iter()
            .any(|v| v.id == "/" && v.total > 0 && v.available <= v.total));
    }
    #[test]
    fn physical_scan_indexes_packages_and_ignores_links() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "karuo-space-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("Sample.app/Contents")).unwrap();
        let data = root.join("Sample.app/Contents/data");
        fs::write(&data, vec![19u8; 16384]).unwrap();
        fs::hard_link(&data, root.join("Sample.app/Contents/data-hardlink")).unwrap();
        symlink(
            "/Applications",
            root.join("Sample.app/Contents/external-link"),
        )
        .unwrap();
        let cache = Arc::new(Mutex::new(Cache::default()));
        scan(
            root.clone(),
            Arc::new(AtomicBool::new(false)),
            cache.clone(),
            |_| {},
        );
        {
            let mut c = cache.lock().unwrap();
            assert!(c.done);
            assert_eq!(
                c.nodes[&root].0,
                fs::metadata(&data).unwrap().blocks().saturating_mul(512)
            );
            assert_eq!(c.nodes[&root].0, c.nodes[&root.join("Sample.app")].0);
            assert!(!c
                .nodes
                .contains_key(&root.join("Sample.app/Contents/external-link")));
            assert!(c.nodes.contains_key(&root.join("Sample.app/Contents")));
            let root_view = c.view();
            let app = root_view["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == "Sample.app")
                .unwrap();
            assert_eq!(
                app["size"].as_u64().unwrap(),
                c.nodes[&root.join("Sample.app")].0
            );
            assert_eq!(app["measured"], true);
            assert_eq!(app["complete"], false);
            assert_eq!(c.children[&root.join("Sample.app/Contents")].len(), 2);
            assert!(c.browse(&root.join("Sample.app/Contents")));
            let view = c.view();
            assert_eq!(view["entries"].as_array().unwrap().len(), 2);
            assert!(view["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == "data"));
            assert!(view["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == "data-hardlink"));
            assert_eq!(
                view["size"].as_u64().unwrap(),
                c.nodes[&root.join("Sample.app/Contents")].0
            );
            assert_eq!(view["complete"], false);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
