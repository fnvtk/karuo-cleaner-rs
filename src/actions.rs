//! macOS 文件操作安全层。所有永久删除都限制在本应用历史记录中的单个废纸篓项目。

use crate::model::{ActionReport, Entry, TrashRecord};
use objc2::rc::Retained;
use objc2_foundation::{NSFileManager, NSString, NSURL};
use serde::{Deserialize, Serialize};
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[link(name = "proc")]
unsafe extern "C" {
    fn proc_listallpids(buffer: *mut c_void, buffersize: libc::c_int) -> libc::c_int;
    fn proc_pidpath(pid: libc::c_int, buffer: *mut c_void, buffersize: u32) -> libc::c_int;
}

#[derive(Default, Serialize, Deserialize)]
struct Journal {
    records: Vec<TrashRecord>,
    intents: Vec<Intent>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Intent {
    id: String,
    original_path: String,
    expected_trash_path: String,
    size: u64,
    inode: u64,
    device: u64,
    modified: Option<u64>,
    mtime_nsec: i64,
    date: u64,
}

/// 显式解锁，避免并行创建子进程时继承的文件描述符短暂延长锁寿命。
struct ProcessLock(File);
impl Drop for ProcessLock { fn drop(&mut self) { unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN); } } }

pub struct TrashService {
    allowed_roots: Vec<PathBuf>,
    journal_path: PathBuf,
    lock: Mutex<()>,
}

#[derive(Clone, Copy)]
struct Snapshot {
    inode: u64,
    device: u64,
    size: u64,
    modified: u64,
    mtime_nsec: i64,
    is_dir: bool,
}

impl TrashService {
    /// allowed_roots 必须由后台真实扫描逻辑提供；前端传来的 scope_root 不参与授权。
    pub fn new(allowed_roots: Vec<PathBuf>, journal_path: PathBuf) -> Self {
        let canonical_roots = allowed_roots
            .into_iter()
            .filter_map(|path| fs::canonicalize(path).ok())
            .collect();
        Self {
            allowed_roots: canonical_roots,
            journal_path,
            lock: Mutex::new(()),
        }
    }

    /// 移入系统废纸篓。移动前写入意图日志，移动后保存系统返回的真实 Trash 路径。
    pub fn move_entries(&self, entries: &[Entry]) -> ActionReport {
        let _guard = self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut report = ActionReport::default();
        let _process_lock = match self.process_lock() {
            Ok(lock) => lock,
            Err(error) => { report.failures.push(error); return report; }
        };
        if entries.is_empty() {
            return report;
        }

        if let Some(pair) = overlapping_entries(entries) {
            report.failures.push(format!(
                "选择中存在祖先与后代路径，整批已拒绝：{} 与 {}",
                pair.0.display(),
                pair.1.display()
            ));
            return report;
        }

        // 先逐项核验，再一次性持久化整批恢复意图。旧实现每移动一个文件就
        // 读写并 fsync 整份历史；大批量时会退化为 O(n²) 的磁盘写入。
        // 所有意图先落盘后才移动，进程中断仍可按 inode/device 从废纸篓找回。
        let mut journal = match self.read_journal() {
            Ok(value) => value,
            Err(error) => {
                report.failures.push(format!("历史记录不可读，未移动文件：{error}"));
                return report;
            }
        };
        let mut prepared = Vec::with_capacity(entries.len());
        let mut intents = Vec::with_capacity(entries.len());
        for entry in entries {
            let source = PathBuf::from(&entry.path);
            let snapshot = match self.validate_entry(entry, &source) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    report.failures.push(format!("{}：{}", entry.path, error));
                    continue;
                }
            };

