//! 只读磁盘扫描。扫描结果记录文件身份，供后续操作前重新核验。

use crate::model::{Entry, ScanResult};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const DAY_SECONDS: u64 = 24 * 60 * 60;

struct ScanState<'a> {
    cancel: &'a AtomicBool,
    result: ScanResult,
    stopped: bool,
    cancel_note_added: bool,
}

impl<'a> ScanState<'a> {
    fn new(cancel: &'a AtomicBool) -> Self {
        Self {
            cancel,
            result: ScanResult::default(),
            stopped: false,
            cancel_note_added: false,
        }
    }

    /// 扫描每一个目录项之前检查取消和总遍历上限。
    fn visit(&mut self) -> bool {
        if self.cancel.load(Ordering::Relaxed) {
            self.stopped = true;
            if !self.cancel_note_added {
                self.result
                    .notes
                    .push("扫描已取消，结果仅包含取消前读取到的项目。".into());
                self.cancel_note_added = true;
            }
            return false;
        }
        self.result.scanned += 1;
        true
    }

    fn append(&mut self, entry: Entry) {
        self.result.entries.push(entry);
    }

    fn skipped(&mut self, path: &Path) {
        self.result.skipped = self.result.skipped.saturating_add(1);
        if self.result.notes.len() < 20 {
            self.result.notes.push(format!(
                "无法读取项目，已跳过：{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }

    fn finish(mut self) -> ScanResult {
        let note = "体积按逻辑文件大小统计；APFS 克隆、压缩或共享块可能使实际可回收空间不同。";
        if !self.result.notes.iter().any(|existing| existing == note) {
            self.result.notes.push(note.into());
        }
        self.result
    }
}

/// 搜索用户缓存中至少 7 天未修改的普通文件和至少 30 天未修改的旧日志。
/// 浏览器、聊天、系统及本应用状态目录会从遍历中排除。
pub fn scan_junk(cancel: &AtomicBool, progress: impl Fn(String)) -> ScanResult {
    scan_junk_incremental(cancel,progress, |_|{})
}
pub fn scan_junk_incremental(cancel: &AtomicBool, progress: impl Fn(String), partial: impl Fn(&ScanResult)) -> ScanResult {
    let mut published=Instant::now();
    let mut state = ScanState::new(cancel);
    let ignored = match crate::safety::load_ignored_paths() {
        Ok(paths) => paths,
        Err(error) => { state.result.notes.push(format!("保护名单不可用，本轮停止垃圾扫描：{error}")); return state.finish(); }
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        state.skipped(Path::new("$HOME"));
        state
            .result
            .notes
            .push("无法确定用户目录，未扫描垃圾项目。".into());
        return state.finish();
    };
    let roots = [
        (
            home.join("Library/Caches"),
            1_u64,
            "cache",
            "用户缓存；普通文件且至少 7 天未修改",
        ),
        (
            home.join("Library/Logs"),
            30_u64,
            "log",
            "旧日志；普通文件且至少 30 天未修改",
        ),
        (home.join(".npm/_npx"),30_u64,"developer_cache","npx过期临时下载缓存；至少30天未修改，需确认并排除使用中任务"),
        (home.join(".npm/_cacache"),7_u64,"developer_cache","npm下载缓存；至少7天未修改，可重新下载"),
        (home.join(".cargo/registry/cache"),7_u64,"developer_cache","Rust下载缓存；至少7天未修改，离线构建可能需要重新下载"),
    ];
    let now = now_seconds();

    for (root, days, kind, detail) in roots {
        if state.stopped || cancelled(&mut state) {
            break;
        }
        progress(format!("扫描 {}…", root.display()));
        let Some(root_meta) = readable_directory(&root, &mut state) else {
            continue;
        };
        let oldest = now.saturating_sub(days.saturating_mul(DAY_SECONDS));
        let iter = WalkDir::new(&root)
            .follow_links(false)
            .same_file_system(true)
            .min_depth(1)
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                if entry.file_type().is_symlink() {
                    return false;
                }
                let path = entry.path();
                if root==home.join(".npm/_npx")&&!crate::safety::npx_package_old(path){return false}
                if !path.starts_with(&root) || protected_junk_path(path, &root) || crate::safety::matches_ignored(path, &ignored) {
                    return false;
                }
                if entry.file_type().is_dir() && is_package(path) {
                    return false;
                }
                true
            });
        for item in iter {
            if state.stopped || !state.visit() {
                break;
            }
            let item = match item {
                Ok(item) => item,
                Err(error) => {
                    if let Some(path) = error.path() {
                        state.skipped(path);
                    } else {
                        state.result.skipped = state.result.skipped.saturating_add(1);
                    }
                    continue;
                }
            };
            if published.elapsed()>=std::time::Duration::from_millis(500) {
                progress(format!("已检查 {} 项，找到 {} 个候选…",state.result.scanned,state.result.entries.len()));
                partial(&state.result);published=Instant::now();
            }
            let path = item.path();
            if !path.starts_with(&root)
                || item.file_type().is_symlink()
                || !item.file_type().is_file()
            {
                continue;
            }
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    metadata
                }
                Ok(_) => continue,
                Err(_) => {
                    state.skipped(path);
                    continue;
                }
            };
            let modified = metadata_mtime(&metadata);
            if modified.map_or(true, |mtime| mtime > oldest) || !crate::safety::junk_metadata_safe(path, &metadata) {
                continue;
            }
            let (entry_kind,entry_detail)=if kind=="cache" && modified.is_some_and(|m|m>now.saturating_sub(7*DAY_SECONDS)){("recent_cache","近期缓存；至少1天未修改，清理后可能需要重新下载，请确认")}else{(kind,detail)};
            if let Some(entry) = make_entry(path, entry_kind, &root, entry_detail) {
                state.append(entry);
            } else {
                state.skipped(path);
            }
        }
        // 避免将目录元数据错误当作扫描成功；已通过 read_dir 探测确保目录可列出。
        let _ = root_meta;
    }
    state.finish()
}

