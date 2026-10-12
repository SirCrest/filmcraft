//! Deeply nested projects: a saved bin tree opens again; hostile nesting is refused with an
//! error, never a stack overflow. Everything runs on a 1 MiB stack, the smallest `decode` gets
//! (the Windows main thread, wasm).

use filmcraft_format::{FormatError, MAX_BIN_DEPTH, MAX_NESTING, SCHEMA_VERSION, decode, encode};
use filmcraft_project::{BinId, ItemKind, Label, Project};
use filmcraft_time::FrameRate;

/// Run `f` on a thread with a 1 MiB stack; a panic in it fails the test with its own message.
fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    let worker = std::thread::Builder::new().stack_size(1 << 20).spawn(f).unwrap();
    worker.join().unwrap_or_else(|e| std::panic::resume_unwind(e));
}

/// A project whose bins are nested `depth` deep, named by `name`, with an item in the deepest.
fn deep_bins(depth: usize, name: impl Fn(usize) -> String) -> Project {
    let mut p = Project::new("Deep");
    let mut parent: Option<BinId> = None;
    for i in 0..depth {
        parent = Some(p.add_bin(&name(i), parent));
    }
    let rate = FrameRate::FPS_24;
    p.add_item("Layer", Label::Iris, ItemKind::AdjustmentLayer { width: 1920, height: 1080, rate, duration: rate.tick_of(48) }, parent);
    p
}

fn plain(i: usize) -> String {
    format!("Bin {i}")
}

fn is_too_deep(r: Result<filmcraft_format::Loaded, FormatError>) -> bool {
    matches!(r, Err(FormatError::Corrupt(m)) if m.contains("nested more than"))
}

/// Regression: bins nested 42 deep saved but did not open again ("recursion limit exceeded":
/// each bin level costs three JSON levels and serde_json stops at 128). The Premiere importer
/// keeps 64 levels; the format opens up to `MAX_BIN_DEPTH`.
#[test]
fn deep_bins_round_trip() {
    on_small_stack(|| {
        for depth in [42, 64, MAX_BIN_DEPTH] {
            let p = deep_bins(depth, plain);
            for pretty in [true, false] {
                let l = decode(&encode(&p, pretty)).unwrap_or_else(|e| panic!("bins nested {depth} deep do not open: {e}"));
                assert_eq!(l.project, p);
            }
        }
    });
}

/// Older schemas take the slow path (parse to a `Value`, migrate, deserialize the `Value`): the
/// same depth opens there too.
#[test]
fn deep_bins_open_from_an_older_schema() {
    on_small_stack(|| {
        let p = deep_bins(MAX_BIN_DEPTH, plain);
        let text = String::from_utf8(encode(&p, false)).unwrap();
        let older = text.replacen(&format!("\"schema_version\":{SCHEMA_VERSION}"), &format!("\"schema_version\":{}", SCHEMA_VERSION - 1), 1);
        assert_ne!(older, text);
        let l = decode(older.as_bytes()).unwrap_or_else(|e| panic!("an older deep project does not open: {e}"));
        assert!(l.migrated());
        assert_eq!(l.project, p);
    });
}

/// The bound is exact and documented: one bin deeper is refused with an error naming it.
#[test]
fn one_bin_past_the_bound_is_refused() {
    on_small_stack(|| {
        let e = decode(&encode(&deep_bins(MAX_BIN_DEPTH + 1, plain), false)).unwrap_err();
        assert!(e.to_string().contains(&format!("bins nested up to {MAX_BIN_DEPTH} deep")), "{e}");
    });
}

/// Brackets, quotes and backslashes in names are text, not nesting.
#[test]
fn brackets_in_names_do_not_count() {
    on_small_stack(|| {
        let p = deep_bins(MAX_BIN_DEPTH, tricky);
        for pretty in [true, false] {
            assert_eq!(decode(&encode(&p, pretty)).unwrap().project, p);
        }
    });
}

fn tricky(i: usize) -> String {
    match i % 3 {
        0 => format!("Bin {i} \\"),
        1 => format!("\"{}\" {i}", "[".repeat(600)),
        _ => format!("{}\\\"{}", "{".repeat(600), "\\"),
    }
}

/// Hostile files deeper than any project are refused before anything recursive runs: deep
/// arrays, deep objects, a deep view beside a project, a deep legacy (pre-envelope) project.
#[test]
fn hostile_deep_documents_are_refused() {
    on_small_stack(|| {
        let n = 1_000_000;
        let arrays = format!("{}{}", "[".repeat(n), "]".repeat(n));
        let objects = format!("{}1{}", r#"{"a":"#.repeat(n), "}".repeat(n));
        let unclosed = "{".repeat(n);
        let project = String::from_utf8(encode(&Project::new("x"), false)).unwrap();
        let view = format!("{},\"view\":{}}}", project.strip_suffix('}').unwrap(), arrays);
        let legacy = format!(r#"{{"version":1,"items":{{}},"root":{objects}}}"#);
        for doc in [&arrays, &objects, &unclosed, &view, &legacy] {
            assert!(is_too_deep(decode(doc.as_bytes())), "{}…", &doc[..40]);
        }
    });
}

/// The most expensive way to reach the limit (objects parsed into a `Value`, here the editor
/// view beside the project) still fits the stack.
#[test]
fn the_worst_case_at_the_limit_fits_the_stack() {
    on_small_stack(|| {
        let p = Project::new("x");
        let project = String::from_utf8(encode(&p, false)).unwrap();
        let with_view = |levels: usize| format!("{},\"view\":{}1{}}}", project.strip_suffix('}').unwrap(), r#"{"a":"#.repeat(levels), "}".repeat(levels));
        let l = decode(with_view(MAX_NESTING - 1).as_bytes()).unwrap_or_else(|e| panic!("a project with a deep view does not open: {e}"));
        assert_eq!(l.project, p);
        assert!(is_too_deep(decode(with_view(MAX_NESTING).as_bytes())));
    });
}

/// Mutation run over a project at the bound: truncations and flipped bytes (a quote that no longer
/// opens a name full of brackets, a stray backslash…) give an error or a project, never a crash.
#[test]
fn damaged_deep_projects_never_crash() {
    on_small_stack(|| {
        let bytes = encode(&deep_bins(MAX_BIN_DEPTH, tricky), false);
        let step = (bytes.len() / 600).max(1);
        for at in (0..bytes.len()).step_by(step) {
            let _ = decode(&bytes[..at]);
            for b in [b'"', b'\\', b'[', b'{', b']', b'}', bytes[at] ^ 0x20] {
                let mut m = bytes.clone();
                m[at] = b;
                let _ = decode(&m);
            }
        }
    });
}