            let id = unique_id(snapshot.inode, snapshot.device);
            let now = unix_now();
            let intent = Intent {
                id: id.clone(),
                original_path: source.to_string_lossy().into_owned(),
                expected_trash_path: String::new(),
                size: entry.size,
                inode: snapshot.inode,
                device: snapshot.device,
                modified: entry.modified,
                mtime_nsec: entry.mtime_nsec,
                date: now,
            };
            intents.push(intent.clone());
            prepared.push((entry, source, snapshot, intent));
        }
        if !intents.is_empty() {
            journal.intents.extend(intents);
            if let Err(error) = self.write_journal(&journal) {
                report.failures.push(format!("无法先写入整批恢复意图，未移动文件：{error}"));
                return report;
            }
        }

        for (entry, source, snapshot, intent) in prepared {
            // journal I/O 期间文件可能发生变化，因此移动前再比对扫描身份。
            if let Err(error) = self.validate_entry(entry, &source) {
                journal.intents.retain(|item| item.id != intent.id);
                report.failures.push(format!(
                    "意图日志写入期间文件变化，已拒绝移动 {}：{}",
                    entry.path,
                    error
                ));
                continue;
            }

            let moved_path = match native_trash(&source) {
                Ok(path) => path,
                Err(error) => {
                    report.failures.push(format!("移入系统废纸篓失败 {}：{}；恢复意图已保留", entry.path, error));
                    continue;
                }
            };
            let moved_snapshot = match fs::symlink_metadata(&moved_path) {
                Ok(metadata) => snapshot_from_metadata(&metadata),
                Err(error) => {
                    report.failures.push(format!("已移入废纸篓但无法核对返回路径 {}：{}；意图已保留", entry.path, error));
                    continue;
                }
            };
            if moved_snapshot.inode != snapshot.inode || moved_snapshot.device != snapshot.device {
                report.failures.push(format!("已移动项目身份与扫描快照不符 {}；恢复意图已保留", entry.path));
                continue;
            }

            let record = TrashRecord {
                id: intent.id.clone(),
                date: intent.date,
                original_path: source.to_string_lossy().into_owned(),
                trash_path: moved_path.to_string_lossy().into_owned(),
                size: entry.size,
                restored: false,
                inode: moved_snapshot.inode,
                device: moved_snapshot.device,
            };
            journal.records.retain(|old| old.id != record.id);
            journal.records.push(record.clone());
            journal.intents.retain(|item| item.id != intent.id);
            report.records.push(record);
        }
        if let Err(error) = self.write_journal(&journal) {
            report.failures.push(format!(
                "文件已处理，但批次历史更新失败；移动前保存的恢复意图仍保留，重新打开历史可尝试找回：{error}"
            ));
        }
        report
    }

    /// 读取本应用记录，并尝试用未完成意图的 inode/device 找回实际 Trash 路径。
    pub fn history(&self) -> Vec<TrashRecord> {
        let _guard = self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // history 也会回收未完成意图，必须与后台任务互斥。
        // 后台正在处理时读回最近一次原子保存的快照；不尝试修复意图，也不假报空历史。
        let _process_lock = match self.process_lock() {
            Ok(lock) => lock,
            Err(_) => return self.read_journal().map(|journal| journal.records).unwrap_or_default(),
        };
        let mut journal = match self.read_journal() {
            Ok(value) => value,
            Err(_) => return Vec::new(),
        };
        let pending = journal.intents.clone();
        let mut changed = false;
        for intent in pending {
            let original = PathBuf::from(&intent.original_path);
            // 批次预写后尚未移动的项目仍在原处，移除无用意图，避免下次查历史时
            // 对每个未处理项目都遍历系统废纸篓。
            if fs::symlink_metadata(&original).is_ok_and(|metadata| {
                let current = snapshot_from_metadata(&metadata);
                current.inode == intent.inode && current.device == intent.device
            }) {
                journal.intents.retain(|old| old.id != intent.id);
                changed = true;
                continue;
            }
            if let Some(found) = self.find_in_trash(&original, intent.inode, intent.device) {
                let record = TrashRecord {
                    id: intent.id.clone(),
                    date: intent.date,
                    original_path: intent.original_path.clone(),
                    trash_path: found.to_string_lossy().into_owned(),
                    size: intent.size,
                    restored: false,
                    inode: intent.inode,
                    device: intent.device,
                };
                if !journal.records.iter().any(|old| old.id == record.id) {
                    journal.records.push(record);
                    journal.intents.retain(|old| old.id != intent.id);
                    changed = true;
                }
            }
        }
        if changed {
            // 回收意图映射失败时不掩盖记录；后续 restore/purge 仍需再次验证权威日志。
            let _ = self.write_journal(&journal);
        }
        journal.records
    }

    /// 只还原本应用日志中的对象；目标已存在时不会覆盖。
    pub fn restore(&self, records: &[TrashRecord]) -> Vec<String> {
        let _guard = self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _process_lock = match self.process_lock() { Ok(lock) => lock, Err(error) => return vec![error] };
        let mut errors = Vec::new();
        let mut journal = match self.read_journal() {
            Ok(value) => value,
            Err(error) => return vec![format!("历史记录不可读，未还原：{error}")],
        };

        for requested in records {
            let Some(index) = journal.records.iter().position(|saved| same_record(saved, requested) && !saved.restored) else {
                errors.push(format!("不是本应用有效历史记录或记录已还原：{}", requested.original_path));
                continue;
            };
            let saved = journal.records[index].clone();
            let source = PathBuf::from(&saved.trash_path);
            let destination = PathBuf::from(&saved.original_path);
            if !self.valid_trash_object(&source, &destination, &saved) {
                errors.push(format!("Trash 位置或 inode/device 不匹配，拒绝还原：{}", saved.original_path));
                continue;
            }
            let parent = destination.parent().unwrap_or_else(|| Path::new("/"));
            if !self.allowed_parent(parent) || !no_symlink_components(parent, true) {
                errors.push(format!("还原父目录已不在允许范围或含符号链接：{}", parent.display()));
                continue;
            }
            if destination.exists() || fs::symlink_metadata(&destination).is_ok() {
                errors.push(format!("还原目标已存在，拒绝覆盖：{}", destination.display()));
                continue;
            }
            match native_move(&source, &destination) {
                Ok(()) => {
                    journal.records[index].restored = true;
                    if let Err(error) = self.write_journal(&journal) {
                        errors.push(format!("已还原 {}，但历史状态写入失败：{}", destination.display(), error));
                    }
                }
                Err(error) => errors.push(format!("还原失败 {}：{}", destination.display(), error)),
            }
        }
        errors
    }

    /// 只清理经再次确认的本应用记录项目，绝不清空废纸篓。
    pub fn purge(&self, records: &[TrashRecord]) -> Vec<String> {
        let _guard = self.lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _process_lock = match self.process_lock() { Ok(lock) => lock, Err(error) => return vec![error] };
        let mut errors = Vec::new();
        let mut journal = match self.read_journal() {
            Ok(value) => value,
            Err(error) => return vec![format!("历史记录不可读，未清理：{error}")],
        };

        for requested in records {
            let Some(index) = journal.records.iter().position(|saved| same_record(saved, requested) && !saved.restored) else {
                errors.push(format!("不是本应用有效历史记录：{}", requested.original_path));
                continue;
            };
            let saved = journal.records[index].clone();
            let source = PathBuf::from(&saved.trash_path);
            let original = PathBuf::from(&saved.original_path);
            if !self.valid_trash_object(&source, &original, &saved) {
                errors.push(format!("Trash 位置或 inode/device 不匹配，拒绝永久清理：{}", saved.original_path));
                continue;
            }
            // 身份和真实 Trash 位置再检查一次，随后只删除这一条目。
            if !self.valid_trash_object(&source, &original, &saved) {
                errors.push(format!("永久清理前身份再次校验失败：{}", saved.original_path));
                continue;
            }
            match native_remove(&source) {
                Ok(()) => {
                    journal.records.remove(index);
                    if let Err(error) = self.write_journal(&journal) {
                        errors.push(format!("已永久清理 {}，但历史记录更新失败：{}", source.display(), error));
                    }
                }
                Err(error) => errors.push(format!("永久清理失败 {}：{}", source.display(), error)),
            }
        }
        errors
    }

    fn validate_entry(&self, entry: &Entry, source: &Path) -> Result<Snapshot, String> {
        let ignored = crate::safety::load_ignored_paths()?;
        if crate::safety::matches_ignored(source, &ignored) {
            return Err("此路径位于用户保护名单，已拒绝清理".into());
        }
        if source.as_os_str().is_empty() || source.to_string_lossy() != entry.path {
            return Err("路径格式无效".into());
        }
        if !no_symlink_components(source, true) {
            return Err("路径不存在或包含符号链接".into());
        }
        let canonical = fs::canonicalize(source).map_err(|error| error.to_string())?;
        if canonical != source {
            return Err("路径 canonical 化后发生变化".into());
        }
        if !self.allowed_roots.iter().any(|root| strict_child(&canonical, root)) {
            return Err("路径不在后台批准的扫描根目录内，或选中了根目录本身".into());
        }
        if entry.kind=="app_residue" { crate::uninstall::validate_residue(entry)?; }
        if protected_root(&canonical) || (crate::safety::is_protected(&canonical) && entry.kind!="app_residue") || self.is_self_app(&canonical) || apple_app(&canonical) || running_app(&canonical) {
            return Err("系统根目录、个人根目录、Apple 应用、本应用或运行中应用受保护".into());
        }
        let metadata = fs::symlink_metadata(source).map_err(|error| error.to_string())?;
        // 清理类型是后台扫描产生的能力边界，不能把目录或空间图当作可删除文件。
        match entry.kind.as_str() {
            "app_residue" => crate::uninstall::validate_residue(entry)?,
            "cache" | "log" => crate::safety::validate_junk_entry(entry)?,
            "file" | "large" | "old" => {
                if !crate::safety::junk_metadata_safe(source, &metadata) {
                    return Err("受保护、数据库、云端占位、硬链接或非普通文件，已跳过".into());
                }
            }
            "application" if metadata.is_dir() && source.extension().is_some_and(|e| e == "app")
                && source.parent().is_some_and(|p| p == Path::new("/Applications") || std::env::var_os("HOME").is_some_and(|h| p == PathBuf::from(h).join("Applications"))) => {}
            _ => return Err("不支持的清理类型或应用目录不在卸载白名单".into()),
        }
        if !metadata.is_dir() { crate::safety::ensure_not_open(source)?; }
        // 占用检查之后重读，避免慢检查掩盖文件更新。
        let metadata = fs::symlink_metadata(source).map_err(|e| e.to_string())?;
        let current = snapshot_from_metadata(&metadata);
        if current.inode != entry.inode || current.device != entry.device {
            return Err("扫描后 inode/device 已变化".into());
        }
        if !entry.is_dir && current.size != entry.size {
            return Err("扫描后文件大小已变化".into());
        }
        if current.modified != entry.modified.unwrap_or(0) || current.mtime_nsec != entry.mtime_nsec {
            return Err("扫描后修改时间已变化".into());
        }
        if current.is_dir != entry.is_dir {
            return Err("文件类型已变化".into());
        }
        Ok(current)
    }

    fn allowed_parent(&self, parent: &Path) -> bool {
        let Ok(canonical) = fs::canonicalize(parent) else { return false };
        // 允许恢复到用户已批准的根目录内，例如 /Applications 下的直接 .app；
        // 保护的是根本身不被移动，不应阻止把其子项恢复回来。
        self.allowed_roots.iter().any(|root| canonical == *root || strict_child(&canonical, root))
    }

    fn is_self_app(&self, path: &Path) -> bool {
        self_app(path).is_some()
    }

    fn valid_trash_object(&self, trash_path: &Path, original: &Path, record: &TrashRecord) -> bool {
        if !no_symlink_components(trash_path, true) || !no_symlink_components(original, false) {
            return false;
        }
        let Ok(canonical) = fs::canonicalize(trash_path) else { return false };
        if canonical != trash_path || !is_real_trash_path(&canonical, original, record.device) {
            return false;
        }
        let Ok(metadata) = fs::symlink_metadata(trash_path) else { return false };
        let current = snapshot_from_metadata(&metadata);
        current.inode == record.inode && current.device == record.device
    }

    /// 同一日志的 GUI/定时进程共享固定 sidecar；返回 File 的生命周期持有 flock。
    fn process_lock(&self) -> Result<ProcessLock, String> {
        let parent = self.journal_path.parent().ok_or("历史路径没有父目录")?;
        if !no_symlink_components(parent, false) { return Err("历史父目录包含符号链接，已停止操作".into()); }
        fs::create_dir_all(parent).map_err(|e| format!("无法创建历史目录：{e}"))?;
        if !no_symlink_components(parent, true) || !no_symlink_components(&self.journal_path, false) {
            return Err("历史路径包含符号链接，已停止操作".into());
        }
        let mut name = self.journal_path.as_os_str().to_os_string(); name.push(".lock");
        let lock_path = PathBuf::from(name);
        let file = OpenOptions::new().read(true).write(true).create(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&lock_path)
            .map_err(|e| format!("无法打开清理互斥锁，已停止操作：{e}"))?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.nlink() != 1 || meta.uid() != unsafe { libc::geteuid() } {
            return Err("清理互斥锁身份不安全，已停止操作".into());
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("其他清理进程正在操作或无法取得互斥锁，本轮已跳过".into());
        }
        Ok(ProcessLock(file))
    }

    fn read_journal(&self) -> Result<Journal, String> {
        let mut file = match File::open(&self.journal_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Journal::default()),
            Err(error) => return Err(error.to_string()),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|error| error.to_string())?;
        serde_json::from_slice(&bytes).map_err(|error| format!("JSON 历史损坏：{error}"))
    }

    fn write_journal(&self, journal: &Journal) -> Result<(), String> {
        let parent = self.journal_path.parent().ok_or("历史路径没有父目录")?;
        fs::create_dir_all(parent).map_err(|error| format!("创建历史目录失败：{error}"))?;
        let mut temp = self.journal_path.clone();
        let suffix = unique_id(0, 0);
        temp.set_extension(format!("json.{suffix}.tmp"));
        let bytes = serde_json::to_vec_pretty(journal).map_err(|error| error.to_string())?;
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)
            .map_err(|error| format!("创建临时历史文件失败：{error}"))?;
        if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&temp);
            return Err(format!("写入或同步临时历史文件失败：{error}"));
        }
        drop(file);
        fs::rename(&temp, &self.journal_path).map_err(|error| {
            let _ = fs::remove_file(&temp);
            format!("原子替换历史文件失败：{error}")
        })?;
        File::open(parent).and_then(|directory| directory.sync_all())
            .map_err(|error| format!("历史文件已替换，但同步目录失败：{error}"))
    }

    fn find_in_trash(&self, original: &Path, inode: u64, device: u64) -> Option<PathBuf> {
        for root in trash_roots_for(original, device)? {
            let mut pending = vec![root];
            while let Some(directory) = pending.pop() {
                let Ok(entries) = fs::read_dir(&directory) else { continue };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Ok(metadata) = fs::symlink_metadata(&path) else { continue };
                    if metadata.file_type().is_symlink() { continue; }
                    let snapshot = snapshot_from_metadata(&metadata);
                    if snapshot.inode == inode && snapshot.device == device { return Some(path); }
                    if metadata.is_dir() { pending.push(path); }
                }
            }
        }
        None
    }
}

