use crate::{actions::TrashService, engine};
use std::{fs, path::PathBuf, sync::atomic::AtomicBool, time::{SystemTime, UNIX_EPOCH}};

/// 端到端夹具只使用本程序刚创建的文件，不清理任何已有用户资料。
pub fn run() -> i32 {
    let mut fixture = PathBuf::from(std::env::var_os("HOME").unwrap()).join("Library/Caches");
    fixture.push(format!("KaruoCleanerSelfTest-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
    let mut passed = 0;
    let run = || -> Result<usize, String> {
        let files = fixture.join("files");
        fs::create_dir_all(&files).map_err(|e| e.to_string())?;
        let files = fs::canonicalize(files).map_err(|e| e.to_string())?;
        let old = files.join("fixture.bin");
        let changed = files.join("changed.bin");
        let outside = fixture.join("outside-important.txt");
        fs::write(&old, vec![17u8; 2_000_000]).map_err(|e| e.to_string())?;
        fs::write(&changed, vec![29u8; 1_000_000]).map_err(|e| e.to_string())?;
        fs::write(&outside, b"preserve-this-fixture").map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(&outside, files.join("outside-link")).map_err(|e| e.to_string())?;
        let mut count = 0;
        let mut check = |ok: bool, label: &str| -> Result<(), String> {
            if !ok { return Err(label.to_owned()); }
            count += 1; println!("PASS {label}"); Ok(())
        };
        let result = engine::scan_large(&files, 1_000_000, None, &AtomicBool::new(false), |_| {});
        check(result.entries.len() == 2, "Rust 扫描真实大型文件并排除符号链接")?;
        let usage = engine::scan_space(&files, &AtomicBool::new(false), |_| {});
        check(usage.entries.iter().map(|e| e.size).sum::<u64>() == 3_000_000, "空间透镜统计与夹具大小一致")?;
        let service = TrashService::new(vec![files.clone()], fixture.join("journal/history.json"));
        let entry = engine::make_entry(&old, "file", &files, "自建测试文件").ok_or("无法读取夹具")?;
        let moved = service.move_entries(&[entry]);
        check(moved.records.len() == 1 && moved.failures.is_empty(), &format!("真实系统废纸篓移动：{:?}", moved.failures))?;
        let record = moved.records[0].clone();
        check(!old.exists() && PathBuf::from(&record.trash_path).exists(), "移动结果与恢复记录一致")?;
        let errors = service.restore(&[record]);
        check(errors.is_empty() && old.exists(), &format!("从废纸篓恢复：{errors:?}"))?;
        check(fs::read(&old).map_err(|e| e.to_string())? == vec![17u8; 2_000_000], "恢复文件内容完全一致")?;
        let out = engine::make_entry(&outside, "file", &fixture, "越界夹具").ok_or("无法读取越界夹具")?;
        check(service.move_entries(&[out]).records.is_empty() && outside.exists(), "拒绝越出已批准目录")?;
        let stale = engine::make_entry(&changed, "file", &files, "变化夹具").ok_or("无法读取夹具")?;
        fs::write(&changed, b"changed-after-scan").map_err(|e| e.to_string())?;
        check(service.move_entries(&[stale]).records.is_empty() && changed.exists(), "拒绝使用已变化的扫描快照")?;
        let entry = engine::make_entry(&old, "file", &files, "自建永久清理夹具").ok_or("读取失败")?;
        let moved = service.move_entries(&[entry]);
        check(moved.records.len() == 1, "永久清理前重新移动自建夹具")?;
        let target = PathBuf::from(&moved.records[0].trash_path);
        let errors = service.purge(&moved.records);
        check(errors.is_empty() && !target.exists(), &format!("仅永久删除应用指定记录：{errors:?}"))?;
        check(fs::read_to_string(&outside).map_err(|e| e.to_string())? == "preserve-this-fixture", "其他文件保持原样")?;
        check(service.history().iter().filter(|r| !r.restored).count() == 0, "清理历史与文件结果一致")?;
        // 验证卸载器使用目录逻辑大小时不会与目录 stat 大小混淆。
        let app = files.join("Fixture.app");
        fs::create_dir_all(app.join("Contents")).map_err(|e| e.to_string())?;
        fs::write(app.join("Contents/test.bin"), b"own-app-fixture").map_err(|e| e.to_string())?;
        let mut entry = engine::make_entry(&app, "application", &files, "自建应用包").ok_or("应用夹具读取失败")?;
        entry.size = 123_456;
        let moved = service.move_entries(&[entry]);
        check(moved.records.is_empty() && !moved.failures.is_empty() && app.exists(), "拒绝卸载非Applications目录中的应用包")?;
        check(fs::read(app.join("Contents/test.bin")).map_err(|e|e.to_string())? == b"own-app-fixture", "拒绝操作后自建应用内容保持原样")?;
        let applications=PathBuf::from(std::env::var_os("HOME").unwrap()).join("Applications");
        fs::create_dir_all(&applications).map_err(|e|e.to_string())?;
        let allowed_app=applications.join(format!("KaruoSafetyFixture-{}.app",SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir( &allowed_app).map_err(|e|e.to_string())?;
        fs::create_dir(allowed_app.join("Contents")).map_err(|e|e.to_string())?;
        fs::write(allowed_app.join("Contents/fixture.bin"),b"own-fixture-app").map_err(|e|e.to_string())?;
        let app_service=TrashService::new(vec![applications.clone()],fixture.join("app-journal/history.json"));
        let app_entry=engine::make_entry(&allowed_app,"application",&applications,"自建白名单应用夹具").ok_or("应用读取失败")?;
        let app_moved=app_service.move_entries(&[app_entry]);
        check(app_moved.records.len()==1,&format!("白名单Applications目录内自建应用包可移动：{:?}",app_moved.failures))?;
        check(app_service.restore(&app_moved.records).is_empty()&&allowed_app.exists(),"白名单自建应用包可恢复")?;
        fs::remove_dir_all(allowed_app).map_err(|e|e.to_string())?;
        fs::remove_dir_all(&fixture).map_err(|e| e.to_string())?;
        Ok(count)
    };
    match run() {
        Ok(count) => { passed += count; println!("ALL {passed} RUST CHECKS PASSED"); 0 },
        Err(error) => { eprintln!("FAIL {error}\n夹具保留于 {}", fixture.display()); 1 },
    }
}