/// 列出 /Applications 与 ~/Applications 的直接 .app 子项，并统计包内普通文件逻辑大小。
pub fn scan_applications(cancel: &AtomicBool, progress: impl Fn(String)) -> ScanResult {
    let mut state = ScanState::new(cancel);
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join("Applications"));
    }
    let running = running_application_paths();
    let own_bundle = current_bundle_path();

    for root in roots {
        if state.stopped || cancelled(&mut state) {
            break;
        }
        progress(format!("检查 {}…", root.display()));
        if readable_directory(&root, &mut state).is_none() {
            continue;
        }
        let children = match fs::read_dir(&root) {
            Ok(children) => children,
            Err(_) => {
                state.skipped(&root);
                continue;
            }
        };
        for child in children {
            if state.stopped || !state.visit() {
                break;
            }
            let child = match child {
                Ok(child) => child,
                Err(_) => {
                    state.result.skipped = state.result.skipped.saturating_add(1);
                    continue;
                }
            };
            let path = child.path();
            if !path.starts_with(&root) || !is_app_bundle(&path) {
                continue;
            }
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => metadata,
                Ok(_) => continue,
                Err(_) => {
                    state.skipped(&path);
                    continue;
                }
            };
            let info = read_app_info(&path);
            let bundle_id = info.bundle_id.unwrap_or_default();
            if bundle_id.starts_with("com.apple.") || own_bundle.as_deref() == Some(path.as_path())
            {
                continue;
            }
            let (size, was_cancelled) = directory_size(&path, metadata.dev(), &mut state, true);
            if was_cancelled || state.stopped {
                break;
            }
            let is_running = running.contains(&path);
            let name = info.display_name.unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            let mut detail = format!("应用包体积（逻辑文件大小）；{}", name);
            if !bundle_id.is_empty() {
                detail.push_str(&format!("；Bundle ID：{bundle_id}"));
            }
            detail.push_str(if path.join("Contents/_MASReceipt/receipt").is_file(){"；[App Store收据]"}else{"；[其他来源]"});
            if is_running {
                detail.push_str("；运行中，清理操作应拒绝处理");
            }
            if let Some(entry) = make_entry_with_size(&path, "application", &root, &detail, size) {
                state.append(entry);
            } else {
                state.skipped(&path);
            }
        }
    }
    state.finish()
}