fn native_trash(path: &Path) -> Result<PathBuf, String> {
    let manager = NSFileManager::defaultManager();
    let source = nsurl(path)?;
    let mut resulting: Option<Retained<NSURL>> = None;
    manager.trashItemAtURL_resultingItemURL_error(&source, Some(&mut resulting))
        .map_err(|error| error.localizedDescription().to_string())?;
    let url = resulting.ok_or("系统废纸篓操作未返回实际项目 URL")?;
    url.path().map(|value| PathBuf::from(value.to_string())).ok_or_else(|| "系统返回的废纸篓 URL 没有路径".into())
}

fn native_move(source: &Path, destination: &Path) -> Result<(), String> {
    let manager = NSFileManager::defaultManager();
    let source = nsurl(source)?;
    let destination = nsurl(destination)?;
    manager.moveItemAtURL_toURL_error(&source, &destination)
        .map_err(|error| error.localizedDescription().to_string())
}

fn native_remove(path: &Path) -> Result<(), String> {
    let manager = NSFileManager::defaultManager();
    let path = nsurl(path)?;
    manager.removeItemAtURL_error(&path)
        .map_err(|error| error.localizedDescription().to_string())
}

fn nsurl(path: &Path) -> Result<Retained<NSURL>, String> {
    let text = path.to_str().ok_or("路径不是有效 UTF-8")?;
    Ok(NSURL::fileURLWithPath(&NSString::from_str(text)))
}

