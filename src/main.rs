mod actions;
mod read_worker;
mod engine;
mod model;
mod selftest;
mod space;
mod safety;
mod schedule;
mod guardian;
mod sessions;
mod analysis;
mod startup;
mod control;
mod uninstall;
mod cleanup;

use actions::TrashService;
use model::{Entry, ScanResult, TrashRecord};
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication, NSWorkspace};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap, ffi::CString, fs, path::{Path, PathBuf},
    sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc, Mutex}, thread,
};
use tao::{
    dpi::LogicalSize, event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy}, window::WindowBuilder,
};
use wry::WebViewBuilder;

#[derive(Clone)]
enum UiEvent { Request(Value), Output(Value) }

/// 前端仅传候选编号；授权根目录与文件身份保留在 Rust 端，不能由页面伪造。
struct State {
    volumes: Vec<space::Volume>,
    space_cache: Arc<Mutex<space::Cache>>,
    approved: HashMap<String, Entry>,
    roots: HashMap<String, PathBuf>,
    allowed: Vec<PathBuf>,
    sessions: sessions::Sessions,
    last_moved: Vec<TrashRecord>,
}

struct BusyReset(Arc<AtomicBool>);
impl Drop for BusyReset {
    fn drop(&mut self) { self.0.store(false, Ordering::SeqCst); }
}

#[derive(Serialize)]
struct RunningApp { pid: i32, name: String, bundle_id: String, is_active: bool }

