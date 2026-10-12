//! Opt-in: import every Premiere Pro project under `FILMCRAFT_PRPROJ_CORPUS` (a directory; auto-save
//! folders are skipped) and check the result's invariants. Skipped when the variable is unset:
//! real projects are private and never committed.
//!
//! `FILMCRAFT_PRPROJ_CORPUS=/path/to/projects cargo test -p filmcraft-interchange --test prproj_corpus -- --nocapture`

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use filmcraft_interchange::{ImportOptions, prproj};
use filmcraft_project::{BinEntry, ItemKind, Project};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if !name.contains("Auto-Save") {
                collect(&p, out);
            }
        } else if name.to_ascii_lowercase().ends_with(".prproj") {
            out.push(p);
        }
    }
}

fn bin_items(p: &Project) -> Vec<filmcraft_project::ItemId> {
    fn walk(b: &filmcraft_project::Bin, out: &mut Vec<filmcraft_project::ItemId>) {
        for c in &b.children {
            match c {
                BinEntry::Item(i) => out.push(*i),
                BinEntry::Bin(b) => walk(b, out),
            }
        }
    }
    let mut out = Vec::new();
    walk(&p.root, &mut out);
    out
}

/// `FILMCRAFT_PRPROJ_DUMP=<sequence name>`: print that sequence's tracks, clips and transitions.
fn dump(p: &Project, want: &str) {
    let secs = |t: filmcraft_time::Tick| t.seconds();
    if !p.sequences().any(|i| i.name.contains(want)) {
        println!("   sequences: {:?}", p.sequences().map(|i| i.name.as_str()).collect::<Vec<_>>());
    }
    for it in p.items.values().filter(|i| i.name.contains(want)) {
        let Some(seq) = it.as_sequence() else { continue };
        let st = &seq.settings;
        println!("== {} {}x{} {} fps df={} {} Hz", it.name, st.width, st.height, st.frame_rate.as_f64(), st.drop_frame, st.sample_rate);
        for m in &seq.markers {
            println!("   marker {:.3}s +{:.3}s {:?} {:?} \"{}\"", secs(m.start), secs(m.duration), m.kind, m.color, m.name);
        }
        for t in seq.all_tracks() {
            println!("  track {:?} \"{}\" locked={} enabled={} muted={} sync={}", t.kind, t.name, t.locked, t.enabled, t.muted, t.sync_lock);
            for c in &t.items {
                let src = p.item(c.item).map(|i| i.name.as_str()).unwrap_or("?");
                println!(
                    "    {:8.3}s +{:7.3}s in {:8.3}s x{:.3}{} {} \"{}\" <- \"{}\" {:?} link={:?} gain={} hold={:?} stf={}",
                    secs(c.start),
                    secs(c.duration),
                    secs(c.source_in),
                    c.speed,
                    if c.reverse { " rev" } else { "" },
                    if c.enabled { "" } else { "DISABLED" },
                    c.name,
                    src,
                    c.label,
                    c.link,
                    c.gain_db,
                    c.frame_hold.map(secs),
                    c.scale_to_frame
                );
                for e in &c.effects {
                    let def = e.def();
                    let changed: Vec<String> = e
                        .params
                        .iter()
                        .filter(|(k, v)| v.is_animated() || def.and_then(|d| d.param(k)).is_none_or(|d| format!("{:?}", d.default) != format!("{:?}", v.value)))
                        .map(|(k, v)| {
                            let kf: Vec<String> = v.keyframes.iter().take(4).map(|k| format!("{:.3}s={:?}/{:?}", secs(k.time), k.value, k.interp)).collect();
                            format!("{k}={:?}{}", v.value, if kf.is_empty() { String::new() } else { format!(" kf[{}]", kf.join(", ")) })
                        })
                        .collect();
                    if !changed.is_empty() || !e.enabled {
                        println!("        fx {}{}: {}", e.effect, if e.enabled { "" } else { " (off)" }, changed.join("; "));
                    }
                }
            }
            for tr in &t.transitions {
                println!("    ~ {:8.3}s +{:.3}s {} {:?} from={:?} to={:?}", secs(tr.start), secs(tr.duration), tr.effect.effect, tr.align, tr.from, tr.to);
            }
        }
    }
}

