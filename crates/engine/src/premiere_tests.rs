//! Premiere Pro projects through the command interface: File ▸ Open reads one as the project (never
//! writing over it), File ▸ Import merges one into the open project.

use super::*;
use filmcraft_project::MediaRef;
use serde_json::json;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fc-prproj-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const S: i64 = 254_016_000_000;

/// A small Premiere project: a bin with an audio file (its recorded path missing, its
/// project-relative one present), a Color Matte and a sequence using both, with an effect FilmCraft
/// does not have.
fn project_xml() -> String {
    let o = |tag: &str, id: &str, body: &str| {
        let attr = if id.contains('-') { "ObjectUID" } else { "ObjectID" };
        format!("<{tag} {attr}=\"{id}\" ClassID=\"0\" Version=\"1\">{body}</{tag}>\n")
    };
    let pi = |name: &str, extra: &str| format!("<ProjectItem Version=\"1\"><Name>{name}</Name></ProjectItem>{extra}");
    let items = |ids: &[&str]| ids.iter().map(|i| format!("<Item ObjectURef=\"{i}\"/>")).collect::<String>();
    [
        o(
            "RootProjectItem",
            "a0000000-0000-0000-0000-000000000001",
            &pi("Root Bin", &format!("<ProjectItemContainer><Items>{}</Items></ProjectItemContainer>", items(&["b0000000-0000-0000-0000-000000000001", "c0000000-0000-0000-0000-000000000003"]))),
        ),
        o(
            "BinProjectItem",
            "b0000000-0000-0000-0000-000000000001",
            &pi("Audio", &format!("<ProjectItemContainer><Items>{}</Items></ProjectItemContainer>", items(&["c0000000-0000-0000-0000-000000000001", "c0000000-0000-0000-0000-000000000002"]))),
        ),
        o("ClipProjectItem", "c0000000-0000-0000-0000-000000000001", &pi("Narration", "<MasterClip ObjectURef=\"d0000000-0000-0000-0000-000000000001\"/>")),
        o("ClipProjectItem", "c0000000-0000-0000-0000-000000000002", &pi("Red Matte", "<MasterClip ObjectURef=\"d0000000-0000-0000-0000-000000000002\"/>")),
        o("ClipProjectItem", "c0000000-0000-0000-0000-000000000003", &pi("Cut", "<MasterClip ObjectURef=\"d0000000-0000-0000-0000-000000000003\"/>")),
        o("MasterClip", "d0000000-0000-0000-0000-000000000001", "<Clips><Clip ObjectRef=\"10\"/></Clips><Name>Narration</Name>"),
        o("AudioClip", "10", &format!("<Clip><Source ObjectRef=\"11\"/><InPoint>0</InPoint><OutPoint>{}</OutPoint></Clip>", 4 * S)),
        o("AudioMediaSource", "11", "<MediaSource><Media ObjectURef=\"e0000000-0000-0000-0000-000000000001\"/></MediaSource>"),
        o(
            "Media",
            "e0000000-0000-0000-0000-000000000001",
            "<AudioStream ObjectRef=\"12\"/><FilePath>Z:\\gone\\narration.wav</FilePath><RelativePath>media\\narration.wav</RelativePath><Title>narration.wav</Title>",
        ),
        o("AudioStream", "12", &format!("<FrameRate>5292000</FrameRate><Duration>{}</Duration>", 4 * S)),
        o("MasterClip", "d0000000-0000-0000-0000-000000000002", "<Clips><Clip ObjectRef=\"13\"/></Clips><Name>Red Matte</Name>"),
        o("VideoClip", "13", &format!("<Clip><Source ObjectRef=\"14\"/><InPoint>0</InPoint><OutPoint>{}</OutPoint></Clip>", 5 * S)),
        o("VideoMediaSource", "14", "<MediaSource><Media ObjectURef=\"e0000000-0000-0000-0000-000000000002\"/></MediaSource>"),
        o("Media", "e0000000-0000-0000-0000-000000000002", "<VideoStream ObjectRef=\"15\"/><FilePath>1129270354</FilePath><ImporterPrefs>/wAAAAEAAAA=</ImporterPrefs>"),
        o("VideoStream", "15", "<FrameRate>10160640000</FrameRate><FrameRect>0,0,1280,720</FrameRect>"),
        o("MasterClip", "d0000000-0000-0000-0000-000000000003", "<Clips><Clip ObjectRef=\"16\"/></Clips><Name>Cut</Name>"),
        o("VideoClip", "16", "<Clip><Source ObjectRef=\"17\"/></Clip>"),
        o("VideoSequenceSource", "17", "<SequenceSource><Sequence ObjectURef=\"f0000000-0000-0000-0000-000000000001\"/></SequenceSource>"),
        o(
            "Sequence",
            "f0000000-0000-0000-0000-000000000001",
            "<TrackGroups><TrackGroup><Second ObjectRef=\"20\"/></TrackGroup><TrackGroup><Second ObjectRef=\"21\"/></TrackGroup></TrackGroups><Name>Cut</Name>",
        ),
        o(
            "VideoTrackGroup",
            "20",
            "<TrackGroup><Tracks><Track ObjectURef=\"f1000000-0000-0000-0000-000000000001\"/></Tracks><FrameRate>10160640000</FrameRate></TrackGroup><FrameRect>0,0,1280,720</FrameRect>",
        ),
        o("AudioTrackGroup", "21", "<TrackGroup><Tracks><Track ObjectURef=\"f1000000-0000-0000-0000-000000000002\"/></Tracks><FrameRate>5292000</FrameRate></TrackGroup>"),
        o("VideoClipTrack", "f1000000-0000-0000-0000-000000000001", "<ClipTrack><Track/><ClipItems><TrackItems><TrackItem ObjectRef=\"30\"/></TrackItems></ClipItems></ClipTrack>"),
        o("AudioClipTrack", "f1000000-0000-0000-0000-000000000002", "<ClipTrack><Track/><ClipItems><TrackItems><TrackItem ObjectRef=\"31\"/></TrackItems></ClipItems></ClipTrack>"),
        o(
            "VideoClipTrackItem",
            "30",
            &format!("<ClipTrackItem><ComponentOwner><Components ObjectRef=\"40\"/></ComponentOwner><TrackItem><End>{}</End></TrackItem><SubClip ObjectRef=\"32\"/></ClipTrackItem>", 2 * S),
        ),
        o("SubClip", "32", "<Clip ObjectRef=\"13\"/><MasterClip ObjectURef=\"d0000000-0000-0000-0000-000000000002\"/><Name>Red Matte</Name>"),
        o("VideoComponentChain", "40", "<ComponentChain><Components><Component ObjectRef=\"41\"/></Components></ComponentChain>"),
        o("VideoFilterComponent", "41", "<Component><DisplayName>Warp Stabilizer</DisplayName></Component><MatchName>AE.ADBE SubspaceStabilizer</MatchName>"),
        o("AudioClipTrackItem", "31", &format!("<ClipTrackItem><TrackItem><End>{}</End></TrackItem><SubClip ObjectRef=\"33\"/></ClipTrackItem>", 4 * S)),
        o("SubClip", "33", "<Clip ObjectRef=\"10\"/><MasterClip ObjectURef=\"d0000000-0000-0000-0000-000000000001\"/><Name>Narration</Name>"),
    ]
    .iter()
    .fold("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<PremiereData Version=\"3\">\n".to_string(), |acc, x| acc + x)
        + "</PremiereData>\n"
}