/// 统计所选目录的直接子项；目录子项的大小为递归普通文件逻辑大小。
pub fn scan_space(root: &Path, cancel: &AtomicBool, progress: impl Fn(String)) -> ScanResult {
    let mut state = ScanState::new(cancel);
    let root = normalized_path(root);
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| normalized_path(&path));
    if home.as_deref() == Some(root.as_path()) {
        state
            .result
            .notes
            .push("为避免扫描整个用户目录，请选择其中一个具体子目录。".into());
        state
            .result
            .notes
            .push("本次仅识别所选根目录的位置，没有递归读取内容。".into());
        return state.finish();
    }
    progress(format!("分析 {}…", root.display()));
    let Some(root_meta) = readable_directory(&root, &mut state) else {
        return state.finish();
    };
    let children = match fs::read_dir(&root) {
        Ok(children) => children,
        Err(_) => {
            state.skipped(&root);
            return state.finish();
        }
    };
    for child in children {
        if state.stopped || !state.visit() {
            break;
        }
        let child = match child {
            Ok(child) => child,
            Err(_) => {
                state.result.skipped = state.result.skipped.saturating_add(1);
                continue;
            }
        };
        let path = child.path();
        if !path.starts_with(&root) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.file_type().is_symlink() => metadata,
            Ok(_) => continue,
            Err(_) => {
                state.skipped(&path);
                continue;
            }
        };
        let (size, is_dir) = if metadata.is_file() {
            (metadata.len(), false)
        } else if metadata.is_dir() {
            if is_package(&path) {
                state.result.skipped = state.result.skipped.saturating_add(1);
                if state.result.notes.len() < 20 {
                    state.result.notes.push(format!(
                        "已跳过 package 内部：{}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
                (0, true)
            } else {
                let (size, was_cancelled) =
                    directory_size(&path, root_meta.dev(), &mut state, false);
                if was_cancelled || state.stopped {
                    break;
                }
                (size, true)
            }
        } else {
            continue;
        };
        let kind = if is_dir { "folder" } else { "file" };
        let detail = if is_dir {
            "所选目录的直接子文件夹；递归逻辑文件大小"
        } else {
            "所选目录的直接普通文件；逻辑文件大小"
        };
        if let Some(entry) = make_entry_with_size(&path, kind, &root, detail, size) {
            state.append(entry);
        } else {
            state.skipped(&path);
        }
    }
    state.finish()
}

/// 查找符合逻辑大小阈值的普通文件；若指定天数，则同时要求 mtime 达到日期阈值。
pub fn scan_large(
    root: &Path,
    min_bytes: u64,
    older_days: Option<u64>,
    cancel: &AtomicBool,
    progress: impl Fn(String),
) -> ScanResult {
    scan_large_incremental(root,min_bytes,older_days,cancel,progress, |_|{})
}
pub fn scan_large_incremental(root:&Path,min_bytes:u64,older_days:Option<u64>,cancel:&AtomicBool,progress:impl Fn(String),partial:impl Fn(&ScanResult))->ScanResult {
    let mut state = ScanState::new(cancel);
    let mut published=Instant::now();
    let root = normalized_path(root);
    progress(format!("查找大型或旧文件：{}…", root.display()));
    if cancel.load(Ordering::Relaxed){state.visit();return state.finish()}
    let Some(root_meta) = readable_directory(&root, &mut state) else {
        return state.finish();
    };
    let threshold =
        older_days.map(|days| now_seconds().saturating_sub(days.saturating_mul(DAY_SECONDS)));
    let mut queue=std::collections::VecDeque::new();
    if let Ok(children)=fs::read_dir(&root){for child in children{
        if cancel.load(Ordering::Relaxed){state.visit();break}
        let child=match child{Ok(child)=>child,Err(_)=>{state.result.skipped=state.result.skipped.saturating_add(1);continue}};
        let path=child.path();
        // Each WalkDir gets its own root device. Validate against the selected
        // volume *before* opening that root, or autofs/mounted children can hang.
        let metadata=match fs::symlink_metadata(&path){Ok(m)=>m,Err(_)=>{state.skipped(&path);continue}};
        if metadata.file_type().is_symlink()||metadata.dev()!=root_meta.dev(){state.skipped(&path);continue}
        if metadata.is_dir()&&is_package(&path){continue}
        if (root==Path::new("/")||root==Path::new("/System/Volumes/Data"))&&child.file_name()=="Volumes"{continue}
        queue.push_back(WalkDir::new(path).follow_links(false).follow_root_links(false).same_file_system(true).into_iter());
    }}
    while let Some(mut iter)=queue.pop_front(){
        let Some(item)=iter.next()else{continue};
        if let Ok(entry)=&item {
            if entry.file_type().is_symlink()||!entry.path().starts_with(&root)||(entry.file_type().is_dir()&&is_package(entry.path())){if entry.file_type().is_dir(){iter.skip_current_dir();}queue.push_back(iter);continue}
        }
        queue.push_back(iter);
        if state.stopped || !state.visit() {
            break;
        }
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                if let Some(path) = error.path() {
                    state.skipped(path);
                } else {
                    state.result.skipped = state.result.skipped.saturating_add(1);
                }
                continue;
            }
        };
        if published.elapsed()>std::time::Duration::from_millis(500){progress(format!("已检查 {} 项，找到 {} 个大型文件…",state.result.scanned,state.result.entries.len()));partial(&state.result);published=Instant::now();}
        let path = item.path();
        if !path.starts_with(&root) || item.file_type().is_symlink() || !item.file_type().is_file()
        {
            continue;
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.dev() == root_meta.dev() =>
            {
                metadata
            }
            Ok(_) => continue,
            Err(_) => {
                state.skipped(path);
                continue;
            }
        };
        if metadata.len() < min_bytes {
            continue;
        }
        let modified = metadata_mtime(&metadata);
        if let Some(threshold) = threshold {
            if modified.map_or(true, |mtime| mtime > threshold) {
                continue;
            }
        }
        let detail = if threshold.is_some() {
            "普通文件；达到大小及修改时间阈值"
        } else {
            "普通文件；逻辑文件大小达到阈值"
        };
        if let Some(entry) = make_entry_with_size(path, "file", &root, detail, metadata.len()) {
            state.append(entry);
        } else {
            state.skipped(path);
        }
    }
    state.finish()
}