fn snapshot_from_metadata(metadata: &fs::Metadata) -> Snapshot {
    Snapshot {
        inode: metadata.ino(),
        device: metadata.dev(),
        size: metadata.size(),
        modified: metadata.mtime().max(0) as u64,
        mtime_nsec: metadata.mtime_nsec(),
        is_dir: metadata.is_dir(),
    }
}

fn no_symlink_components(path: &Path, require_exists: bool) -> bool {
    if !path.is_absolute() { return false; }
    let mut cursor = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(part) => cursor.push(part),
            Component::ParentDir | Component::Prefix(_) => return false,
        }
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => return false,
            Ok(_) => {}
            Err(error) if !require_exists && error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => return false,
        }
    }
    true
}

fn strict_child(path: &Path, root: &Path) -> bool {
    path != root && path.starts_with(root)
}

fn protected_root(path: &Path) -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut protected = vec![PathBuf::from("/"), PathBuf::from("/System"), PathBuf::from("/Library"),
                             PathBuf::from("/Applications"), PathBuf::from("/Users"), home.clone()];
    for name in ["Library", "Documents", "Downloads", "Desktop", "Pictures", "Movies", "Music"] {
        protected.push(home.join(name));
    }
    protected.iter().any(|item| path == item)
}

fn overlapping_entries(entries: &[Entry]) -> Option<(PathBuf, PathBuf)> {
    let mut paths: Vec<PathBuf> = entries.iter().map(|entry| PathBuf::from(&entry.path)).collect();
    paths.sort();
    paths.dedup();
    for (index, path) in paths.iter().enumerate() {
        if let Some(child) = paths.iter().skip(index + 1).find(|candidate| strict_child(candidate, path)) {
            return Some((path.clone(), child.clone()));
        }
    }
    None
}