fn home() -> PathBuf { PathBuf::from(std::env::var_os("HOME").unwrap_or_default()) }
fn journal() -> PathBuf { home().join("Library/Application Support/KaruoCleaner/history-rust.json") }
fn saved_roots() -> PathBuf { home().join("Library/Application Support/KaruoCleaner/selected-roots.json") }
fn valid_picked_root(root: &Path) -> bool {
    root.is_absolute() && root.is_dir()
}
fn emit(proxy: &EventLoopProxy<UiEvent>, value: Value) { let _ = proxy.send_event(UiEvent::Output(value)); }
fn message(proxy: &EventLoopProxy<UiEvent>, text: impl Into<String>, error: bool) {
    emit(proxy, json!({"event":"message","title":if error {"未完成的操作"} else {"操作结果"},"text":text.into(),"error":error}));
}
fn disk() -> Value {
    let Ok(path) = CString::new(home().to_string_lossy().as_bytes()) else { return json!({"total":0,"available":0}); };
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut v) } != 0 { return json!({"total":0,"available":0}); }
    json!({"total":(v.f_blocks as u64).saturating_mul(v.f_frsize as u64),"available":(v.f_bavail as u64).saturating_mul(v.f_frsize as u64)})
}
fn ready(proxy: &EventLoopProxy<UiEvent>) { emit(proxy, json!({"event":"ready","disk":disk()})); }
fn service(state: &Arc<Mutex<State>>) -> TrashService {
    TrashService::new(state.lock().unwrap().allowed.clone(), journal())
}
fn restore_views(state:&Arc<Mutex<State>>,paths:&[String]){
    use std::os::unix::fs::MetadataExt;
    let files:Vec<_>=paths.iter().filter_map(|p|{let m=fs::symlink_metadata(p).ok()?;if !m.is_file(){return None}Some((PathBuf::from(p),m.blocks().saturating_mul(512),m.dev(),m.ino()))}).collect();
    let mut d=state.lock().unwrap();d.sessions.restore_paths(paths);d.space_cache.lock().unwrap().restore_files(&files);space::snapshot::request_save(d.space_cache.clone());let _=d.sessions.save(&home().join("Library/Application Support/KaruoCleaner/scan-sessions.json"));
}
fn records(state: &Arc<Mutex<State>>, proxy: &EventLoopProxy<UiEvent>) {
    if let Ok(bytes) = fs::read(journal()) {
        if serde_json::from_slice::<Value>(&bytes).is_err() {
            message(proxy, "恢复记录无法读取，已停止历史操作。请保留记录文件，不要覆盖。", true);
            return;
        }
    }
    let items: Vec<_> = service(state).history().into_iter().filter(|r| !r.restored).collect();
    emit(proxy, json!({"event":"history","records":items}));
}
fn confirm(title: &str, description: &str) -> bool {
    let Some(mtm) = objc2::MainThreadMarker::new() else { return false; };
    let alert = objc2_app_kit::NSAlert::new(mtm);
    alert.setMessageText(&objc2_foundation::NSString::from_str(title));
    alert.setInformativeText(&objc2_foundation::NSString::from_str(description));
    alert.addButtonWithTitle(&objc2_foundation::NSString::from_str("确认"));
    alert.addButtonWithTitle(&objc2_foundation::NSString::from_str("取消"));
    alert.runModal() == objc2_app_kit::NSAlertFirstButtonReturn
}
fn ids(request: &Value) -> Vec<String> {
    request["ids"].as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

fn exclusion_ids(request:&Value)->Vec<String>{request.get("selection").filter(|v|v.is_object()).unwrap_or(request)["exclude_ids"].as_array().map(|v|v.iter().filter_map(|x|x.as_str().map(str::to_owned)).collect()).unwrap_or_default()}

/// AI上下文来自当前扫描，失败必须回到同一条对话，不退化成“分析全部”。
fn chat_entries(request:&Value,state:&Arc<Mutex<State>>)->Result<Vec<Entry>,String>{
    let module=request["module"].as_str().unwrap_or("smart");
    if !["smart","junk","large","space","uninstall","optimize"].contains(&module){return Err("未知分析板块，请重新选择。".into())}
    let chosen=ids(request);let query=request["query"].as_str().unwrap_or("");let category=request["category"].as_str().unwrap_or("all");
    let d=state.lock().map_err(|_|"扫描状态暂不可用")?;
    if let Some(id)=request["scan_request_id"].as_u64(){if d.sessions.items.get(module).map(|s|s.request_id)!=Some(id){return Err("扫描批次已变化，请在当前结果中重新提问。".into())}}
    if module=="space" {
        let cache=d.space_cache.clone();drop(d);
        let (candidates,root)={let c=cache.lock().map_err(|_|"空间索引暂不可用")?;
            if let Some(path)=request["path"].as_str(){if Path::new(path)!=c.current{return Err("当前目录已变化，请在新目录中重新提问。".into())}}
            let selection=if chosen.is_empty(){Some((query,request["show_hidden"].as_bool().unwrap_or(false)))}else{None};
            (c.cleanup_candidates_excluding(&chosen,selection,&exclusion_ids(request))?,c.root.clone())};
        let entries:Vec<_>=candidates.into_iter().filter_map(|(p,dev,ino)|engine::make_entry(&p,"file",&root,"当前空间索引；AI只接收匿名统计").filter(|e|e.device==dev&&e.inode==ino)).collect();
        Ok(entries)
    } else {
        let found:Vec<_>=d.sessions.selected_excluding(module,category,query,&exclusion_ids(request)).into_iter().filter(|e|chosen.is_empty()||chosen.contains(&e.id)).collect();
        if !chosen.is_empty()&&chosen.iter().any(|id|!found.iter().any(|e|&e.id==id)){return Err("所选项目已不在当前结果或筛选中，请重新选择。".into())}
        Ok(found)
    }
}

fn applications() -> Vec<RunningApp> {
    let workspace = NSWorkspace::sharedWorkspace();
    workspace.runningApplications().iter().filter_map(|app| {
        let bundle = app.bundleIdentifier().map(|x| x.to_string()).unwrap_or_default();
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular ||
            bundle.starts_with("com.apple.") || app.processIdentifier() == std::process::id() as i32 { return None; }
        Some(RunningApp { pid: app.processIdentifier(),
            name: app.localizedName().map(|x| x.to_string()).unwrap_or_else(|| bundle.clone()),
            bundle_id: bundle, is_active: app.isActive() })
    }).collect()
}
fn show_apps(proxy: &EventLoopProxy<UiEvent>) {
    let count = fs::read_dir(home().join("Library/LaunchAgents")).map(|d| d.flatten().filter(|e| e.path().extension().is_some_and(|v| v == "plist")).count()).unwrap_or(0);
    emit(proxy, json!({"event":"apps","apps":applications(),"startup_count":count}));
}

fn scan_error(proxy:&EventLoopProxy<UiEvent>,request:&Value,text:&str){
    emit(proxy,json!({"event":"scan_error","module":request["module"].as_str().unwrap_or(if request["cmd"]=="space_start"{"space"}else{"smart"}),"request_id":request["request_id"],"text":text}));
}
fn scan(request: &Value, state: Arc<Mutex<State>>, proxy: EventLoopProxy<UiEvent>, generation: Arc<AtomicU64>, busy: Arc<AtomicBool>) {
    if busy.load(Ordering::SeqCst) { scan_error(&proxy,request,"正在处理文件；当前结果仍保留，完成后可重新扫描。"); return; }
    let module = request["module"].as_str().unwrap_or("smart").to_owned();
    if !["smart","junk","uninstall","space","large"].contains(&module.as_str()) { return; }
    let request_id = request["request_id"].as_u64().filter(|n|*n>0).unwrap_or_else(||generation.fetch_add(1,Ordering::SeqCst)+1);
    let min = request["min_mb"].as_u64().unwrap_or(100).clamp(1, 100_000) * 1_000_000;
    let days = if request["old_only"].as_bool().unwrap_or(false) { Some(90) } else { None };
    let root = {let data=state.lock().unwrap();if module=="large"{request["volume"].as_str().and_then(|id|data.volumes.iter().find(|v|v.id==id)).map(|v|PathBuf::from(&v.scan_root)).or_else(||data.roots.get(&module).cloned())}else{data.roots.get(&module).cloned()}};
    if ["space","large"].contains(&module.as_str())&&root.is_none(){scan_error(&proxy,request,"请先选择硬盘或文件夹。已有结果已保留。");return;}
    let token={let mut data=state.lock().unwrap();if module!="space"{if let Err(e)=sessions::mark_started(&home().join("Library/Application Support/KaruoCleaner/scan-sessions.json"),&module,request_id){scan_error(&proxy,request,&format!("无法保存扫描开始状态：{e}；已有结果保留。"));return;}}data.sessions.start(&module,request_id)};
    let root=root.map(|r|if r==Path::new("/")&&Path::new("/System/Volumes/Data").is_dir(){PathBuf::from("/System/Volumes/Data")}else{r});
    emit(&proxy,json!({"event":"scan_started","module":module,"request_id":request_id}));
    thread::spawn(move || {
        let progress = |text: String| { let timing={let mut data=state.lock().unwrap();if !data.sessions.progress(&module,request_id,&text){return}let s=&data.sessions.items[&module];(s.started_at_ms,s.elapsed_ms)};emit(&proxy, json!({"event":"progress","module":module,"text":text,"request_id":request_id,"started_at_ms":timing.0,"elapsed_ms":timing.1})); };
        let partial = |partial: &ScanResult| {
            let page={let mut d=state.lock().unwrap();if !d.sessions.current(&module,request_id){return}for entry in &partial.entries{if !d.sessions.items.get(&module).is_some_and(|s|s.removed.iter().any(|p|entry.path==*p||Path::new(&entry.path).starts_with(p))){d.approved.insert(entry.id.clone(),entry.clone());}}d.sessions.update(&module,request_id,partial.clone(),false);d.sessions.page(&module,None)};
            emit(&proxy,json!({"event":"scan_partial","module":module,"request_id":request_id,"data":page}));
        };
        let scan_result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match module.as_str() {
            "smart" | "junk" => engine::scan_junk_incremental(&token, progress, partial),
            "uninstall" => engine::scan_applications(&token, progress),
            "space" => engine::scan_space(root.as_ref().unwrap(), &token, progress),
            "large" => engine::scan_large_incremental(root.as_ref().unwrap(), min, days, &token, progress, |partial| {
                if state.lock().unwrap().sessions.current(&module,request_id) {
                    {let mut data=state.lock().unwrap();for entry in &partial.entries{if !data.sessions.items.get(&module).is_some_and(|s|s.removed.iter().any(|p|entry.path==*p||Path::new(&entry.path).starts_with(p))){data.approved.insert(entry.id.clone(),entry.clone());}if let Some(p)=Path::new(&entry.path).parent(){let p=p.to_path_buf();if !data.allowed.contains(&p){data.allowed.push(p);}}}}
                    let page={let mut d=state.lock().unwrap();d.sessions.update(&module,request_id,partial.clone(),false);d.sessions.page(&module,None)};emit(&proxy,json!({"event":"large_partial","module":"large","request_id":request_id,"data":page}));
                }
            }),
            _ => ScanResult::default(),
        }));
        let mut result=match scan_result {Ok(result)=>result,Err(_)=>{
            let page={let mut data=state.lock().unwrap_or_else(|e|e.into_inner());if !data.sessions.fail(&module,request_id,"扫描器遇到异常，已保留现有结果。请重新扫描；没有修改文件。"){return}let path=home().join("Library/Application Support/KaruoCleaner/scan-sessions.json");if data.sessions.save(&path).is_ok(){let _=sessions::mark_finished(&path,&module,request_id);}data.sessions.page(&module,None)};
            emit(&proxy,json!({"event":"result","module":module,"request_id":request_id,"data":page}));return;
        }};
        result.entries.sort_by(|a, b| b.size.cmp(&a.size));
        if !state.lock().unwrap().sessions.current(&module,request_id) { return; }
        {
            let mut data = state.lock().unwrap();
            for entry in &result.entries {
                if !data.sessions.items.get(&module).is_some_and(|s|s.removed.iter().any(|p|entry.path==*p||Path::new(&entry.path).starts_with(p))){data.approved.insert(entry.id.clone(), entry.clone());}
                if module=="large" {if let Some(parent)=Path::new(&entry.path).parent(){let parent=parent.to_path_buf();if !data.allowed.contains(&parent){data.allowed.push(parent);}}}
            }
            if module=="large" {if let (Some(parent),Ok(bytes))=(saved_roots().parent(),serde_json::to_vec(&data.allowed)) {let _=fs::create_dir_all(parent);let _=fs::write(saved_roots(),bytes);}}
        }
        let page={let mut d=state.lock().unwrap();d.sessions.update(&module,request_id,result,true);let path=home().join("Library/Application Support/KaruoCleaner/scan-sessions.json");if d.sessions.save(&path).is_ok(){let _=sessions::mark_finished(&path,&module,request_id);}else{message(&proxy,"扫描结果本次仍可查看，但未完整保存，请保留原快照并重试。",true);}d.sessions.page(&module,None)};
        emit(&proxy, json!({"event":"result","module":module,"data":page,"request_id":request_id}));
        ready(&proxy);
    });
}