/// 创建含路径与文件系统身份信息的结果条目；调用方应在操作前再次核验身份。
pub fn make_entry(path: &Path, kind: &str, scope: &Path, detail: &str) -> Option<Entry> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
        return None;
    }
    Some(entry_from_metadata(
        path,
        kind,
        scope,
        detail,
        &metadata,
        metadata.len(),
    ))
}

fn make_entry_with_size(
    path: &Path,
    kind: &str,
    scope: &Path,
    detail: &str,
    size: u64,
) -> Option<Entry> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
        return None;
    }
    Some(entry_from_metadata(
        path, kind, scope, detail, &metadata, size,
    ))
}

fn entry_from_metadata(
    path: &Path,
    kind: &str,
    scope: &Path,
    detail: &str,
    metadata: &fs::Metadata,
    size: u64,
) -> Entry {
    use std::os::unix::fs::MetadataExt;
    let canonicalish = normalized_path(path);
    Entry {
        id: canonicalish.to_string_lossy().into_owned(),
        name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        path: canonicalish.to_string_lossy().into_owned(),
        size,
        modified: metadata_mtime(metadata),
        kind: kind.into(),
        detail: detail.into(),
        is_dir: metadata.is_dir(),
        scope_root: normalized_path(scope).to_string_lossy().into_owned(),
        inode: metadata.ino(),
        device: metadata.dev(),
        mtime_nsec: metadata.mtime_nsec(),
    }
}

fn readable_directory(path: &Path, state: &mut ScanState<'_>) -> Option<fs::Metadata> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            // 预先打开目录，尽早记录权限失败。
            match fs::read_dir(path) {
                Ok(_) => Some(metadata),
                Err(_) => {
                    state.skipped(path);
                    None
                }
            }
        }
        _ => {
            state.skipped(path);
            None
        }
    }
}

