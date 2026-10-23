use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub path: String,
    pub size: u64,
    pub modified: Option<u64>,
    pub kind: String,
    pub detail: String,
    pub is_dir: bool,
    pub scope_root: String,
    pub inode: u64,
    pub device: u64,
    pub mtime_nsec: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanResult {
    pub entries: Vec<Entry>,
    pub skipped: u64,
    pub scanned: u64,
    pub notes: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrashRecord {
    pub id: String,
    pub date: u64,
    pub original_path: String,
    pub trash_path: String,
    pub size: u64,
    pub restored: bool,
    pub inode: u64,
    pub device: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ActionReport {
    pub records: Vec<TrashRecord>,
    pub failures: Vec<String>,
}