fn handle(request: Value, state: Arc<Mutex<State>>, proxy: EventLoopProxy<UiEvent>, generation: Arc<AtomicU64>, busy: Arc<AtomicBool>) {
    let command = request["cmd"].as_str().unwrap_or("");
    if busy.load(Ordering::SeqCst) && !allowed_while_files_are_processing(command) {
        if ["scan","space_start"].contains(&command){scan_error(&proxy,&request,"正在处理文件；可以继续浏览已有结果，完成后再开始新扫描。")}else{message(&proxy, "正在处理文件，请等待完成后再操作。", true)}return;
    }
    match command {
        "rules_get" => emit(&proxy,json!({"event":"rules","profile":serde_json::from_str::<Value>(include_str!("../Assets/cleanup-rules.json")).unwrap_or(Value::Null),"scan":"只读，不设文件数量或总时长截断；权限失败与主动取消明确保留","cleanup":"后台扫描身份、真实路径、inode/device、大小、修改时间、打开占用重新核验；移入废纸篓可恢复","protected":["系统与Apple应用","运行中应用与本应用","用户保护名单","凭证与钥匙串","数据库与云占位","链接及共享数据"],"uninstall":"准确Bundle ID关联，AI审阅后逐项确认；应用数据与共享目录保留","schedule":"自动清理执行预算保留：50项、128MB；不处理个人文档与应用"})),
        "session_get" | "scan_page" => {let module=request["module"].as_str().unwrap_or("smart");let mut d=state.lock().unwrap();if module=="space"{let d_running=d.sessions.jobs.get("space").is_some_and(|j|!j.token.load(Ordering::SeqCst));let cache=d.space_cache.clone();let id=d.sessions.items.get(module).map(|s|s.request_id).unwrap_or(0);drop(d);let view=cache.lock().unwrap().view();emit(&proxy,json!({"event":"session","module":"space","request_id":id,"scanning":d_running,"data":view}));}else{let id=d.sessions.items.get(module).map(|s|s.request_id).unwrap_or(0);let filter=if command=="scan_page"{Some((request["category"].as_str().unwrap_or("all"),request["query"].as_str().unwrap_or(""),request["page"].as_u64().unwrap_or(0)as usize))}else{None};let page=d.sessions.page(module,filter);emit(&proxy,json!({"event":if command=="scan_page"{"scan_page"}else{"session"},"module":module,"request_id":id,"scanning":page["running"],"data":page}));}},
        "ai_chat" => {
            let module=request["module"].as_str().unwrap_or("smart").to_owned();let request_id=request["chat_request_id"].clone();
            thread::spawn(move||{
                let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{
                    let entries=chat_entries(&request,&state)?;
                    let restore_ids:Vec<_>=state.lock().map_err(|_|"撤回状态不可用")?.last_moved.iter().map(|r|r.id.clone()).collect();
                    Ok::<Value,String>(analysis::chat(&entries,&module,request["text"].as_str().unwrap_or(""),&request["history"].as_array().cloned().unwrap_or_default(),json!({"scope":"indexed_candidates","count":entries.len(),"restore_ids":restore_ids})))
                }));
                let mut reply=match result{Ok(Ok(v))=>v,Ok(Err(e))=>json!({"event":"ai_chat","module":module,"source":"rules","reply":"未完成分析，没有执行清理。","error":e,"proposals":[]}),Err(_)=>json!({"event":"ai_chat","module":module,"source":"rules","reply":"分析遇到异常，请重新提问；没有执行清理。","error":"分析任务已结束","proposals":[]})};
                reply["chat_request_id"]=request_id;emit(&proxy,reply);
            });
        },
        "ai_proposal_plan" => {let proposal_id=request["proposal_id"].as_str().or_else(||request["id"].as_str()).unwrap_or("").to_owned();let proposal=match analysis::proposal(request["proposal_id"].as_str().or_else(||request["id"].as_str()).unwrap_or("")){Ok(v)=>v,Err(e)=>{message(&proxy,e,true);return}};match proposal{analysis::Proposal::Cleanup(entries)=>{thread::spawn(move||match cleanup::build_entries(entries){Ok(mut v)=>{v["proposal_id"]=json!(proposal_id);emit(&proxy,v)},Err(e)=>message(&proxy,e,true)});},analysis::Proposal::Uninstall(app)=>{match uninstall::build_plan(&app){Ok(v)=>emit(&proxy,v),Err(e)=>message(&proxy,e,true)}},analysis::Proposal::Schedule(config)=>emit(&proxy,json!({"event":"ai_schedule_proposal","config":config})),analysis::Proposal::Restore(targets)=>emit(&proxy,json!({"event":"ai_restore_proposal","count":targets.len(),"action":{"cmd":"undo_last","proposal_id":request["proposal_id"].as_str().or_else(||request["id"].as_str()).unwrap_or("")},"executed":false}))}},
        "ai_get" => emit(&proxy,analysis::config_status()),
        "ai_save" => {let result=analysis::save_config(request["base_url"].as_str().unwrap_or(""),request["model"].as_str().unwrap_or(""),request["api_key"].as_str().unwrap_or(""));if let Err(e)=result{message(&proxy,e,true)}else{emit(&proxy,analysis::config_status())}},
        "ai_models" => {thread::spawn(move||emit(&proxy,analysis::refresh_models()));},
        "ai_analyze" => {let module=request["module"].as_str().unwrap_or("smart").to_string();let entries=state.lock().unwrap().sessions.selected(&module,request["category"].as_str().unwrap_or("all"),request["query"].as_str().unwrap_or(""));thread::spawn(move||emit(&proxy,analysis::analyze(&entries,&module)));},
        "space_cleanup_plan" => {let cache=state.lock().unwrap().space_cache.clone();let chosen=ids(&request);let (candidates,root)={let c=cache.lock().unwrap();let selection=if request["selection"].is_object(){Some((request["selection"]["query"].as_str().unwrap_or(""),request["selection"]["show_hidden"].as_bool().unwrap_or(false)))}else{None};let entries=match c.cleanup_candidates_excluding(&chosen,selection,&exclusion_ids(&request)){Ok(v)=>v,Err(e)=>{message(&proxy,e,true);return}};(entries,c.root.clone())};thread::spawn(move||match cleanup::build_plan(candidates,root){Ok(v)=>emit(&proxy,v),Err(e)=>message(&proxy,e,true)});},
        "cleanup_plan_page" => {match cleanup::page_plan(request["plan_id"].as_str().unwrap_or(""),request["page"].as_u64().unwrap_or(0)as usize){Ok(v)=>emit(&proxy,v),Err(e)=>message(&proxy,e,true)}},
        "cleanup_analyze" => {let id=request["plan_id"].as_str().unwrap_or("").to_owned();thread::spawn(move||match cleanup::analyze_plan(&id){Ok(v)=>emit(&proxy,v),Err(e)=>message(&proxy,e,true)});},
        "cleanup_apply" => {let entries=match cleanup::approved_entries_excluding(request["plan_id"].as_str().unwrap_or(""),&ids(&request),request["all"].as_bool().unwrap_or(false),&exclusion_ids(&request)){Ok(v)=>v,Err(e)=>{message(&proxy,e,true);return}};if entries.is_empty(){message(&proxy,"此计划没有可处理的明确候选。",true);return}if !(request["api_confirmed"]==true||confirm("将空间分析中所选文件移入废纸篓？",&format!("共 {} 个普通文件。目录只展开已索引文件，保护项已排除；每项仍会重新验证身份和占用。可用撤销恢复最近一批。",entries.len()))){return}busy.store(true,Ordering::SeqCst);thread::spawn(move||{let _busy_reset=BusyReset(busy.clone());let mut allowed=state.lock().unwrap().allowed.clone();for e in &entries{if let Some(p)=Path::new(&e.path).parent(){if !allowed.iter().any(|r|p.starts_with(r)){allowed.push(p.to_path_buf());}}}let report=TrashService::new(allowed.clone(),journal()).move_entries(&entries);let paths:Vec<_>=report.records.iter().map(|r|r.original_path.clone()).collect();{let mut d=state.lock().unwrap();if !report.records.is_empty(){d.last_moved=report.records.clone();}for p in allowed{if !d.allowed.contains(&p){d.allowed.push(p)}}let _=schedule::atomic_json(&saved_roots(),&d.allowed);d.approved.retain(|_,e|!paths.iter().any(|p|e.path==*p||Path::new(&e.path).starts_with(p)));d.sessions.remove_paths(&paths);d.space_cache.lock().unwrap().remove_paths(&paths);space::snapshot::request_save(d.space_cache.clone());let _=d.sessions.save(&home().join("Library/Application Support/KaruoCleaner/scan-sessions.json"));}drop(_busy_reset);emit(&proxy,json!({"event":"moved","module":"space","paths":paths,"count":report.records.len(),"size":report.records.iter().map(|r|r.size).sum::<u64>(),"errors":report.failures,"undo_available":!report.records.is_empty()}));ready(&proxy);});},
        "undo_last" => {let batch=state.lock().unwrap().last_moved.clone();if let Some(id)=request["proposal_id"].as_str(){if let Err(e)=analysis::validate_restore_proposal(id,&batch.iter().map(|r|r.id.clone()).collect::<Vec<_>>()){message(&proxy,e,true);return}}if batch.is_empty(){message(&proxy,"本次启动尚无可撤销的清理批次；之前的操作请在历史中恢复。",true);return}let history=service(&state).history();let chosen:Vec<_>=batch.into_iter().filter(|r|history.iter().any(|h|h.id==r.id&&!h.restored)).collect();if chosen.is_empty(){message(&proxy,"最近一批已经恢复或记录已变化。",true);return}busy.store(true,Ordering::SeqCst);thread::spawn(move||{let _busy_reset=BusyReset(busy.clone());let mut allowed=state.lock().unwrap().allowed.clone();for r in &chosen{if let Some(p)=Path::new(&r.original_path).parent(){allowed.push(p.to_path_buf())}}let errors=TrashService::new(allowed,journal()).restore(&chosen);let now=service(&state).history();let restored_paths:Vec<_>=chosen.iter().filter(|r|now.iter().any(|h|h.id==r.id&&h.restored)).map(|r|r.original_path.clone()).collect();restore_views(&state,&restored_paths);{let mut d=state.lock().unwrap();d.last_moved.retain(|r|now.iter().any(|h|h.id==r.id&&!h.restored));}drop(_busy_reset);emit(&proxy,json!({"event":"undo","restored":chosen.iter().filter(|r|now.iter().any(|h|h.id==r.id&&h.restored)).count(),"errors":errors}));records(&state,&proxy);ready(&proxy);});},
        "uninstall_plan" => {let entries={let d=state.lock().unwrap();if request["selection"].is_object(){d.sessions.selected_excluding("uninstall",request["selection"]["category"].as_str().unwrap_or("all"),request["selection"]["query"].as_str().unwrap_or(""),&exclusion_ids(&request))}else{let mut chosen=ids(&request);if let Some(id)=request["id"].as_str(){chosen.push(id.into())}chosen.iter().filter_map(|id|d.approved.get(id).cloned()).collect::<Vec<_>>()}};if entries.len()!=1{message(&proxy,"深度卸载请一次选择一个应用，便于逐项核对关联资料。",true);return}match uninstall::build_plan(&entries[0]){Ok(v)=>emit(&proxy,v),Err(e)=>message(&proxy,e,true)}},
        "uninstall_analyze" => {let id=request["plan_id"].as_str().unwrap_or("").to_owned();thread::spawn(move||match uninstall::analyze_plan(&id){Ok(v)=>emit(&proxy,v),Err(e)=>message(&proxy,e,true)});},
        "uninstall_apply" => {let chosen=ids(&request);let entries=match uninstall::approved_entries(request["plan_id"].as_str().unwrap_or(""),&chosen){Ok(v)=>v,Err(e)=>{message(&proxy,e,true);return}};if entries.is_empty(){message(&proxy,"请明确选择计划中的项目。",true);return}if !(request["api_confirmed"]==true||confirm("移入废纸篓所选卸载项目？","关联资料可能包含登录状态和应用设置；只处理你勾选且已审阅的精确路径，移入废纸篓可恢复。")){return}busy.store(true,Ordering::SeqCst);thread::spawn(move||{let _busy_reset=BusyReset(busy.clone());let mut allowed=state.lock().unwrap().allowed.clone();allowed.push(home().join("Library"));let report=TrashService::new(allowed,journal()).move_entries(&entries);let paths:Vec<_>=report.records.iter().map(|r|r.original_path.clone()).collect();{let mut d=state.lock().unwrap();if !report.records.is_empty(){d.last_moved=report.records.clone();}d.approved.retain(|_,e|!paths.iter().any(|p|e.path==*p||Path::new(&e.path).starts_with(p)));d.sessions.remove_paths(&paths);d.space_cache.lock().unwrap().remove_paths(&paths);space::snapshot::request_save(d.space_cache.clone());let _=d.sessions.save(&home().join("Library/Application Support/KaruoCleaner/scan-sessions.json"));}drop(_busy_reset);emit(&proxy,json!({"event":"moved","paths":paths,"count":report.records.len(),"errors":report.failures}));ready(&proxy);});},
        "startup_get" => emit(&proxy,startup::list()),
        "startup_set" => {let selected=ids(&request);let enabled=request["enabled"].as_bool().unwrap_or(false);if request["api_confirmed"]==true||confirm("更改所选登录项？","只更改可管理的当前用户登录项，不删除配置文件。确认后生效。") {emit(&proxy,startup::set(&selected,enabled));}},

        "ignore_get" | "ignore_add" | "ignore_remove" => {
            let mut error = None;
            let mut paths = match safety::load_ignored_paths() { Ok(p)=>p, Err(e)=>{emit(&proxy,json!({"event":"ignore","error":e}));return;} };
            if command == "ignore_add" {
                let entry = request["id"].as_str().and_then(|id|state.lock().unwrap().approved.get(id).cloned());
                if let Some(entry)=entry {
                    match fs::canonicalize(&entry.path) {
                        Ok(path) if path.to_string_lossy()==entry.path => {if !paths.contains(&path){paths.push(path)}},
                        _=>error=Some("文件路径已变化，请重新扫描再保护".to_string())
                    }
                } else {error=Some("此项不属于当前扫描，请先扫描".into())}
            } else if command=="ignore_remove" {
                if let Some(path)=request["path"].as_str(){paths.retain(|p|p!=Path::new(path));}else{error=Some("缺少要解除的路径".into())}
            }
            if command!="ignore_get" && error.is_none(){
                if paths.len()>1000{error=Some("保护名单最多1000项".into())}else if let Err(e)=schedule::atomic_json(&home().join("Library/Application Support/KaruoCleaner/ignored-paths.json"),&paths){error=Some(e)}
            }
            match safety::load_ignored_paths(){Ok(p)=>emit(&proxy,json!({"event":"ignore","paths":p,"error":error})),Err(e)=>emit(&proxy,json!({"event":"ignore","error":e}))}
        },
        "guardian_get" => {thread::spawn(move||emit(&proxy,guardian::status(None)));},
        "guardian_save" => {let result=serde_json::from_value::<guardian::Preferences>(request["preferences"].clone()).map_err(|_|"习惯设置格式无效".to_string()).and_then(guardian::save);emit(&proxy,guardian::status(result.err()));},
        "schedule_get" => {thread::spawn(move ||emit(&proxy,schedule::status(None)));},
        "schedule_save" => {
            let mut raw=request.clone();raw.as_object_mut().map(|m|m.remove("cmd"));
            let config=match serde_json::from_value::<schedule::Config>(raw){Ok(c)=>c,Err(_)=>{emit(&proxy,schedule::status(Some("设置参数无效，未保存".into())));return;}};
            if config.enabled && config.mode=="trash" && !confirm("开启定时保守清理？","开启后，只处理至少14天的pip、Go、Homebrew缓存和至少90天的旧日志；单次最多50项、128MB，移入废纸篓并可在历史中恢复。不会清理个人文档、下载、大型文件或应用，也不会清空废纸篓。构建进程运行或文件状态不明时跳过。"){
                emit(&proxy,schedule::status(Some("未开启自动清理，设置保持原状".into())));return;
            }
            thread::spawn(move ||{let error=schedule::save(config).err();emit(&proxy,schedule::status(error));});
        },
        "volumes" => {
            let state = state.clone(); let proxy = proxy.clone();
            thread::spawn(move || {
                let volumes=space::volumes();let h=home();let mut home_volume=volumes.iter().find(|v|v.internal).cloned();
                if let Some(v)=home_volume.as_mut(){v.id="home".into();v.name=format!("{}（用户目录）",h.file_name().unwrap_or_default().to_string_lossy());v.mount=h.to_string_lossy().into_owned();v.scan_root=v.mount.clone();}
                let mut choices=volumes.clone();if let Some(v)=&home_volume{choices.push(v.clone());}state.lock().unwrap().volumes=choices;
                emit(&proxy,json!({"event":"volumes","volumes":volumes,"home":home_volume}));
            });
        },
        "space_start" => {
            let id = request["volume"].as_str().unwrap_or("");
            let request_id = request["request_id"].as_u64().filter(|n|*n>0).unwrap_or_else(||generation.fetch_add(1,Ordering::SeqCst)+1);
            let (volume,cache,token) = {
                let mut data = state.lock().unwrap();
                let volume = if let Some(path)=request["path"].as_str() {
                    let selected=PathBuf::from(path);
                    if data.roots.get("space")!=Some(&selected){scan_error(&proxy,&request,"请先通过选择文件夹批准扫描目录。已有结果已保留。");return;}
                    let root=if selected==Path::new("/")&&Path::new("/System/Volumes/Data").is_dir(){"/System/Volumes/Data".into()}else{selected.to_string_lossy().into_owned()};
                    space::Volume{id:path.into(),name:path.into(),mount:path.into(),scan_root:root,total:0,available:0,used:0,internal:false}
                } else {let Some(volume)=data.volumes.iter().find(|v|v.id==id).cloned()else{scan_error(&proxy,&request,"请选择已挂载的硬盘。已有结果已保留。");return;};volume};
                if let Err(e)=space::snapshot::begin(Path::new(&volume.scan_root)){scan_error(&proxy,&request,&format!("无法保存扫描状态：{e}；已有结果仍保留。"));return;}
                let token=data.sessions.start("space",request_id);
                let cache = Arc::new(Mutex::new(space::Cache::default()));cache.lock().unwrap().volume_id=Some(volume.id.clone()); data.space_cache = cache.clone();
                (volume,cache,token)
            };
            emit(&proxy,json!({"event":"scan_started","module":"space","request_id":request_id}));
            let fail_token=token.clone();
            let snapshot_epoch=space::snapshot::epoch();
            thread::spawn(move || {
                let volume_id = volume.id.clone();
                let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||space::scan(PathBuf::from(volume.scan_root),token,cache.clone(),|data| {
                    if state.lock().unwrap().sessions.current("space",request_id) {emit(&proxy,json!({"event":"space_view","volume":volume_id,"request_id":request_id,"data":data}));}
                })));
                {let mut d=state.lock().unwrap();if d.sessions.current("space",request_id){if let Some(s)=d.sessions.items.get_mut("space"){s.done=true;}d.sessions.jobs.remove("space");}}
                if result.is_ok(){
                    cache.lock().unwrap().volume_id=Some(volume_id.clone());
                    let saved=space::snapshot::save(&cache,snapshot_epoch);
                    let data={let mut c=cache.lock().unwrap();match saved{Ok(_)=>{},Err(e) if space::snapshot::superseded(&e)=>{},Err(e)=>c.snapshot_notice=Some(format!("空间结果未保存：{e}"))}c.view()};
                    if state.lock().unwrap().sessions.items.get("space").is_some_and(|s|s.request_id==request_id){emit(&proxy,json!({"event":"space_view","module":"space","request_id":request_id,"data":data}));}
                }
                if result.is_err() {fail_token.store(true,Ordering::SeqCst);}
                if result.is_err() && state.lock().unwrap().sessions.items.get("space").is_some_and(|s|s.request_id==request_id){emit(&proxy,json!({"event":"space_error","request_id":request_id,"text":"扫描器遇到读取错误，已停止。没有修改任何文件，请换目录重试。"}));}
            });
        },
        "space_browse" => {
            let path = PathBuf::from(request["path"].as_str().unwrap_or(""));
            let request_id = request["request_id"].as_u64().unwrap_or(0);
            let cache = state.lock().unwrap().space_cache.clone();
            thread::spawn(move || {let mut c=cache.lock().unwrap();if c.browse(&path){let data=c.view_page(request["page"].as_u64().unwrap_or(0) as usize,request["query"].as_str().unwrap_or(""),request["show_hidden"].as_bool().unwrap_or(false));if let Err(e)=space::snapshot::save_view(&c){c.snapshot_notice=Some(format!("浏览位置未保存：{e}"));}emit(&proxy,json!({"event":"space_view","request_id":request_id,"data":data}));}});
        },
        "ready" => ready(&proxy),
        "scan" => scan(&request, state, proxy, generation, busy),
        "cancel" => {if let Some(module)=request["module"].as_str(){
            let stopped={let mut d=state.lock().unwrap();d.sessions.cancel(module);if module=="space"&&d.sessions.jobs.contains_key(module){let id=d.sessions.items[module].request_id;d.sessions.jobs.remove(module);if let Some(s)=d.sessions.items.get_mut(module){s.done=true;s.outcome="cancelled".into();}Some((d.space_cache.clone(),id))}else{None}};
            if let Some((cache,id))=stopped{
                let view={let mut c=cache.lock().unwrap();c.stop_partial();c.view()};emit(&proxy,json!({"event":"space_view","module":"space","request_id":id,"data":view}));let snapshot_epoch=space::snapshot::epoch();
                thread::spawn(move||{let result=space::snapshot::save(&cache,snapshot_epoch);let view={let mut c=cache.lock().unwrap();if let Err(e)=result{if !space::snapshot::superseded(&e){c.snapshot_notice=Some(format!("部分空间结果未保存：{e}"));}}c.view()};if state.lock().unwrap().sessions.items.get("space").is_some_and(|s|s.request_id==id){emit(&proxy,json!({"event":"space_view","module":"space","request_id":id,"data":view}));}});
            }
        }},
        "root_set" => {let module=request["module"].as_str().unwrap_or("space");if !["space","large"].contains(&module){message(&proxy,"此模块不支持自定义扫描目录。",true);return}let path=PathBuf::from(request["path"].as_str().unwrap_or(""));if !path.is_absolute(){message(&proxy,"扫描目录必须是绝对路径。",true);return}match fs::canonicalize(path){Ok(root) if root.is_dir()=>{state.lock().unwrap().roots.insert(module.into(),root.clone());emit(&proxy,json!({"event":"root","module":module,"path":root,"read_only":true}));},_=>message(&proxy,"所选目录不存在或无法读取。",true)}},
        "pick_root" => {
            let module = request["module"].as_str().unwrap_or("space");
            if !["space","large"].contains(&module) { return; }
            let Some(root) = rfd::FileDialog::new().set_title("选择要分析的具体文件夹").pick_folder() else { return; };
            let Ok(root) = fs::canonicalize(root) else { message(&proxy, "无法读取所选文件夹。", true); return; };
            if !valid_picked_root(&root) {
                message(&proxy, "请选择可以读取的硬盘或文件夹。", true); return;
            }
            {
                let mut data = state.lock().unwrap();
                data.roots.insert(module.into(), root.clone());
                // 选目录只授权分析；实际清理权限仍来自扫描到的具体候选父目录。
                // 只保存通过原生选择框批准的具体目录，便于下次启动恢复清理记录。
                let selected: Vec<_> = data.allowed.iter().filter(|p| valid_picked_root(p)).cloned().collect();
                let path = saved_roots();
                if let (Some(parent), Ok(bytes)) = (path.parent(), serde_json::to_vec(&selected)) {
                    if fs::create_dir_all(parent).is_ok() {
                        let temp = path.with_extension("tmp");
                        if fs::write(&temp, bytes).is_ok() { let _ = fs::rename(temp, path); }
                    }
                }
            }
            emit(&proxy, json!({"event":"root","module":module,"path":root}));
        },
        "move" => {
            let ignored=match safety::load_ignored_paths(){Ok(v)=>v,Err(e)=>{message(&proxy,e,true);return}};
            let mut selected=ids(&request);
            let entries:Vec<_>={let data=state.lock().unwrap();if request["selection"].is_object(){let entries=data.sessions.selected_excluding(request["module"].as_str().unwrap_or(""),request["selection"]["category"].as_str().unwrap_or("all"),request["selection"]["query"].as_str().unwrap_or(""),&exclusion_ids(&request));let entries:Vec<_>=entries.into_iter().filter(|e|safety::selection_protection_reason(Path::new(&e.path),&ignored).is_none()).collect();selected=entries.iter().map(|e|e.id.clone()).collect();entries}else{selected.iter().filter_map(|id|data.approved.get(id).cloned()).collect()}};
            if let Ok(bytes)=serde_json::to_vec(&state.lock().unwrap().allowed){let _=fs::create_dir_all(saved_roots().parent().unwrap());let _=fs::write(saved_roots(),bytes);}
            if entries.is_empty() || entries.len() != selected.len() { message(&proxy, "选择已失效，请重新扫描。", true); return; }
            if entries.iter().any(|e| e.kind == "folder" || e.kind.starts_with("space_")) {
                message(&proxy, "空间透镜只用于查看。请到大型和旧文件中选择具体文件，避免整文件夹误删。", true); return;
            }
            let size: u64 = entries.iter().map(|e| e.size).sum();
            if !(request["api_confirmed"]==true || confirm("移入废纸篓？", &format!("将移动 {} 项（文件逻辑大小 {:.1} MB）。可以恢复。大型或旧文件不一定是垃圾，请确认不再需要。\n移入废纸篓不会立即释放空间。", entries.len(), size as f64 / 1_000_000.0))) { return; }

            busy.store(true, Ordering::SeqCst);
            thread::spawn(move || {
                let _busy_reset = BusyReset(busy.clone());
                let result = service(&state).move_entries(&entries);
                let paths: Vec<_> = result.records.iter().map(|r| r.original_path.clone()).collect();
                let size: u64 = result.records.iter().map(|r| r.size).sum();
                {
                    let mut data = state.lock().unwrap();
                    if !result.records.is_empty(){data.last_moved=result.records.clone();}
                    data.approved.retain(|_,e|!paths.iter().any(|p|e.path==*p||Path::new(&e.path).starts_with(p)));data.sessions.remove_paths(&paths);data.space_cache.lock().unwrap().remove_paths(&paths);space::snapshot::request_save(data.space_cache.clone());let _=data.sessions.save(&home().join("Library/Application Support/KaruoCleaner/scan-sessions.json"));
                }
                drop(_busy_reset);
                emit(&proxy, json!({"event":"moved","paths":paths,"count":result.records.len(),"size":size,"errors":result.failures}));
                ready(&proxy);
            });
        },
        "history" => records(&state, &proxy),
        "restore" | "purge" => {
            let selected = ids(&request);
            let chosen: Vec<_> = service(&state).history().into_iter().filter(|r| !r.restored && selected.contains(&r.id)).collect();
            if chosen.is_empty() || chosen.len() != selected.len() { message(&proxy, "记录不存在或已变化，未操作。", true); return; }
            let purge = command == "purge";
            if purge && !confirm("永久清理这些文件？", &format!("将永久删除本应用记录的 {} 项。此操作不可撤销，不会清空废纸篓中的其他内容。", chosen.len())) { return; }
            busy.store(true, Ordering::SeqCst);
            thread::spawn(move || {
                let _busy_reset = BusyReset(busy.clone());
                let before = disk()["available"].as_u64().unwrap_or(0);
                let errors = if purge { service(&state).purge(&chosen) } else {
                    let mut allowed=state.lock().unwrap().allowed.clone();
                    let h=home();let residual_roots=[h.join("Library/Preferences"),h.join("Library/Saved Application State")];
                    for record in &chosen {if let Some(parent)=Path::new(&record.original_path).parent(){if residual_roots.iter().any(|r|parent.starts_with(r)){allowed.push(parent.to_path_buf());}}}
                    TrashService::new(allowed,journal()).restore(&chosen)
                };
                if !purge {let now=service(&state).history();let restored_paths:Vec<_>=chosen.iter().filter(|r|now.iter().any(|h|h.id==r.id&&h.restored)).map(|r|r.original_path.clone()).collect();restore_views(&state,&restored_paths);}
                let after = disk()["available"].as_u64().unwrap_or(0);
                let mut text = if errors.is_empty() {
                    if purge { "已永久清理选定项目。".to_owned() } else { "已恢复到原位置。".to_owned() }
                } else { format!("部分操作未完成：\n{}", errors.iter().take(8).cloned().collect::<Vec<_>>().join("\n")) };
                if purge { text.push_str(&format!("\n系统可用空间变化：{:.1} MB。系统读数可能延后更新，并受其他程序活动影响。", after.saturating_sub(before) as f64 / 1_000_000.0)); }
                drop(_busy_reset);
                message(&proxy, text, !errors.is_empty()); records(&state, &proxy); ready(&proxy);
            });
        },
        "optimize" => show_apps(&proxy),
        "quit" | "terminate" => {
            let pids: Vec<_> = request["pids"].as_array().map(|v| v.iter().filter_map(|v| v.as_i64().map(|p| p as i32)).collect()).unwrap_or_default();
            let targets: Vec<_> = applications().into_iter().filter(|a| pids.contains(&a.pid)).collect();
            if targets.is_empty() { return; }
            if !(request["api_confirmed"]==true || confirm("正常退出这些应用？", &format!("{}\n\n不会强制结束进程。应用可能需要你先保存文件。", targets.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join("、")))) { return; }
            let mut count = 0;
            for app in &targets {
                if let Some(running) = NSRunningApplication::runningApplicationWithProcessIdentifier(app.pid) {
                    if running.terminate() { count += 1; }
                }
            }
            message(&proxy, format!("已发送 {count} 个正常退出请求。请处理应用自己的保存提示；没有强制结束进程。"), false);
            show_apps(&proxy);
        },
        "open_login" | "login_settings" => {
            let url = objc2_foundation::NSURL::URLWithString(&objc2_foundation::NSString::from_str("x-apple.systempreferences:com.apple.LoginItems-Settings.extension"));
            if let Some(url) = url { NSWorkspace::sharedWorkspace().openURL(&url); }
        },
        "reveal" => {
            let id = request["id"].as_str().unwrap_or("");
            if let Some(entry) = state.lock().unwrap().approved.get(id) {
                let _ = std::process::Command::new("/usr/bin/open").args(["-R", &entry.path]).spawn();
            }
        },
        _ => {},
    }
}