/// 计算目录中的普通文件大小，WalkDir 使用显式栈，避免深路径递归耗尽线程栈。
fn directory_size(
    path: &Path,
    root_device: u64,
    state: &mut ScanState<'_>,
    include_packages: bool,
) -> (u64, bool) {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0_u64;
    let iter = WalkDir::new(path)
        .follow_links(false)
        .same_file_system(true)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_type().is_symlink() || !entry.path().starts_with(path) {
                return false;
            }
            if !include_packages && entry.file_type().is_dir() && is_package(entry.path()) {
                return false;
            }
            true
        });
    for item in iter {
        if !state.visit() {
            return (total, true);
        }
        let item = match item {
            Ok(item) => item,
            Err(error) => {
                if let Some(path) = error.path() {
                    state.skipped(path);
                } else {
                    state.result.skipped = state.result.skipped.saturating_add(1);
                }
                continue;
            }
        };
        let child = item.path();
        if !child.starts_with(path) || item.file_type().is_symlink() {
            continue;
        }
        let metadata = match fs::symlink_metadata(child) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.dev() == root_device => {
                metadata
            }
            Ok(_) => continue,
            Err(_) => {
                state.skipped(child);
                continue;
            }
        };
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    (total, state.stopped)
}

fn is_app_bundle(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
}

fn is_package(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            let extension = extension.to_ascii_lowercase();
            extension == "app"
                || extension == "bundle"
                || extension == "framework"
                || extension == "plugin"
                || extension == "appex"
                || extension == "xpc"
                || extension == "pkg"
                || extension == "workflow"
        })
}

pub(crate) fn protected_junk_path(path: &Path, root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    // 仅在真实 Caches 范围审阅厂商缓存，日志不放宽；敏感子目录始终保留。
    static RULES: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    let rules=RULES.get_or_init(||serde_json::from_str(include_str!("../Assets/cleanup-rules.json")).expect("bundled cleanup rules"));
    if relative.components().any(|part| {let name=part.as_os_str().to_string_lossy().to_lowercase();rules["preserve_components"].as_array().unwrap().iter().any(|v|name.contains(v.as_str().unwrap()))}) {return true}
    let cache_review=root.ends_with("Caches");
    let protected = [
        "safari",
        "chrome",
        "chromium",
        "firefox",
        "edge",
        "browser",
        "webkit",
        "google",
        "mozilla",
        "wechat",
        "weixin",
        "微信",
        "tencent",
        "腾讯",
        "qq",
        "dingtalk",
        "钉钉",
        "lark",
        "feishu",
        "飞书",
        "slack",
        "discord",
        "telegram",
        "whatsapp",
        "com.apple",
        "karuo",
        "卡若",
        "codex",
    ];
    relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy().to_lowercase();
        protected.iter().any(|needle| name.contains(needle) && !(cache_review && rules["cache_vendor_review"].as_array().unwrap().iter().any(|v|v.as_str()==Some(*needle))))
    })
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(unix)]
fn metadata_mtime(metadata: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    u64::try_from(metadata.mtime()).ok()
}

