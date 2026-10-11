//! A project file dropped from the OS on its own: it opens (a FilmCraft or Premiere Pro project), as
//! File ▸ Open does, unless the open project has unsaved changes; then a Premiere project is
//! imported into it and a FilmCraft one is refused with a message.

use std::path::{Path, PathBuf};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use serde_json::json;

fn step(h: &mut Harness<'static, FilmcraftApp>) {
    let ctx = h.ctx.clone();
    let mut raw = std::mem::take(h.input_mut());
    eframe::App::raw_input_hook(h.state_mut(), &ctx, &mut raw);
    *h.input_mut() = raw;
    ctx.request_repaint();
    h.step();
}

fn harness(s: Session) -> Harness<'static, FilmcraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).build_eframe(move |_cc| FilmcraftApp::new(s));
    for _ in 0..3 {
        step(&mut h);
    }
    h
}

#[derive(Debug)]
struct OsFile(PathBuf);

impl egui::DroppedFile for OsFile {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

fn drop_file(h: &mut Harness<'static, FilmcraftApp>, path: &Path) {
    h.input_mut().dropped_files.push(std::sync::Arc::new(OsFile(path.to_path_buf())));
    step(h);
    h.input_mut().dropped_files.clear();
    step(h);
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fc-drop-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A Premiere Pro project with one bin holding one audio file.
fn write_premiere(dir: &Path) -> PathBuf {
    let xml = r#"<?xml version="1.0" encoding="UTF-8" ?>
<PremiereData Version="3">
<RootProjectItem ObjectUID="a0000000-0000-0000-0000-000000000001" ClassID="0" Version="1"><ProjectItem><Name>Root Bin</Name></ProjectItem><ProjectItemContainer><Items><Item ObjectURef="b0000000-0000-0000-0000-000000000001"/></Items></ProjectItemContainer></RootProjectItem>
<BinProjectItem ObjectUID="b0000000-0000-0000-0000-000000000001" ClassID="0" Version="1"><ProjectItem><Name>Sound</Name></ProjectItem><ProjectItemContainer><Items><Item ObjectURef="c0000000-0000-0000-0000-000000000001"/></Items></ProjectItemContainer></BinProjectItem>
<ClipProjectItem ObjectUID="c0000000-0000-0000-0000-000000000001" ClassID="0" Version="1"><ProjectItem><Name>Theme</Name></ProjectItem><MasterClip ObjectURef="d0000000-0000-0000-0000-000000000001"/></ClipProjectItem>
<MasterClip ObjectUID="d0000000-0000-0000-0000-000000000001" ClassID="0" Version="1"><Clips><Clip ObjectRef="10"/></Clips><Name>Theme</Name></MasterClip>
<AudioClip ObjectID="10" ClassID="0" Version="1"><Clip><Source ObjectRef="11"/></Clip></AudioClip>
<AudioMediaSource ObjectID="11" ClassID="0" Version="1"><MediaSource><Media ObjectURef="e0000000-0000-0000-0000-000000000001"/></MediaSource></AudioMediaSource>
<Media ObjectUID="e0000000-0000-0000-0000-000000000001" ClassID="0" Version="1"><FilePath>theme.wav</FilePath></Media>
</PremiereData>
"#;
    let p = dir.join("Show Reel.prproj");
    std::fs::write(&p, xml).unwrap();
    p
}

#[test]
fn a_dropped_premiere_project_opens() {
    let d = temp_dir("open");
    let p = write_premiere(&d);
    let mut h = harness(Session::default());
    assert!(!h.state().session.is_dirty());
    drop_file(&mut h, &p);
    let s = &h.state().session;
    assert_eq!(s.project.name, "Show Reel");
    assert_eq!(s.path, None, "Premiere's file is not the session's file");
    assert!(s.project.items.values().any(|i| i.name == "Theme"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn unsaved_changes_are_kept() {
    let d = temp_dir("dirty");
    let p = write_premiere(&d);
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s.execute("file.newBin", json!({"name": "Selects"})).unwrap();
    assert!(s.is_dirty());
    let name = s.project.name.clone();
    let mut h = harness(s);
    // a Premiere project is imported into a bin of the open project
    drop_file(&mut h, &p);
    let s = &h.state().session;
    assert_eq!(s.project.name, name);
    assert!(s.project.root.children.iter().any(|c| matches!(c, filmcraft_engine::project::BinEntry::Bin(b) if b.name == "Show Reel")));
    // a FilmCraft project is not opened over them
    let fc = d.join("other.fcproj");
    std::fs::write(&fc, b"{}").unwrap();
    let items = h.state().session.project.items.len();
    drop_file(&mut h, &fc);
    assert_eq!(h.state().session.project.items.len(), items);
    assert!(h.state().ui.status.contains("Save or revert"), "{}", h.state().ui.status);
    let _ = std::fs::remove_dir_all(&d);
}