#[test]
fn corpus_imports_cleanly() {
    let Ok(dir) = std::env::var("FILMCRAFT_PRPROJ_CORPUS") else {
        eprintln!("FILMCRAFT_PRPROJ_CORPUS not set; skipped");
        return;
    };
    let mut files = Vec::new();
    collect(Path::new(&dir), &mut files);
    files.sort();
    assert!(!files.is_empty(), "no .prproj files under {dir}");
    let mut failures = Vec::new();
    for f in &files {
        let bytes = std::fs::read(f).unwrap();
        assert!(prproj::sniff(&bytes), "{} not sniffed", f.display());
        let opts = ImportOptions {
            base_dir: f.parent().map(|d| d.to_string_lossy().to_string()),
            name: f.file_stem().map(|s| s.to_string_lossy().to_string()),
            ..Default::default()
        };
        let t0 = std::time::Instant::now();
        let r = match prproj::import(&bytes, &opts) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{}: {e}", f.display()));
                continue;
            }
        };
        let ms = t0.elapsed().as_millis();
        let p = &r.imported.project;
        // every item is in exactly one bin
        let listed = bin_items(p);
        let unique: HashSet<_> = listed.iter().copied().collect();
        assert_eq!(listed.len(), unique.len(), "{}: an item is in two bins", f.display());
        for id in p.items.keys() {
            assert!(unique.contains(id), "{}: item {id:?} is in no bin", f.display());
        }
        let (mut clips, mut transitions, mut markers, mut effects, mut keyed) = (0, 0, 0, 0, 0);
        for it in p.items.values() {
            if let ItemKind::Media(m) = &it.kind {
                markers += m.markers.len();
            }
            let Some(seq) = it.as_sequence() else { continue };
            markers += seq.markers.len();
            for t in seq.all_tracks() {
                t.check().unwrap_or_else(|e| panic!("{}: sequence \"{}\": {e}", f.display(), it.name));
                let ids: HashSet<_> = t.items.iter().map(|c| c.id).collect();
                for c in &t.items {
                    assert!(p.item(c.item).is_some(), "{}: clip \"{}\" points at a missing item", f.display(), c.name);
                    assert!(c.speed.is_finite() && c.speed > 0.0, "{}: clip \"{}\" speed {}", f.display(), c.name, c.speed);
                    clips += 1;
                    markers += c.markers.len();
                    effects += c.effects.iter().filter(|e| e.def().is_some_and(|d| !d.intrinsic)).count();
                    keyed += c.effects.iter().flat_map(|e| e.params.values()).filter(|p| p.is_animated()).count();
                }
                for tr in &t.transitions {
                    assert!(tr.duration.0 > 0);
                    for side in [tr.from, tr.to].into_iter().flatten() {
                        assert!(ids.contains(&side), "{}: transition points at a clip on another track", f.display());
                    }
                    transitions += 1;
                }
            }
        }
        let seqs = p.sequences().count();
        let media = p.items.values().filter(|i| matches!(i.kind, ItemKind::Media(_))).count();
        println!(
            "{:>6} ms  {:<55} seqs {seqs:>3} (top {:>3})  media {media:>4}  clips {clips:>5}  transitions {transitions:>4}  markers {markers:>4}  effects {effects:>5}  animated {keyed:>4}  rel {:>4}",
            ms,
            f.file_name().unwrap().to_string_lossy(),
            r.imported.sequences.len(),
            r.relative_paths.len()
        );
        for e in &r.report.entries {
            println!("            {:?}: {} (×{})", e.level, e.message, e.count);
        }
        if let Ok(want) = std::env::var("FILMCRAFT_PRPROJ_DUMP") {
            dump(p, &want);
        }
    }
    assert!(failures.is_empty(), "failed:\n{}", failures.join("\n"));
}