fn app_bundle(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|ancestor| ancestor.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
}

fn bundle_id(app: &Path) -> Option<String> {
    let plist = app.join("Contents/Info.plist");
    let value = plist::Value::from_file(plist).ok()?;
    value.as_dictionary()?.get("CFBundleIdentifier")?.as_string().map(str::to_owned)
}

fn apple_app(path: &Path) -> bool {
    if let Some(bundle) = app_bundle(path) {
        if bundle_id(&bundle).is_some_and(|id| id == "com.apple" || id.starts_with("com.apple.")) {
            return true;
        }
        let text = bundle.to_string_lossy();
        if text.starts_with("/System/Applications/") || text.starts_with("/System/Library/") {
            return true;
        }
    }
    false
}

fn self_app(path: &Path) -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    app_bundle(&executable).filter(|bundle| path == bundle || path.starts_with(bundle))
}

fn running_app(path: &Path) -> bool {
    let mut pids = vec![0i32; 16_384];
    let byte_len = (pids.len() * std::mem::size_of::<i32>()) as libc::c_int;
    let count = unsafe { proc_listallpids(pids.as_mut_ptr().cast(), byte_len) };
    if count <= 0 { return false; }
    pids.truncate((count as usize).min(pids.len()));
    let self_pid = unsafe { libc::getpid() };
    let mut buffer = vec![0u8; 4096];
    for pid in pids.into_iter().filter(|pid| *pid > 0 && *pid != self_pid) {
        let length = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        if length <= 0 { continue; }
        let bytes = &buffer[..(length as usize).min(buffer.len())];
        let executable = PathBuf::from(String::from_utf8_lossy(bytes).as_ref());
        if let Some(bundle) = app_bundle(&executable) {
            if path == bundle || path.starts_with(&bundle) { return true; }
        }
    }
    false
}