/// 磁盘写入期间仍保持界面可读、可导航。文件清理/扫描等待；本机设置和启动项调整可继续使用。
fn allowed_while_files_are_processing(command: &str) -> bool {
    matches!(command,
        "ready" | "cancel" | "session_get" | "scan_page" | "volumes" | "history" |
        "ignore_get" | "schedule_get" | "guardian_get" | "ai_get" | "rules_get" |
        "optimize" | "startup_get" | "login_settings" | "reveal" | "ai_chat" |
        "ai_analyze" | "ai_models" | "uninstall_plan" | "uninstall_analyze" |
        "cleanup_plan_page" | "cleanup_analyze" | "space_browse" | "space_cleanup_plan" |
        "ai_save" | "schedule_save" | "guardian_save" | "ignore_add" | "ignore_remove" |
        "startup_set" | "root_set" | "ai_proposal_plan")
}

#[cfg(test)]
mod busy_gate_tests {
    use super::{allowed_while_files_are_processing, BusyReset};
    use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

    #[test]
    fn file_processing_keeps_read_only_pages_available() {
        for command in ["optimize", "startup_get", "startup_set", "session_get", "space_browse", "history", "ai_analyze", "ai_save", "guardian_save"] {
            assert!(allowed_while_files_are_processing(command), "{command} must remain available");
        }
    }