/// The project at `<dir>/Show/Show Cut.prproj` with its media at `<dir>/Show/media/narration.wav`.
fn write_project(dir: &std::path::Path) -> String {
    let show = dir.join("Show");
    std::fs::create_dir_all(show.join("media")).unwrap();
    std::fs::write(show.join("media").join("narration.wav"), b"RIFF").unwrap();
    let path = show.join("Show Cut.prproj");
    std::fs::write(&path, project_xml()).unwrap();
    path.to_string_lossy().to_string()
}

#[test]
fn opens_a_premiere_project_as_the_project() {
    let d = temp_dir("open");
    let path = write_project(&d);
    let mut s = Session::default();
    let r = s.execute("file.open", json!({"path": path})).unwrap();
    assert_eq!(r["format"], "Premiere Pro project", "{r}");
    assert_eq!(r["relinkedMedia"], 1, "the project-relative copy of the narration was found");
    assert_eq!(s.project.name, "Show Cut");
    assert_eq!(s.path, None, "the session has no FilmCraft file yet");
    assert!(!s.is_dirty());
    // the bins, items and the sequence, which is open
    let names: Vec<&str> = s.project.items.values().map(|i| i.name.as_str()).collect();
    for n in ["Narration", "Red Matte", "Cut"] {
        assert!(names.contains(&n), "{names:?}");
    }
    let cut = s.project.items.values().find(|i| i.name == "Cut").unwrap().id;
    assert_eq!(s.state.active_sequence, Some(cut));
    let narration = s.project.items.values().find(|i| i.name == "Narration").and_then(|i| i.as_media()).unwrap();
    let MediaRef::File { path: media } = &narration.media else { panic!() };
    assert!(std::path::Path::new(media).exists(), "{media}");
    assert!(s.offline.missing.is_empty());
    // what was not brought over is in the Events panel; the open is announced
    let events = s.execute("events.list", json!({})).unwrap().to_string();
    assert!(events.contains("Warp Stabilizer"), "{events}");
    assert!(s.drain_events().iter().any(|e| matches!(e, Event::Toast { message, .. } if message.contains("Premiere Pro project"))));
    assert_eq!(s.prefs.general.recent_projects.first(), Some(&path), "recents reopen it");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_premiere_project_is_never_written_over() {
    let d = temp_dir("save");
    let path = write_project(&d);
    let before = std::fs::read(&path).unwrap();
    let mut s = Session::default();
    s.execute("file.open", json!({"path": path})).unwrap();
    s.execute("sequence.addEdit", json!({"seconds": 1.0})).ok();
    // Save has no file to go to (the UI asks with Save As); saving onto the .prproj is refused
    assert!(s.execute("file.save", json!({})).is_err());
    let e = s.execute("file.save", json!({"path": path})).unwrap_err().to_string();
    assert!(e.contains("does not write Premiere Pro projects"), "{e}");
    assert!(s.execute("file.saveAs", json!({"path": path.to_uppercase()})).is_err(), "any case of .prproj");
    assert!(s.execute("file.saveCopy", json!({"path": path})).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before, "Premiere's file is untouched");
    // Save As a FilmCraft project, which then reopens as it was saved
    let fc = d.join("Show").join("Show Cut.fcproj").to_string_lossy().to_string();
    s.execute("file.saveAs", json!({"path": fc})).unwrap();
    assert_eq!(s.path.as_deref(), Some(fc.as_str()));
    assert!(!s.is_dirty());
    let mut t = Session::default();
    t.execute("file.open", json!({"path": fc})).unwrap();
    assert_eq!(format!("{:?}", t.project), format!("{:?}", s.project));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn import_merges_a_premiere_project_into_a_bin() {
    let d = temp_dir("import");
    let path = write_project(&d);
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let before = s.project.items.len();
    let r = s.execute("file.import", json!({"paths": [path]})).unwrap();
    assert_eq!(r["documents"][0]["format"], "Premiere Pro project", "{r}");
    assert_eq!(r["sequences"].as_array().map(Vec::len), Some(1), "{r}");
    assert_eq!(s.project.items.len(), before + 3);
    let bin = s.project.root.children.iter().find_map(|c| match c {
        filmcraft_project::BinEntry::Bin(b) if b.name == "Show Cut" => Some(b),
        _ => None,
    });
    assert!(bin.is_some(), "the project comes in as a bin named after it");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.items.len(), before, "one undo step");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_damaged_premiere_project_is_an_error_not_a_crash() {
    let d = temp_dir("damaged");
    let path = write_project(&d);
    let xml = project_xml();
    std::fs::write(&path, &xml.as_bytes()[..xml.len() / 2]).unwrap();
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let name = s.project.name.clone();
    assert!(s.execute("file.open", json!({"path": path})).is_err());
    assert_eq!(s.project.name, name, "the open project stays");
    let _ = std::fs::remove_dir_all(&d);
}

/// Equal JSON, numbers to a relative 1e-12. `Err` names the first difference.
fn same(a: &Value, b: &Value, at: &str) -> std::result::Result<(), String> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN));
            if x == y || (x - y).abs() <= 1e-12 * x.abs().max(y.abs()) { Ok(()) } else { Err(format!("{at}: {x} vs {y}")) }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x.iter().zip(y).enumerate().try_for_each(|(i, (x, y))| same(x, y, &format!("{at}[{i}]"))),
        (Value::Object(x), Value::Object(y)) if x.len() == y.len() => {
            x.iter().try_for_each(|(k, v)| y.get(k).ok_or_else(|| format!("{at}.{k} missing")).and_then(|w| same(v, w, &format!("{at}.{k}"))))
        }
        _ if a == b => Ok(()),
        _ => Err(format!("{at}: {a} vs {b}")),
    }
}