fn trash_roots_for(original: &Path, device: u64) -> Option<Vec<PathBuf>> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let home_trash = home.join(".Trash");
        if fs::metadata(&home_trash).is_ok_and(|meta| meta.dev() == device) { roots.push(home_trash); }
    }
    if let Some(volume) = volume_root(original, device) {
        roots.push(volume.join(".Trashes").join(unsafe { libc::getuid() }.to_string()));
    }
    roots.sort();
    roots.dedup();
    Some(roots)
}

fn volume_root(path: &Path, device: u64) -> Option<PathBuf> {
    let mut existing = path.to_path_buf();
    while fs::metadata(&existing).map(|meta| meta.dev() != device).unwrap_or(true) {
        existing = existing.parent()?.to_path_buf();
    }
    let mut current = fs::canonicalize(existing).ok()?;
    loop {
        let parent = current.parent()?.to_path_buf();
        if fs::metadata(&parent).ok()?.dev() != device { return Some(current); }
        if parent == current { return Some(current); }
        current = parent;
    }
}

fn is_real_trash_path(path: &Path, original: &Path, device: u64) -> bool {
    trash_roots_for(original, device).is_some_and(|roots| roots.iter().any(|root| strict_child(path, root)))
}

fn same_record(left: &TrashRecord, right: &TrashRecord) -> bool {
    left.id == right.id && left.original_path == right.original_path && left.trash_path == right.trash_path
        && left.size == right.size && left.inode == right.inode && left.device == right.device
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn unique_id(inode: u64, device: u64) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let pid = unsafe { libc::getpid() };
    format!("{nanos:x}-{pid:x}-{device:x}-{inode:x}")
}

