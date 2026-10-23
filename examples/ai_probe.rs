#[path="../src/model.rs"] mod model;
#[path="../src/actions.rs"] mod actions;
#[path="../src/engine.rs"] mod engine;
#[path="../src/safety.rs"] mod safety;
#[path="../src/schedule.rs"] mod schedule;
#[path="../src/analysis.rs"] mod analysis;
fn main(){let entry=model::Entry{id:"own-fixture".into(),name:"not-transmitted".into(),path:"/local-only/not-transmitted".into(),size:4096,modified:Some(1),kind:"cache".into(),detail:"not-transmitted".into(),is_dir:false,scope_root:"/local-only".into(),inode:0,device:0,mtime_nsec:0};println!("{}",analysis::analyze(&[entry],"junk"));}