#[cfg(not(unix))]
fn metadata_mtime(metadata: &fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn normalized_path(path: &Path) -> PathBuf {
    // normalize lexical components without resolving symlinks
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn cancelled(state: &mut ScanState<'_>) -> bool {
    if state.cancel.load(Ordering::Relaxed) {
        let _ = state.visit();
        true
    } else {
        false
    }
}

fn read_app_info(path: &Path) -> AppInfo {
    let plist_path = path.join("Contents/Info.plist");
    let Ok(value) = plist::Value::from_file(plist_path) else {
        return AppInfo::default();
    };
    let Some(dict) = value.as_dictionary() else {
        return AppInfo::default();
    };
    let string = |key: &str| {
        dict.get(key)
            .and_then(plist::Value::as_string)
            .map(str::to_owned)
    };
    AppInfo {
        display_name: string("CFBundleDisplayName").or_else(|| string("CFBundleName")),
        bundle_id: string("CFBundleIdentifier"),
    }
}

#[derive(Default)]
struct AppInfo {
    display_name: Option<String>,
    bundle_id: Option<String>,
}

fn current_bundle_path() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    executable
        .ancestors()
        .find(|ancestor| {
            ancestor
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        })
        .map(normalized_path)
}

#[cfg(target_os = "macos")]
fn running_application_paths() -> std::collections::HashSet<PathBuf> {
    use objc2_app_kit::NSWorkspace;
    let mut paths = std::collections::HashSet::new();
    // AppKit 的运行应用列表是线程安全读取接口。
    unsafe {
        let apps = NSWorkspace::sharedWorkspace().runningApplications();
        for app in apps.iter() {
            if let Some(url) = app.bundleURL() {
                if let Some(path) = url.path() {
                    paths.insert(normalized_path(Path::new(&path.to_string())));
                }
            }
        }
    }
    paths
}

#[cfg(not(target_os = "macos"))]
fn running_application_paths() -> std::collections::HashSet<PathBuf> {
    std::collections::HashSet::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[test] fn cache_review_is_precise_and_keeps_sensitive_data() {
        let root=Path::new("/fixture/Library/Caches");
        assert!(!protected_junk_path(&root.join("com.google.Chrome/Default/Cache/data_1"),root));
        assert!(!protected_junk_path(&root.join("com.apple.help/cache.bin"),root));
        for name in ["com.google.Chrome/Default/Cookies", "com.google.Chrome/Local Storage/item", "com.tencent.WeChat/cache.bin", "KaruoCleaner/state"] {assert!(protected_junk_path(&root.join(name),root),"{name}");}
        let logs=Path::new("/fixture/Library/Logs");assert!(protected_junk_path(&logs.join("com.apple/run.log"),logs));
    }
    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> PathBuf {
        let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "karuo-cleaner-engine-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create fixture root");
        path
    }

    fn write_file(path: &Path, bytes: usize) {
        let mut file = File::create(path).expect("create fixture file");
        file.write_all(&vec![b'x'; bytes])
            .expect("write fixture file");
    }

    #[test]
    fn large_file_filter_uses_size_threshold() {
        let root = fixture();
        write_file(&root.join("small.bin"), 3);
        write_file(&root.join("large.bin"), 12);
        let result = scan_large(&root, 8, None, &AtomicBool::new(false), |_| {});
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].name, "large.bin");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn large_scan_does_not_follow_symbolic_links() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let outside = fixture();
        write_file(&root.join("inside.bin"), 10);
        write_file(&outside.join("outside.bin"), 20);
        symlink(outside.join("outside.bin"), root.join("linked.bin")).expect("create symlink");
        let result = scan_large(&root, 1, None, &AtomicBool::new(false), |_| {});
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].name, "inside.bin");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_does_not_prune_remaining_siblings() {
        use std::os::unix::fs::symlink;
        let root=fixture();let dir=root.join("nested");fs::create_dir_all(&dir).unwrap();
        symlink("/tmp",dir.join("00-link")).unwrap();
        write_file(&dir.join("zz-large.bin"),20);
        let result=scan_large(&root,1,None,&AtomicBool::new(false), |_|{});
        assert!(result.entries.iter().any(|e|e.name=="zz-large.bin"));
        let _=fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlink_at_queue_root_is_never_opened_or_traversed() {
        use std::os::unix::fs::symlink;
        let root=fixture();let outside=fixture();
        write_file(&outside.join("must-not-read.bin"),30);
        write_file(&root.join("inside.bin"),10);
        symlink(&outside,root.join("linked-directory")).unwrap();
        let result=scan_large(&root,1,None,&AtomicBool::new(false), |_|{});
        assert_eq!(result.entries.len(),1);
        assert_eq!(result.entries[0].name,"inside.bin");
        assert_eq!(result.scanned,1);
        assert_eq!(result.skipped,1);
        fs::remove_dir_all(root).unwrap();fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn pre_cancelled_scan_returns_cancel_note_without_entries() {
        let root = fixture();
        write_file(&root.join("large.bin"), 10);
        let cancel = AtomicBool::new(true);
        let result = scan_large(&root, 1, None, &cancel, |_| {});
        assert!(result.entries.is_empty());
        assert!(result.notes.iter().any(|note| note.contains("已取消")));
        let _ = fs::remove_dir_all(root);
    }
}