/// Opt-in: every real project under `FILMCRAFT_PRPROJ_CORPUS` opens, saves as a FilmCraft project
/// (in a temporary directory) and reopens unchanged. Real projects are never committed.
#[test]
fn real_projects_open_save_and_reopen() {
    let Ok(dir) = std::env::var("FILMCRAFT_PRPROJ_CORPUS") else { return };
    fn collect(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            let n = e.file_name().to_string_lossy().to_string();
            if p.is_dir() && !n.contains("Auto-Save") {
                collect(&p, out);
            } else if n.to_ascii_lowercase().ends_with(".prproj") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    collect(std::path::Path::new(&dir), &mut files);
    let out = temp_dir("corpus");
    for (i, f) in files.iter().enumerate() {
        let mut s = Session::default();
        let t0 = std::time::Instant::now();
        let r = s.execute("file.open", json!({"path": f.to_string_lossy()})).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        let open_ms = t0.elapsed().as_millis();
        let fc = out.join(format!("{i}.fcproj")).to_string_lossy().to_string();
        s.execute("file.saveAs", json!({"path": fc})).unwrap();
        let mut t = Session::default();
        t.execute("file.open", json!({"path": fc})).unwrap();
        // what was saved is what reopens (numbers to 1e-12: the JSON reader may round the last bit)
        let enc = |p: &filmcraft_project::Project| serde_json::from_slice::<Value>(&filmcraft_format::encode_with_view(p, None, false)).unwrap();
        if let Err(at) = same(&enc(&s.project), &enc(&t.project), "") {
            panic!("{}: reopened differs at {at}", f.display());
        }
        println!(
            "{open_ms:>6} ms  {}  sequences {}  relinked {}  missing {}  report {}",
            f.file_name().unwrap_or_default().to_string_lossy(),
            r["sequences"].as_array().map(Vec::len).unwrap_or(0),
            r["relinkedMedia"],
            r["missingMedia"],
            r["report"].as_array().map(Vec::len).unwrap_or(0)
        );
    }
    let _ = std::fs::remove_dir_all(&out);
}