    #[test]
    fn file_processing_still_blocks_new_mutating_operations() {
        for command in ["move", "cleanup_apply", "uninstall_apply", "purge", "restore", "undo_last", "scan", "space_start"] {
            assert!(!allowed_while_files_are_processing(command), "{command} must wait");
        }
    }

    #[test]
    fn panicking_file_worker_releases_the_busy_gate() {
        let busy = Arc::new(AtomicBool::new(true));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let busy = busy.clone();
            move || { let _reset = BusyReset(busy); panic!("fixture worker failure"); }
        }));
        assert!(result.is_err());
        assert!(!busy.load(Ordering::SeqCst));
    }
}

fn main() -> wry::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|v|v=="--directory-worker") {let _=read_worker::run();return Ok(());}
    if args.get(1).is_some_and(|v|v=="--api"){use std::io::Read;let raw=if args.get(2).is_some_and(|v|v=="-"){let mut b=String::new();std::io::stdin().read_to_string(&mut b).unwrap();b}else{args.get(2).cloned().unwrap_or_else(||"{}".into())};match serde_json::from_str::<Value>(&raw).map_err(|e|e.to_string()).and_then(|r|control::client(&r)){Ok(v)=>println!("{v}"),Err(e)=>{eprintln!("{e}");std::process::exit(1)}}return Ok(());}

    if args.get(1).is_some_and(|v|v=="--scheduled-run") {match schedule::run(){Ok(report)=>println!("{report}"),Err(e)=>{eprintln!("定时任务未执行：{e}");std::process::exit(1)}}return Ok(())}
    if args.get(1).is_some_and(|v|v=="--guardian-check"){println!("{}",guardian::status(None));return Ok(())}
    if args.get(1).is_some_and(|v|v=="--schedule-check") {println!("{}",schedule::status(None));return Ok(())}

    if args.get(1).is_some_and(|v|v=="--space-bench") {
        let root=args.get(2).and_then(|v|fs::canonicalize(v).ok()).filter(|p|p.is_dir()).expect("需要有效的只读测速目录");
        let seconds=args.get(3).and_then(|v|v.parse::<u64>().ok()).unwrap_or(15).clamp(1,300);
        let cancel=Arc::new(AtomicBool::new(false));let stop=cancel.clone();
        thread::spawn(move||{thread::sleep(std::time::Duration::from_secs(seconds));stop.store(true,Ordering::SeqCst);});
        let cache=Arc::new(Mutex::new(space::Cache::default()));let start=std::time::Instant::now();
        let first=Mutex::new(None::<u128>);let updates=AtomicU64::new(0);
        space::scan(root,cancel,cache.clone(),|value|{
            if value["scanned"].as_u64().unwrap_or(0)>0{let mut f=first.lock().unwrap();if f.is_none(){*f=Some(start.elapsed().as_millis());}}
            let count=updates.fetch_add(1,Ordering::Relaxed);
            if count<3||value["done"]==true {println!("{}",json!({"phase":"update","elapsed_ms":start.elapsed().as_millis(),"scanned":value["scanned"],"bytes":value["size"],"done":value["done"],"truncated":value["truncated"]}));}
        });
        let c=cache.lock().unwrap();println!("{}",json!({"phase":"summary","elapsed_ms":start.elapsed().as_millis(),"first_result_ms":*first.lock().unwrap(),"scanned":c.scanned,"skipped":c.skipped,"bytes":c.nodes.get(&c.root).map(|n|n.0),"done":c.done,"truncated":c.truncated}));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--self-test") {
        std::process::exit(selftest::run());
    }
    let event_loop = EventLoopBuilder::<UiEvent>::with_user_event().build();
    let window = WindowBuilder::new().with_title("毕方AI")
        .with_inner_size(LogicalSize::new(1180.0, 760.0))
        .with_min_inner_size(LogicalSize::new(960.0, 600.0)).build(&event_loop).expect("无法创建窗口");
    let mut allowed = vec![home().join(".npm/_cacache"),home().join(".cargo/registry/cache"),home().join("Library/Caches"), home().join("Library/Logs"), PathBuf::from("/Applications"), home().join("Applications")];
    if let Ok(bytes) = fs::read(saved_roots()) {
        if let Ok(roots) = serde_json::from_slice::<Vec<PathBuf>>(&bytes) {
            for root in roots {
                if let Ok(root) = fs::canonicalize(root) {
                    if root.is_dir() && valid_picked_root(&root) && !allowed.contains(&root) { allowed.push(root); }
                }
            }
        }
    }
    allowed.retain(|p| p.exists());
    let state = Arc::new(Mutex::new(State { volumes: Vec::new(), space_cache: Arc::new(Mutex::new(space::Cache::default())), approved: HashMap::new(), roots: HashMap::new(), allowed, sessions: sessions::Sessions::default(), last_moved:Vec::new() }));
    let session_load_error={let mut data=state.lock().unwrap();match sessions::Sessions::load(&home().join("Library/Application Support/KaruoCleaner/scan-sessions.json")){Ok(items)=>{for session in items.values(){for e in &session.result.entries{data.approved.insert(e.id.clone(),e.clone());}}data.sessions.items=items;None},Err(e)=>Some(e)}};
    let busy = Arc::new(AtomicBool::new(false));
    let generation = Arc::new(AtomicU64::new(0));
    let proxy = event_loop.create_proxy();
    let hub=control::start(proxy.clone()).unwrap_or_else(|e|{eprintln!("本机API未启动，已停止此实例以避免窗口与接口状态分离：{e}");std::process::exit(1)});
    {
        let state=state.clone();let proxy=proxy.clone();let original=state.lock().unwrap().space_cache.clone();
        thread::spawn(move||match space::snapshot::load(){
            Ok(Some(mut cache))=>{
                // 历史是真实文件动作记录；不能让上次索引带回已成功移走的项目。
                let removed:Vec<_>=service(&state).history().into_iter().filter(|r|!r.restored).map(|r|r.original_path).filter(|p|Path::new(p).starts_with(&cache.root)).collect();
                if !removed.is_empty(){cache.remove_paths(&removed);}
                let request_id=cache.saved_at.saturating_mul(1000).max(1);let view=cache.view();
                let mut d=state.lock().unwrap();if !Arc::ptr_eq(&d.space_cache,&original)||d.sessions.jobs.contains_key("space"){return}
                d.roots.insert("space".into(),cache.root.clone());
                d.space_cache=Arc::new(Mutex::new(cache));d.sessions.items.insert("space".into(),sessions::Session{request_id,done:true,..Default::default()});drop(d);
                emit(&proxy,json!({"event":"session","module":"space","request_id":request_id,"scanning":false,"data":view}));
            },Ok(None)=>{},Err(e)=>{message(&proxy,format!("空间快照未恢复：{e}。原快照已保留，可以重新扫描。"),true)}
        });
    }
    if let Some(error)=session_load_error{message(&proxy,format!("扫描快照未恢复：{error}。原文件已保留。"),true);}
    let ipc_proxy = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_custom_protocol("karuo".into(), |_id, request| {
            let (status,bytes):(u16,&'static [u8])=match request.uri().path() {"/space-hero.png" => (200,include_bytes!("../Assets/space-hero.png")), "/smart-hero.png" => (200,include_bytes!("../Assets/smart-hero.png")), "/smart-clean.png" => (200,include_bytes!("../Assets/smart-clean.png")), "/smart-protect.png" => (200,include_bytes!("../Assets/smart-protect.png")), "/smart-files.png" => (200,include_bytes!("../Assets/smart-files.png")), _ => (404,b"")};
            wry::http::Response::builder().status(status).header("Content-Type","image/png").body(std::borrow::Cow::Borrowed(bytes)).unwrap()
        })
        .with_html(include_str!("../ui/index.html"))
        .with_navigation_handler(|url| url == "about:blank" || url.starts_with("about:blank#"))
        .with_ipc_handler(move |request: wry::http::Request<String>| {
            if let Ok(value) = serde_json::from_str(request.body()) { let _ = ipc_proxy.send_event(UiEvent::Request(value)); }
        }).build(&window)?;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UiEvent::Output(value)) => {
                control::broadcast(&hub,&value);
                if let Ok(encoded) = serde_json::to_string(&value) {
                    let _ = webview.evaluate_script(&format!("window.receive({encoded})"));
                }
            },
            Event::UserEvent(UiEvent::Request(request)) => handle(request, state.clone(), proxy.clone(), generation.clone(), busy.clone()),
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                if busy.load(Ordering::SeqCst) { message(&proxy, "正在处理文件，请完成后再关闭窗口。", true); }
                else { state.lock().unwrap().sessions.cancel_all(); *control_flow = ControlFlow::Exit; }
            },
            _ => {},
        }
    });
}

#[cfg(test)]mod ai_scope_tests{
 use super::*;
 fn state()->Arc<Mutex<State>>{Arc::new(Mutex::new(State{volumes:vec![],space_cache:Arc::new(Mutex::new(space::Cache::default())),approved:HashMap::new(),roots:HashMap::new(),allowed:vec![],sessions:sessions::Sessions::default(),last_moved:vec![]}))}
 #[test]fn stale_ai_scan_and_selection_fail_before_model_request(){let s=state();let _t=s.lock().unwrap().sessions.start("junk",123);assert!(chat_entries(&json!({"module":"junk","scan_request_id":122}),&s).unwrap_err().contains("批次"));assert!(chat_entries(&json!({"module":"junk","scan_request_id":123,"ids":["missing"]}),&s).unwrap_err().contains("所选"));}
 #[test]fn changed_space_directory_fails_before_disk_reads(){let s=state();assert!(chat_entries(&json!({"module":"space","path":"/old-directory","ids":["/old-directory/file"]}),&s).unwrap_err().contains("目录"));}
}