#[cfg(test)]
mod safety_tests {
    use super::*;
    static FIXTURE_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn fixture() -> PathBuf {
        let root = PathBuf::from(std::env::var_os("HOME").unwrap()).join(format!(
            ".karuo-action-test-{}-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(), FIXTURE_SERIAL.fetch_add(1,std::sync::atomic::Ordering::Relaxed)));
        fs::create_dir_all(&root).unwrap();root
    }
    #[test]
    fn journal_lock_excludes_other_service_and_releases_on_drop() {
        let root=fixture();let journal=root.join("history.json");
        let first=TrashService::new(vec![root.clone()],journal.clone());
        let second=TrashService::new(vec![root.clone()],journal);
        let lock=first.process_lock().unwrap();
        assert!(second.process_lock().is_err());
        assert!(!second.move_entries(&[]).failures.is_empty());
        drop(lock);
        assert!(second.process_lock().is_ok());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn journal_lock_rejects_symlink_sidecar() {
        use std::os::unix::fs::symlink;
        let root=fixture();let target=root.join("ordinary.txt");fs::write(&target,b"keep").unwrap();
        symlink(&target,root.join("history.json.lock")).unwrap();
        let service=TrashService::new(vec![root.clone()],root.join("history.json"));
        assert!(service.process_lock().is_err());
        assert_eq!(fs::read(&target).unwrap(),b"keep");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn changed_file_snapshot_is_rejected_without_moving() {
        let root=fixture();let path=root.join("ordinary.txt");fs::write(&path,b"before").unwrap();
        let service=TrashService::new(vec![root.clone()],root.join("history.json"));
        let entry=crate::engine::make_entry(&path,"file",&root,"自建安全夹具").unwrap();
        fs::write(&path,b"after-size-change").unwrap();
        assert!(service.validate_entry(&entry,&path).is_err());
        assert_eq!(fs::read(&path).unwrap(),b"after-size-change");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn batch_move_persists_and_restores_every_fixture_item() {
        let root = fixture();
        let files: Vec<_> = (0..8).map(|n| {
            let path = root.join(format!("batch-{n}.txt"));
            fs::write(&path, format!("fixture {n}")).unwrap();
            crate::engine::make_entry(&path, "file", &root, "批量恢复测试").unwrap()
        }).collect();
        let service = TrashService::new(vec![root.clone()], root.join("history.json"));
        let moved = service.move_entries(&files);
        assert_eq!(moved.records.len(), files.len(), "{:?}", moved.failures);
        assert!(moved.failures.is_empty(), "{:?}", moved.failures);
        assert_eq!(service.history().iter().filter(|r| !r.restored).count(), files.len());
        assert!(service.restore(&moved.records).is_empty());
        for entry in &files {
            assert_eq!(fs::read_to_string(&entry.path).unwrap(), format!("fixture {}", entry.name.trim_start_matches("batch-").trim_end_matches(".txt")));
        }
        fs::remove_dir_all(root).unwrap();
    }
}

pub(crate) fn ensure_uninstall_app_closed(path:&Path)->Result<(),String>{if running_app(path){Err("应用仍在运行，请正常退出后再卸载".into())}else{Ok(())}}
