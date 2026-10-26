//! Standard responder-chain actions keep WebKit editing and clipboard shortcuts native.
pub fn install(){
 use objc2::{MainThreadMarker, sel};
 use objc2_app_kit::{NSApplication,NSMenu,NSMenuItem,NSEventModifierFlags};
 use objc2_foundation::NSString;
 let Some(mtm)=MainThreadMarker::new()else{return};
 let app=NSApplication::sharedApplication(mtm);
 let menu=app.mainMenu().unwrap_or_else(||NSMenu::new(mtm));
 if menu.numberOfItems()==0{let application=NSMenu::new(mtm);let parent=NSMenuItem::new(mtm);parent.setSubmenu(Some(&application));menu.addItem(&parent);unsafe{application.addItemWithTitle_action_keyEquivalent(&NSString::from_str("退出轻理AI"),Some(sel!(terminate:)),&NSString::from_str("q"));}}
 let edit=NSMenu::new(mtm);edit.setTitle(&NSString::from_str("编辑"));
 for (title,action,key) in [("撤销",sel!(undo:),"z"),("重做",sel!(redo:),"Z"),("剪切",sel!(cut:),"x"),("复制",sel!(copy:),"c"),("粘贴",sel!(paste:),"v"),("全选",sel!(selectAll:),"a")]{
  // A nil target routes to the focused WebKit editor, never to application IPC.
  let item=unsafe{edit.addItemWithTitle_action_keyEquivalent(&NSString::from_str(title),Some(action),&NSString::from_str(key))};
  item.setKeyEquivalentModifierMask(if key=="Z"{NSEventModifierFlags::Command|NSEventModifierFlags::Shift}else{NSEventModifierFlags::Command});
 }
 let parent=NSMenuItem::new(mtm);parent.setTitle(&NSString::from_str("编辑"));parent.setSubmenu(Some(&edit));menu.addItem(&parent);app.setMainMenu(Some(&menu));
}
