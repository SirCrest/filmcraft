//! Adobe Premiere Pro projects (`.prproj`): import, so a Premiere project opens like a FilmCraft one.
//!
//! A project file is gzip-compressed XML (RFC 1952): `<PremiereData Version="3">` holding a flat
//! list of objects. Each object carries `ObjectID="n"` or `ObjectUID="guid"` (plus `ClassID` and
//! `Version`) and points at others with `ObjectRef="n"` / `ObjectURef="guid"` reference elements.
//! Only the top-level children are objects: some property blobs nest private id scopes of their own.
//!
//! The model, learnt from projects recent Premiere Pro releases wrote (project versions 43 and 45;
//! clean-room, never from Adobe code):
//! - `RootProjectItem` / `BinProjectItem` → `ProjectItemContainer/Items/Item` → bins and
//!   `ClipProjectItem`s (name, `Column.PropertyText.Label`), each pointing at a `MasterClip`.
//! - `MasterClip/Clips/Clip` → `VideoClip` / `AudioClip`, whose `Clip/Source` is a media source
//!   (`MediaSource/Media` → `Media`: `FilePath`, `RelativePath`, `VideoStream` / `AudioStream`)
//!   or a sequence source (`SequenceSource/Sequence`). Synthetic media (Black Video, Color Matte,
//!   Transparent Video, graphics, captions) have a numeric four-character code as `FilePath`.
//! - `Sequence/TrackGroups` → video and audio track groups (`FrameRate` in ticks per frame or
//!   sample, `FrameRect`) → tracks → `ClipItems` (track items) and `TransitionItems`.
//! - A track item's `TrackItem/Start`/`End` are sequence ticks; its `SubClip/Clip` holds the
//!   source `InPoint`/`OutPoint` (zero-based media ticks), `PlaybackSpeed` and `PlayBackwards`.
//! - Effects are `VideoFilterComponent`s (`MatchName`, params with `StartKeyframe` and
//!   `Keyframes` = `time,value,type,subtype,inSpeed,inInfluence,outSpeed,outInfluence;…`) and
//!   `AudioFilterComponent`s (`FilterMatchName`). Keyframe times are media ticks, as in FilmCraft.
//!   Positions are fractions of the frame (`x:y`), anchors fractions of the source, volume levels
//!   linear amplitudes where 1.0 is +15 dB.
//!
//! Premiere's tick (254 016 000 000/s) is FilmCraft's, so every time carries over exactly.
//!
//! Imported: bins, media (files with their stream properties, Color Matte, Black Video,
//! Transparent Video, adjustment layers), sequences (settings, tracks with name / lock / mute /
//! sync lock, clips with in/out, speed, reverse, enable, label, clip gain, links, groups, nesting),
//! transitions (by name; single- and double-sided), markers (sequence, source and clip; type and
//! colour), sequence in/out, and the effects Motion (with its crop), Opacity, Volume, Channel
//! Volume, Lumetri Color (Basic Correction and Creative sliders), Crop, Drop Shadow, Ultra Key,
//! Luma Key and Posterize Time, with keyframes. Everything else is listed in the [`Report`].

use std::collections::{HashMap, HashSet};

use filmcraft_geom::Vec2;
use filmcraft_media::{Generator, MediaKind};
use filmcraft_project::effect::{BLEND_MODES, ParamKind};
use filmcraft_project::{
    BinId, ClipId, Interpolation, ItemId, ItemKind, Keyframe, Label, Marker, MarkerId, MarkerKind, MediaRef, Param, ParamValue, SequenceSettings, Track,
    TrackItem, TrackKind, Transition, TransitionAlign, TransitionId, find_effect, validate_frame_size,
};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};
use roxmltree::Node;

use crate::common::{
    Builder, MediaSpec, empty_sequence, file_name, gain_to_db, is_absolute, kind_from_ext, media_clip, media_info, resolve_path, set_param, settings_for,
    transition_effect,
};
use crate::xml::{child, child_bool, child_f64, child_i64, child_text, children, path_text};
use crate::{Error, ImportOptions, Imported, Report, Result};

const FORMAT: &str = "Premiere Pro project";

/// Largest uncompressed project accepted. Real projects are tens of megabytes of XML; the parsed
/// document takes several times its size in memory.
pub const MAX_XML_BYTES: usize = 512 << 20;

/// Deepest bin nesting followed (a damaged or hostile file could nest forever).
const MAX_BIN_DEPTH: usize = 64;

/// Deepest XML element nesting parsed. Real projects nest 8 deep; the XML parser recurses per
/// level, so a deeper document is refused before it can exhaust the stack.
pub const MAX_XML_DEPTH: usize = 128;

/// Keyframes read per parameter.
const MAX_KEYFRAMES: usize = 100_000;

/// Synthetic media codes (`Media/FilePath`).
const BLACK_VIDEO: u32 = 1_112_293_707;
const TRANSPARENT_VIDEO: u32 = 1_414_680_150;
const COLOR_MATTE: u32 = 1_129_270_354;
const GRAPHIC: u32 = 1_196_574_294;
const CAPTION: u32 = 1_396_920_390;
const TRANSCRIPT: u32 = 1_398_035_014;

/// What [`import`] returns.
#[derive(Clone, Debug)]
pub struct PrprojImport {
    pub imported: Imported,
    pub report: Report,
    /// Media items whose project-relative path (resolved against `base_dir`) differs from the
    /// absolute path they were given. Premiere looks there when the absolute path is missing (a
    /// project moved together with its media, or opened on another computer); so should the
    /// caller, which can see the filesystem.
    pub relative_paths: Vec<(ItemId, String)>,
}

fn parse_err(message: impl Into<String>) -> Error {
    Error::Parse { format: FORMAT, message: message.into() }
}

// ---------------------------------------------------------------------------------------------
// gzip (RFC 1952) around raw DEFLATE
// ---------------------------------------------------------------------------------------------

/// The DEFLATE payload of a gzip member (header skipped), if `b` starts with a valid header.
fn gzip_body(b: &[u8]) -> Option<&[u8]> {
    if b.get(..3)? != [0x1f, 0x8b, 8] {
        return None;
    }
    let flags = *b.get(3)?;
    if flags & 0xe0 != 0 {
        return None;
    }
    let mut at = 10usize;
    if flags & 0x04 != 0 {
        let xlen = usize::from(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]));
        at = at.checked_add(2)?.checked_add(xlen)?;
    }
    for flag in [0x08, 0x10] {
        if flags & flag != 0 {
            let end = b.get(at..)?.iter().position(|&c| c == 0)?;
            at = at.checked_add(end)?.checked_add(1)?;
        }
    }
    if flags & 0x02 != 0 {
        at = at.checked_add(2)?;
    }
    b.get(at..)
}

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in data {
        c = CRC_TABLE[((c ^ u32::from(x)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

fn gunzip(b: &[u8], report: &mut Report) -> Result<Vec<u8>> {
    let body = gzip_body(b).ok_or_else(|| parse_err("not a gzip stream"))?;
    let out = miniz_oxide::inflate::decompress_to_vec_with_limit(body, MAX_XML_BYTES).map_err(|e| {
        if e.status == miniz_oxide::inflate::TINFLStatus::HasMoreOutput {
            parse_err(format!("the project is larger than {} MB uncompressed", MAX_XML_BYTES >> 20))
        } else {
            parse_err("the compressed data is damaged or incomplete")
        }
    })?;
    // Trailer: CRC-32 and size of the uncompressed data (single-member files, as Premiere writes).
    let trailer = b.len().checked_sub(8).and_then(|at| b.get(at..));
    if let Some(t) = trailer
        && let (Some(crc), Some(size)) = (t.get(..4), t.get(4..8))
    {
        let crc = u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]);
        let size = u32::from_le_bytes([size[0], size[1], size[2], size[3]]);
        if size != out.len() as u32 || crc != crc32(&out) {
            report.warn("the project file's checksum does not match its contents; it may be damaged");
        }
    }
    Ok(out)
}

/// The deepest element nesting in `xml`, counting stops once it passes `limit`. Exact for the
/// well-formed prefix of a document, which is all the parser gets through: quotes, comments, CDATA
/// and processing instructions are skipped like the parser does.
fn nesting_depth(xml: &[u8], limit: usize) -> usize {
    let find = |from: usize, pat: &[u8]| xml.get(from..).and_then(|s| s.windows(pat.len()).position(|w| w == pat)).map(|p| from + p + pat.len());
    let (mut depth, mut max, mut i) = (0usize, 0usize, 0usize);
    while let Some(at) = xml.get(i..).and_then(|s| s.iter().position(|&c| c == b'<')).map(|p| i + p) {
        i = match xml.get(at + 1) {
            None => break,
            Some(b'/') => {
                depth = depth.saturating_sub(1);
                at + 2
            }
            Some(b'?') => find(at + 2, b"?>").unwrap_or(xml.len()),
            Some(b'!') if xml.get(at..at + 4) == Some(b"<!--") => find(at + 4, b"-->").unwrap_or(xml.len()),
            Some(b'!') if xml.get(at..at + 9) == Some(b"<![CDATA[") => find(at + 9, b"]]>").unwrap_or(xml.len()),
            Some(b'!') => find(at + 2, b">").unwrap_or(xml.len()),
            Some(_) => {
                // a start tag, to its '>' outside quoted attribute values
                let (mut j, mut quote, mut self_closing) = (at + 1, None, false);
                while let Some(&c) = xml.get(j) {
                    match quote {
                        Some(q) if c == q => quote = None,
                        Some(_) => {}
                        None if c == b'"' || c == b'\'' => quote = Some(c),
                        None if c == b'>' => {
                            self_closing = xml.get(j - 1) == Some(&b'/');
                            break;
                        }
                        None => {}
                    }
                    j += 1;
                }
                if !self_closing {
                    depth += 1;
                    max = max.max(depth);
                    if max > limit {
                        return max;
                    }
                }
                j + 1
            }
        };
    }
    max
}

fn looks_like_premiere(head: &[u8]) -> bool {
    let s = String::from_utf8_lossy(head);
    let s = s.trim_start_matches('\u{feff}').trim_start();
    s.starts_with('<') && s.contains("<PremiereData")
}

/// Whether `bytes` are a Premiere Pro project (gzip-compressed, or plain `<PremiereData>` XML).
pub fn sniff(bytes: &[u8]) -> bool {
    match gzip_body(bytes) {
        Some(body) => {
            let head = match miniz_oxide::inflate::decompress_to_vec_with_limit(body, 4096) {
                Ok(v) => v,
                Err(e) => e.output,
            };
            looks_like_premiere(&head)
        }
        None => looks_like_premiere(bytes.get(..bytes.len().min(4096)).unwrap_or_default()),
    }
}

// ---------------------------------------------------------------------------------------------
// Small decoders
// ---------------------------------------------------------------------------------------------

fn base64(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

fn num(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn boolean(s: &str) -> Option<bool> {
    match s.trim() {
        t if t.eq_ignore_ascii_case("true") => Some(true),
        t if t.eq_ignore_ascii_case("false") => Some(false),
        t => num(t).map(|v| v != 0.0),
    }
}

fn point(s: &str) -> Option<(f64, f64)> {
    let (x, y) = s.split_once(':')?;
    Some((num(x)?, num(y)?))
}

/// A colour parameter: 16 bits per channel, `AAAA RRRR GGGG BBBB`, holding 8-bit values in the
/// high byte (opaque black is `0xFF00_0000_0000_0000`).
fn color(s: &str) -> Option<[f32; 4]> {
    let v = s.trim().parse::<u64>().ok()?;
    let ch = |shift: u32| (((v >> shift) & 0xffff) as f32 / 65280.0).min(1.0);
    Some([ch(32), ch(16), ch(0), ch(48)])
}

/// A frame rate from ticks per frame.
fn rate_from_ticks(tpf: i64) -> Option<FrameRate> {
    if tpf <= 0 {
        return None;
    }
    let r = FrameRate::new(TICKS_PER_SECOND, tpf);
    (0.5..=1000.0).contains(&r.as_f64()).then_some(r)
}

/// A sample rate from ticks per sample.
fn sample_rate_from_ticks(tps: i64) -> Option<u32> {
    if tps <= 0 {
        return None;
    }
    let sr = (TICKS_PER_SECOND as f64 / tps as f64).round();
    (1000.0..=768_000.0).contains(&sr).then_some(sr as u32)
}

/// `"0,0,1920,1080"` → (1920, 1080), if it is a frame size FilmCraft can work with.
fn parse_rect(s: &str) -> Option<(u32, u32)> {
    let v: Vec<i64> = s.split(',').map(|p| p.trim().parse::<i64>().ok()).collect::<Option<_>>()?;
    let [l, t, r, b] = v.as_slice() else { return None };
    let w = u32::try_from(r.checked_sub(*l)?).ok()?;
    let h = u32::try_from(b.checked_sub(*t)?).ok()?;
    validate_frame_size(w, h).is_ok().then_some((w, h))
}

/// Channels in an `AudioChannelLayout` (`[{"channellabel":100},{"channellabel":101}]`).
fn channel_count(json: &str) -> Option<u32> {
    let v: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    u32::try_from(v.len()).ok().filter(|n| (1..=64).contains(n))
}

/// `BE.Prefs.LabelColors.N` → the N-th label (Premiere's Label menu order, which is ours).
fn label_of(s: Option<&str>) -> Option<Label> {
    let n = s?.trim().strip_prefix("BE.Prefs.LabelColors.")?.parse::<usize>().ok()?;
    Label::ALL.get(n).copied()
}

/// A marker colour (`0xAABBGGRR`) → the nearest marker label.
fn marker_label(c: u64) -> Label {
    let rgb = [(c & 0xff) as i32, ((c >> 8) & 0xff) as i32, ((c >> 16) & 0xff) as i32];
    let dist = |p: [u8; 3]| (0..3).map(|i| (rgb[i] - i32::from(p[i])).pow(2)).sum::<i32>();
    let mut best = (Label::Green, dist(Label::Green.marker_rgb()));
    for l in [Label::Rose, Label::Purple, Label::Mango, Label::Yellow, Label::Blue, Label::Teal] {
        let d = dist(l.marker_rgb());
        if d < best.1 {
            best = (l, d);
        }
    }
    // White markers are Lavender, as in the EDL and OTIO importers.
    if dist([255, 255, 255]) < best.1 { Label::Lavender } else { best.0 }
}

fn transition_alias(name: &str) -> &str {
    let n = name.trim();
    let n = n.strip_prefix("FI: ").unwrap_or(n);
    let n = n.strip_suffix(" (Legacy)").unwrap_or(n);
    match n {
        "Push Impacts" => "Push",
        _ => n,
    }
}

// ---------------------------------------------------------------------------------------------
// Parameters and keyframes
// ---------------------------------------------------------------------------------------------

struct Key<'a> {
    time: i64,
    raw: &'a str,
    interp: Interpolation,
    in_influence: f64,
    out_influence: f64,
}

/// One Premiere effect parameter, values still as text.
struct PParam<'a> {
    pid: Option<u32>,
    name: &'a str,
    value: Option<&'a str>,
    keys: Vec<Key<'a>>,
    /// More than [`MAX_KEYFRAMES`] keyframes: the rest were dropped.
    truncated: bool,
}

fn interpolation(kind: &str, sub: &str) -> Interpolation {
    match (kind.trim(), sub.trim()) {
        ("4", _) => Interpolation::Hold,
        ("5", "4") => Interpolation::AutoBezier,
        ("5", "2") => Interpolation::ContinuousBezier,
        ("5", _) => Interpolation::Bezier,
        _ => Interpolation::Linear,
    }
}

fn influence(s: Option<&str>) -> f64 {
    s.and_then(num).filter(|v| *v > 0.0 && *v <= 1.0).unwrap_or(1.0 / 3.0)
}

/// The keyframes of a `Keyframes` value, at most [`MAX_KEYFRAMES`]; `true` when more were there.
fn parse_keys(text: &str) -> (Vec<Key<'_>>, bool) {
    let mut out = Vec::new();
    let mut entries = text.split(';').filter(|k| !k.trim().is_empty());
    for k in entries.by_ref().take(MAX_KEYFRAMES) {
        let f: Vec<&str> = k.split(',').collect();
        let (Some(t), Some(raw)) = (f.first().and_then(|t| t.trim().parse::<i64>().ok()), f.get(1)) else { continue };
        if !(Tick::MIN.0..=Tick::MAX.0).contains(&t) {
            continue;
        }
        out.push(Key {
            time: t,
            raw,
            interp: interpolation(f.get(2).copied().unwrap_or("0"), f.get(3).copied().unwrap_or("0")),
            in_influence: influence(f.get(5).copied()),
            out_influence: influence(f.get(7).copied()),
        });
    }
    let truncated = entries.next().is_some();
    (out, truncated)
}

fn parse_param<'a>(p: Node<'a, '_>) -> PParam<'a> {
    let value = child_text(p, "StartKeyframe").and_then(|s| s.split(',').nth(1)).or_else(|| child_text(p, "CurrentValue"));
    let (keys, truncated) = match (child_bool(p, "IsTimeVarying"), child_text(p, "Keyframes")) {
        (Some(false), _) | (_, None) => (Vec::new(), false),
        (_, Some(k)) => parse_keys(k),
    };
    PParam { pid: child_text(p, "ParameterID").and_then(|s| s.parse().ok()), name: child_text(p, "Name").unwrap_or(""), value, keys, truncated }
}

/// A FilmCraft parameter from a Premiere one. `conv` turns a value into ours; keyframe times move
/// back by `offset` (the media time a synthetic clip's first frame had in Premiere).
fn to_param(p: &PParam, offset: i64, conv: impl Fn(&str) -> Option<ParamValue>) -> Option<Param> {
    let first = p.value.and_then(&conv).or_else(|| p.keys.first().and_then(|k| conv(k.raw)))?;
    let mut out = Param::new(first);
    for k in &p.keys {
        let (Some(v), Some(t)) = (conv(k.raw), k.time.checked_sub(offset)) else { continue };
        let mut kf = Keyframe::new(Tick(t), v);
        kf.interp = k.interp;
        kf.in_influence = k.in_influence;
        kf.out_influence = k.out_influence;
        out.keyframes.push(kf);
    }
    out.keyframes.sort_by_key(|k| k.time);
    out.keyframes.dedup_by_key(|k| k.time);
    if let Some(k) = out.keyframes.first() {
        out.value = k.value.clone();
    }
    Some(out)
}

/// Farthest a Motion point may be from the frame origin, in pixels (frames are at most 32 768
/// wide; a point farther out is damage, not an edit).
const MAX_POINT_PX: f64 = 1e7;

/// A point in pixels, if it is finite and within [`MAX_POINT_PX`]: a huge fraction from the file
/// times the frame size could otherwise overflow to infinity and poison the transform maths.
fn pixels(x: f64, y: f64) -> Option<ParamValue> {
    let ok = |v: f64| v.is_finite() && v.abs() <= MAX_POINT_PX;
    (ok(x) && ok(y)).then(|| ParamValue::Vec2(Vec2::new(x, y)))
}

/// A number clamped to the range our `effect`'s `param` takes (any finite number when it has no
/// range, e.g. an angle).
fn float_in(effect: &str, param: &str) -> impl Fn(&str) -> Option<ParamValue> {
    let (lo, hi) = match find_effect(effect).and_then(|d| d.param(param)).map(|p| &p.kind) {
        Some(ParamKind::Float { min, max, .. }) => (*min, *max),
        _ => (-MAX_POINT_PX, MAX_POINT_PX),
    };
    move |s: &str| num(s).map(|v| ParamValue::Float(v.clamp(lo, hi)))
}

fn is_centre(p: &PParam) -> bool {
    p.keys.is_empty() && p.value.and_then(point).is_none_or(|(x, y)| (x - 0.5).abs() < 1e-6 && (y - 0.5).abs() < 1e-6)
}

fn is_zero(p: &PParam) -> bool {
    p.keys.is_empty() && p.value.and_then(num).is_none_or(|v| v.abs() < 1e-9)
}

/// Volume levels: linear amplitude where 1.0 is +15 dB (0 dB = 0.177 827 941).
fn level_db(s: &str, max: f64) -> Option<ParamValue> {
    let v = num(s)?;
    let db = if v <= 1e-15 { -287.5 } else { 20.0 * v.log10() + 15.0 };
    Some(ParamValue::Float(db.clamp(-287.5, max)))
}

/// How a Premiere parameter maps onto one of ours.
#[derive(Clone, Copy)]
enum Conv {
    /// Number, times a scale, clamped to our range.
    Float(f64),
    Bool,
    Choice,
    Color,
}

const LUMETRI: &[(u32, &str, Conv)] = &[
    (7, "temperature", Conv::Float(1.0)),
    (8, "tint", Conv::Float(1.0)),
    (11, "exposure", Conv::Float(1.0)),
    (12, "contrast", Conv::Float(1.0)),
    (13, "highlights", Conv::Float(1.0)),
    (14, "shadows", Conv::Float(1.0)),
    (15, "whites", Conv::Float(1.0)),
    (16, "blacks", Conv::Float(1.0)),
    (20, "saturation", Conv::Float(1.0)),
    (26, "look_intensity", Conv::Float(1.0)),
    (28, "faded_film", Conv::Float(1.0)),
    (29, "sharpen", Conv::Float(1.0)),
    (30, "vibrance", Conv::Float(1.0)),
    (31, "creative_sat", Conv::Float(1.0)),
];
const CROP: &[(u32, &str, Conv)] = &[
    (1, "left", Conv::Float(1.0)),
    (2, "top", Conv::Float(1.0)),
    (3, "right", Conv::Float(1.0)),
    (4, "bottom", Conv::Float(1.0)),
    (5, "zoom", Conv::Bool),
    (6, "feather", Conv::Float(1.0)),
];
const DROP_SHADOW: &[(u32, &str, Conv)] = &[
    (1, "color", Conv::Color),
    // stored 0–255
    (2, "opacity", Conv::Float(100.0 / 255.0)),
    (3, "direction", Conv::Float(1.0)),
    (4, "distance", Conv::Float(1.0)),
    (5, "softness", Conv::Float(1.0)),
    (6, "only", Conv::Bool),
];
const ULTRA_KEY: &[(u32, &str, Conv)] = &[
    (1, "output", Conv::Choice),
    (2, "setting", Conv::Choice),
    (3, "key_color", Conv::Color),
    (5, "transparency", Conv::Float(1.0)),
    (6, "highlight", Conv::Float(1.0)),
    (7, "shadow", Conv::Float(1.0)),
    (8, "tolerance", Conv::Float(1.0)),
    (9, "pedestal", Conv::Float(1.0)),
    (12, "choke", Conv::Float(1.0)),
    (13, "soften", Conv::Float(1.0)),
    (20, "spill", Conv::Float(1.0)),
];
const LUMA_KEY: &[(u32, &str, Conv)] = &[(1, "threshold", Conv::Float(1.0)), (2, "cutoff", Conv::Float(1.0))];
const POSTERIZE_TIME: &[(u32, &str, Conv)] = &[(1, "rate", Conv::Float(1.0))];

/// Effects mapped parameter by parameter: (Premiere match name, our effect id, parameters).
const MAPPED: &[(&str, &str, &[(u32, &str, Conv)])] = &[
    ("AE.ADBE Lumetri", "lumetri", LUMETRI),
    ("AE.ADBE AECrop", "crop", CROP),
    ("AE.ADBE Drop Shadow", "drop_shadow", DROP_SHADOW),
    ("AE.ADBE Ultra Key", "ultra_key", ULTRA_KEY),
    ("AE.ADBE Legacy Key Luma", "luma_key", LUMA_KEY),
    ("AE.ADBE Posterize Time", "posterize_time", POSTERIZE_TIME),
];

// ---------------------------------------------------------------------------------------------
// Object graph
// ---------------------------------------------------------------------------------------------

struct Graph<'a, 'i> {
    root: Node<'a, 'i>,
    by_id: HashMap<&'a str, Node<'a, 'i>>,
    by_uid: HashMap<&'a str, Node<'a, 'i>>,
}

impl<'a, 'i> Graph<'a, 'i> {
    fn new(root: Node<'a, 'i>) -> Self {
        let (mut by_id, mut by_uid) = (HashMap::new(), HashMap::new());
        for c in root.children().filter(|c| c.is_element()) {
            if let Some(id) = c.attribute("ObjectID") {
                by_id.entry(id).or_insert(c);
            }
            if let Some(uid) = c.attribute("ObjectUID") {
                by_uid.entry(uid).or_insert(c);
            }
        }
        Self { root, by_id, by_uid }
    }

    /// The object a reference element points at.
    fn deref(&self, r: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
        if let Some(id) = r.attribute("ObjectRef") {
            return self.by_id.get(id).copied();
        }
        r.attribute("ObjectURef").and_then(|u| self.by_uid.get(u).copied())
    }

    /// Follow the reference element at `path` below `n`.
    fn follow(&self, n: Node<'a, 'i>, path: &[&str]) -> Option<Node<'a, 'i>> {
        self.deref(at(n, path)?)
    }

    /// The objects a list element (`<Items><Item ObjectURef=…/>…</Items>`) points at.
    fn list(&self, list: Option<Node<'a, 'i>>, item: &'static str) -> Vec<Node<'a, 'i>> {
        list.map(|l| children(l, item).filter_map(|r| self.deref(r)).collect()).unwrap_or_default()
    }
}

/// The element at `path` below `n`.
fn at<'a, 'i>(n: Node<'a, 'i>, path: &[&str]) -> Option<Node<'a, 'i>> {
    let mut cur = n;
    for p in path {
        cur = child(cur, p)?;
    }
    Some(cur)
}

fn key<'a>(n: Node<'a, '_>) -> Option<&'a str> {
    n.attribute("ObjectUID").or_else(|| n.attribute("ObjectID"))
}

/// What a report calls an audio filter: its display name, else what its match name tells
/// (`Internal Lowpass`, `2:<code> <plug-in name>`); third-party ids alone tell nothing.
fn audio_filter_name(comp: Node, core: Option<Node>) -> Option<String> {
    if let Some(n) = core.and_then(|c| child_text(c, "DisplayName")).filter(|n| !n.is_empty()) {
        return Some(n.to_string());
    }
    let fm = child_text(comp, "FilterMatchName").unwrap_or("");
    // built-in effects whose ids projects pair with these display names
    const KNOWN: [(&str, &str); 2] =
        [("8edcd326-33de-4a4d-985b-f1bf48ed1427", "Parametric Equalizer"), ("2da3e2bc-5005-481c-bbeb-7689d3169edf", "Single-band Compressor")];
    if let Some((_, n)) = KNOWN.iter().find(|(id, _)| *id == fm) {
        return Some((*n).to_string());
    }
    fm.strip_prefix("Internal ").or_else(|| fm.strip_prefix("2:").and_then(|r| r.split_once(' ')).map(|(_, n)| n)).filter(|n| !n.is_empty()).map(str::to_string)
}

/// The element holding a clip object's properties: a `VideoClip` / `AudioClip` itself, or the
/// `AudioClip` a `RemixClip` wraps.
fn clip_body<'a, 'i>(clip: Node<'a, 'i>) -> Node<'a, 'i> {
    if child(clip, "Clip").is_some() {
        return clip;
    }
    child(clip, "AudioClip").or_else(|| child(clip, "VideoClip")).unwrap_or(clip)
}

enum Source<'a, 'i> {
    Media(Node<'a, 'i>),
    Sequence(Node<'a, 'i>),
}

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

/// Import a Premiere Pro project. `opts.base_dir` (the project's directory) resolves relative
/// media paths; `opts.name` names the project.
pub fn import(bytes: &[u8], opts: &ImportOptions) -> Result<PrprojImport> {
    let mut report = Report::default();
    let xml = if gzip_body(bytes).is_some() {
        gunzip(bytes, &mut report)?
    } else if looks_like_premiere(bytes.get(..bytes.len().min(4096)).unwrap_or_default()) {
        if bytes.len() > MAX_XML_BYTES {
            return Err(parse_err(format!("the project is larger than {} MB", MAX_XML_BYTES >> 20)));
        }
        bytes.to_vec()
    } else {
        return Err(parse_err("not a Premiere Pro project"));
    };
    // Premiere writes UTF-8. Invalid bytes mean a damaged file: refuse it rather than copying
    // a possibly huge document into a lossy (up to three times larger) one.
    let text = String::from_utf8(xml)
        .map_err(|e| parse_err(format!("the project is not valid UTF-8 text at byte {} (the file is damaged)", e.utf8_error().valid_up_to())))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    if nesting_depth(text.as_bytes(), MAX_XML_DEPTH) > MAX_XML_DEPTH {
        return Err(parse_err(format!("elements are nested more than {MAX_XML_DEPTH} deep (the file is damaged)")));
    }
    let parse = roxmltree::ParsingOptions { allow_dtd: false, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(text, parse).map_err(|e| parse_err(e.to_string()))?;
    let root = doc.root_element();
    if root.tag_name().name() != "PremiereData" {
        return Err(parse_err("not a Premiere Pro project (no <PremiereData>)"));
    }
    let name = opts.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "Premiere Pro Project".into());
    let mut imp = Imp {
        g: Graph::new(root),
        b: Builder::new(&name),
        report,
        base_dir: opts.base_dir.clone(),
        masters: HashMap::new(),
        master_media: HashMap::new(),
        media: HashMap::new(),
        seqs: HashMap::new(),
        pending: Vec::new(),
        visited: HashSet::new(),
        synthetic: HashSet::new(),
        relative: Vec::new(),
        paths: HashMap::new(),
        source_markers: HashMap::new(),
        graphic_clips: 0,
    };
    imp.run()?;
    let Imp { b, report, relative, .. } = imp;
    Ok(PrprojImport { imported: b.finish(), report, relative_paths: relative })
}

struct Imp<'a, 'i> {
    g: Graph<'a, 'i>,
    b: Builder,
    report: Report,
    base_dir: Option<String>,
    /// MasterClip uid → project item.
    masters: HashMap<&'a str, ItemId>,
    /// MasterClip uid → the uid of the media it shows.
    master_media: HashMap<&'a str, &'a str>,
    /// Media uid → the first project item made from it.
    media: HashMap<&'a str, ItemId>,
    /// Sequence uid → sequence item.
    seqs: HashMap<&'a str, ItemId>,
    /// Sequences still to fill, in the order they were found.
    pending: Vec<(Node<'a, 'i>, ItemId)>,
    /// Project items already walked (guards against a bin that contains itself).
    visited: HashSet<&'a str>,
    /// Items whose clips start at media time zero here (generators, adjustment layers, graphics,
    /// stills), although Premiere gives their media an arbitrary origin.
    synthetic: HashSet<ItemId>,
    relative: Vec<(ItemId, String)>,
    /// Media file path → the first item made for it.
    paths: HashMap<String, ItemId>,
    /// Item → the Markers object holding its source markers (clip instances share it).
    source_markers: HashMap<ItemId, &'a str>,
    graphic_clips: usize,
}

impl<'a, 'i> Imp<'a, 'i> {
    fn run(&mut self) -> Result<()> {
        let root_item = self.g.root.children().find(|c| c.is_element() && c.tag_name().name() == "RootProjectItem").ok_or(Error::Empty)?;
        self.walk(root_item, None, 0);
        let mut i = 0;
        while let Some(&(node, id)) = self.pending.get(i) {
            self.fill_sequence(node, id);
            i += 1;
        }
        self.break_cycles();
        self.grow_synthetic();
        let order: Vec<ItemId> = self.pending.iter().map(|(_, id)| *id).collect();
        let nested: HashSet<ItemId> = order.iter().flat_map(|s| self.nested_in(*s)).collect();
        self.b.top = order.iter().copied().filter(|s| !nested.contains(s)).collect();
        if self.b.top.is_empty() {
            self.b.top = order;
        }
        if self.graphic_clips > 0 {
            self.report
                .warn(format!("{} graphic clip(s) were imported without their text and shapes (Essential Graphics are not supported yet)", self.graphic_clips));
        }
        Ok(())
    }

    // ---- project panel ----

    fn walk(&mut self, owner: Node<'a, 'i>, bin: Option<BinId>, depth: usize) {
        if depth > MAX_BIN_DEPTH {
            self.report.warn(format!("bins nested deeper than {MAX_BIN_DEPTH} levels were not imported"));
            return;
        }
        for obj in self.g.list(at(owner, &["ProjectItemContainer", "Items"]), "Item") {
            let Some(k) = key(obj) else { continue };
            if !self.visited.insert(k) {
                continue;
            }
            match obj.tag_name().name() {
                "BinProjectItem" => {
                    let name = path_text(obj, &["ProjectItem", "Name"]).filter(|n| !n.is_empty()).unwrap_or("Bin");
                    let id = self.b.bin(name, bin);
                    self.walk(obj, Some(id), depth + 1);
                }
                "ClipProjectItem" => self.project_item(obj, bin),
                other => self.report.info(format!("project items of type {other} were skipped")),
            }
        }
    }

    fn project_item(&mut self, obj: Node<'a, 'i>, bin: Option<BinId>) {
        let item_name = path_text(obj, &["ProjectItem", "Name"]).filter(|n| !n.is_empty()).unwrap_or("Untitled");
        let label = label_of(path_text(obj, &["ProjectItem", "Node", "Properties", "Column.PropertyText.Label"]));
        let Some(mc) = self.g.follow(obj, &["MasterClip"]) else {
            self.report.warn(format!("project item \"{item_name}\" has no master clip and was skipped"));
            return;
        };
        // The master clip's name is the current one: the project item keeps the name it was
        // created with (a sequence made from a clip, footage that was replaced).
        let name = child_text(mc, "Name").filter(|n| !n.is_empty()).unwrap_or(item_name);
        let Some(uid) = mc.attribute("ObjectUID") else { return };
        if self.masters.contains_key(uid) {
            return;
        }
        let id = match self.master_source(mc) {
            Some(Source::Sequence(seq)) => match seq.attribute("ObjectUID").and_then(|s| self.seqs.get(s)) {
                Some(&id) => id,
                None => self.reserve_sequence(seq, name, label, bin),
            },
            Some(Source::Media(media)) => match self.media_item(media, Some(mc), name, label, bin) {
                Some(id) => {
                    if let Some(m) = media.attribute("ObjectUID") {
                        self.master_media.insert(uid, m);
                    }
                    id
                }
                None => return,
            },
            None => {
                self.report.info(format!("project item \"{name}\" has no media or sequence and was skipped"));
                return;
            }
        };
        self.masters.insert(uid, id);
    }

    /// The clips of a master clip (video first).
    fn master_clips(&self, mc: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
        let mut clips = self.g.list(at(mc, &["Clips"]), "Clip");
        clips.sort_by_key(|c| match c.tag_name().name() {
            "VideoClip" => 0,
            "AudioClip" | "RemixClip" => 1,
            _ => 2,
        });
        clips
    }

    fn clip_source(&self, clip: Node<'a, 'i>) -> Option<Source<'a, 'i>> {
        let src = self.g.follow(clip_body(clip), &["Clip", "Source"])?;
        let tag = src.tag_name().name();
        if tag.ends_with("SequenceSource") {
            self.g.follow(src, &["SequenceSource", "Sequence"]).map(Source::Sequence)
        } else if tag.ends_with("MediaSource") {
            self.g.follow(src, &["MediaSource", "Media"]).map(Source::Media)
        } else {
            None
        }
    }

    fn master_source(&self, mc: Node<'a, 'i>) -> Option<Source<'a, 'i>> {
        self.master_clips(mc).into_iter().filter(|c| matches!(c.tag_name().name(), "VideoClip" | "AudioClip" | "RemixClip")).find_map(|c| self.clip_source(c))
    }

    /// The file a media object names (none for synthetic media).
    fn media_path(&self, media: Node<'a, 'i>) -> Option<String> {
        let fp = child_text(media, "FilePath").filter(|p| !p.is_empty()).or_else(|| child_text(media, "ActualMediaFilePath"))?.trim();
        if fp.is_empty() || fp.parse::<u32>().is_ok() {
            return None;
        }
        Some(if is_absolute(fp) { fp.to_string() } else { resolve_path(fp, self.base_dir.as_deref()) })
    }

    /// The media item already made for the file `media` names (Premiere can hold one file in
    /// several media objects, e.g. one per stream).
    fn file_item(&self, media: Node<'a, 'i>) -> Option<ItemId> {
        self.paths.get(&self.media_path(media)?).copied()
    }

    /// What the media's streams say about it.
    fn media_spec(&self, media: Node<'a, 'i>, path: &str) -> MediaSpec {
        let mut spec = MediaSpec::default();
        let mut duration = 0i64;
        let mut still = kind_from_ext(path) == MediaKind::Still;
        if let Some(v) = self.g.follow(media, &["VideoStream"]) {
            still |= child_bool(v, "IsStill") == Some(true);
            if let Some(d) = child_i64(v, "Duration").filter(|d| *d > 0) {
                duration = duration.max(d);
            }
            if let Some((w, h)) = child_text(v, "FrameRect").and_then(parse_rect) {
                let rate = child_i64(v, "FrameRate").and_then(rate_from_ticks).unwrap_or(FrameRate::FPS_29_97);
                spec.video = Some((w, h, rate));
            }
        }
        if let Some(a) = self.g.follow(media, &["AudioStream"]) {
            if let Some(d) = child_i64(a, "Duration").filter(|d| *d > 0) {
                duration = duration.max(d);
            }
            let sr = child_i64(a, "FrameRate").and_then(sample_rate_from_ticks).unwrap_or(48_000);
            spec.audio = Some((sr, child_text(a, "AudioChannelLayout").and_then(channel_count).unwrap_or(2)));
        }
        spec.kind = Some(if still {
            MediaKind::Still
        } else if spec.video.is_some() {
            MediaKind::Movie
        } else if spec.audio.is_some() {
            MediaKind::AudioOnly
        } else {
            kind_from_ext(path)
        });
        if !still && duration > 0 {
            spec.duration = Some(Tick(duration.min(Tick::MAX.0)));
        }
        spec
    }

    /// The range a master clip's own clip spans (its In/Out in the Source monitor).
    fn master_range(&self, mc: Option<Node<'a, 'i>>) -> Option<(i64, i64)> {
        let c = self.master_clips(mc?).into_iter().next()?;
        let c = child(clip_body(c), "Clip")?;
        let (i, o) = (child_i64(c, "InPoint").unwrap_or(0), child_i64(c, "OutPoint")?);
        (o > i && i >= 0 && o <= Tick::MAX.0).then_some((i, o))
    }

    fn media_item(&mut self, media: Node<'a, 'i>, mc: Option<Node<'a, 'i>>, name: &str, label: Option<Label>, bin: Option<BinId>) -> Option<ItemId> {
        let fp = child_text(media, "FilePath").filter(|p| !p.is_empty()).or_else(|| child_text(media, "ActualMediaFilePath")).unwrap_or("");
        let spec = self.media_spec(media, fp);
        let range = self.master_range(mc);
        let five_s = Tick(5 * TICKS_PER_SECOND);
        let range_len = range.map(|(i, o)| Tick(o - i)).unwrap_or(five_s);
        let (w, h, rate) = spec.video.unwrap_or((1920, 1080, FrameRate::FPS_29_97));
        let id = if mc.and_then(|m| child_bool(m, "IsAdjustmentLayer")) == Some(true) {
            let kind = ItemKind::AdjustmentLayer { width: w, height: h, rate, duration: range_len };
            let id = self.b.p.add_item(name, label.unwrap_or(Label::Lavender), kind, bin);
            self.synthetic.insert(id);
            id
        } else if let Ok(code) = fp.trim().parse::<u32>() {
            let g = match code {
                CAPTION | TRANSCRIPT => return None,
                GRAPHIC => {
                    let id = self.b.p.add_item(name, label.unwrap_or(Label::Rose), ItemKind::Graphic { width: w, height: h, rate }, bin);
                    self.synthetic.insert(id);
                    return self.register(media, id);
                }
                BLACK_VIDEO => Generator::BlackVideo,
                TRANSPARENT_VIDEO => Generator::TransparentVideo,
                COLOR_MATTE => {
                    let prefs = child_text(media, "ImporterPrefs").map(base64).unwrap_or_default();
                    let c = |i: usize| f32::from(prefs.get(i).copied().unwrap_or(0)) / 255.0;
                    Generator::ColorMatte { color: [c(0), c(1), c(2), 1.0] }
                }
                _ => {
                    let title = child_text(media, "Title").unwrap_or("").to_ascii_lowercase();
                    if title.contains("bars") {
                        Generator::BarsAndTone
                    } else if title.contains("leader") {
                        Generator::CountingLeader
                    } else {
                        self.report.warn(format!("synthetic media \"{name}\" is not supported; it was imported as Black Video"));
                        Generator::BlackVideo
                    }
                }
            };
            let mut spec = spec;
            spec.kind = Some(MediaKind::Synthetic);
            spec.duration = Some(range_len);
            spec.video = Some((w, h, rate));
            let info = media_info(name, &spec);
            let id = self.b.p.add_item(name, label.unwrap_or(Label::Lavender), ItemKind::Media(media_clip(MediaRef::Generator(g), info)), bin);
            self.synthetic.insert(id);
            id
        } else {
            let Some(path) = self.media_path(media) else {
                self.report.warn(format!("project item \"{name}\" has no file path and was skipped"));
                return None;
            };
            let default_label = if spec.video.is_none() && spec.audio.is_some() { Label::Caribbean } else { Label::Iris };
            let mut clip = media_clip(MediaRef::File { path: path.clone() }, media_info(name, &spec));
            if let Some((i, o)) = range {
                let full = clip.info.duration;
                if i > 0 {
                    clip.mark_in = Some(Tick(i));
                }
                if full > Tick::ZERO && Tick(o) < full {
                    clip.mark_out = Some(Tick(o));
                }
            }
            let id = self.b.p.add_item(name, label.unwrap_or(default_label), ItemKind::Media(clip), bin);
            self.paths.entry(path.clone()).or_insert(id);
            // Premiere gives stills a media time origin of about an hour; ours start at zero.
            if spec.kind == Some(MediaKind::Still) {
                self.synthetic.insert(id);
            }
            if let Some(rel) = child_text(media, "RelativePath").filter(|r| !r.is_empty()) {
                let alt = resolve_path(rel, self.base_dir.as_deref());
                if alt != path && self.base_dir.is_some() {
                    self.relative.push((id, alt));
                }
            }
            id
        };
        // source markers
        if let Some(c) = mc.and_then(|m| self.master_clips(m).into_iter().next())
            && let Some(mk) = self.g.follow(clip_body(c), &["Clip", "MarkerOwner", "Markers"])
        {
            let offset = if self.synthetic.contains(&id) { range.map(|r| r.0).unwrap_or(0) } else { 0 };
            let markers = self.markers(Some(mk), offset);
            if let Some(m) = self.b.p.item_mut(id).and_then(|i| i.as_media_mut()) {
                m.markers = markers;
            }
            if let Some(k) = mk.attribute("ObjectID") {
                self.source_markers.insert(id, k);
            }
        }
        self.register(media, id)
    }

    fn register(&mut self, media: Node<'a, 'i>, id: ItemId) -> Option<ItemId> {
        if let Some(m) = media.attribute("ObjectUID") {
            self.media.entry(m).or_insert(id);
        }
        Some(id)
    }

    // ---- sequences ----

    fn track_groups(&self, seq: Node<'a, 'i>) -> Vec<(TrackKind, Node<'a, 'i>)> {
        let Some(groups) = child(seq, "TrackGroups") else { return Vec::new() };
        children(groups, "TrackGroup")
            .filter_map(|tg| self.g.follow(tg, &["Second"]))
            .filter_map(|g| match g.tag_name().name() {
                "VideoTrackGroup" => Some((TrackKind::Video, g)),
                "AudioTrackGroup" => Some((TrackKind::Audio, g)),
                _ => None,
            })
            .collect()
    }

    fn seq_settings(&mut self, seq: Node<'a, 'i>, name: &str) -> SequenceSettings {
        let (mut rate, mut size, mut sample_rate) = (None, None, None);
        for (kind, g) in self.track_groups(seq) {
            let tick_rate = at(g, &["TrackGroup", "FrameRate"]).map(crate::xml::text).and_then(|t| t.parse::<i64>().ok());
            match kind {
                TrackKind::Video => {
                    rate = rate.or(tick_rate.and_then(rate_from_ticks));
                    size = size.or(child_text(g, "FrameRect").and_then(parse_rect));
                }
                TrackKind::Audio => sample_rate = sample_rate.or(tick_rate.and_then(sample_rate_from_ticks)),
            }
        }
        let display = path_text(seq, &["Node", "Properties", "MZ.Sequence.VideoTimeDisplayFormat"]).and_then(|s| s.parse::<i64>().ok());
        let rate = rate.unwrap_or_else(|| {
            self.report.warn(format!("sequence \"{name}\" has no usable frame rate; 29.97 fps was assumed"));
            FrameRate::FPS_29_97
        });
        let (w, h) = size.unwrap_or_else(|| {
            self.report.warn(format!("sequence \"{name}\" has no usable frame size; 1920×1080 was assumed"));
            (1920, 1080)
        });
        // Time display 102 / 106 = 29.97 / 59.94 drop-frame timecode.
        let mut s = settings_for(rate, w, h, matches!(display, Some(102 | 106)));
        if let Some(sr) = sample_rate {
            s.sample_rate = sr;
        }
        s
    }

    fn reserve_sequence(&mut self, seq: Node<'a, 'i>, name: &str, label: Option<Label>, bin: Option<BinId>) -> ItemId {
        let settings = self.seq_settings(seq, name);
        let id = self.b.reserve_sequence(name, settings, bin);
        if let (Some(l), Some(it)) = (label, self.b.p.item_mut(id)) {
            it.label = l;
        }
        if let Some(uid) = seq.attribute("ObjectUID") {
            self.seqs.insert(uid, id);
        }
        self.pending.push((seq, id));
        id
    }

    fn fill_sequence(&mut self, node: Node<'a, 'i>, id: ItemId) {
        let Some((settings, seq_name)) = self.b.p.item(id).and_then(|i| Some((i.as_sequence()?.settings.clone(), i.name.clone()))) else { return };
        let frame = (f64::from(settings.width), f64::from(settings.height));
        let mut seq = empty_sequence(settings);
        let mut by_obj: HashMap<&'a str, ClipId> = HashMap::new();
        let mut seen: HashSet<&'a str> = HashSet::new();
        for (kind, group) in self.track_groups(node) {
            for tnode in self.g.list(at(group, &["TrackGroup", "Tracks"]), "Track") {
                let idx = seq.tracks(kind).len();
                let track = self.track(tnode, kind, idx);
                seq.tracks_mut(kind).push(track);
                let ct = child(tnode, "ClipTrack");
                let (mut heads, mut tails) = (HashMap::new(), HashMap::new());
                for n in self.g.list(ct.and_then(|c| at(c, &["ClipItems", "TrackItems"])), "TrackItem") {
                    let Some(k) = n.attribute("ObjectID") else { continue };
                    if !seen.insert(k) {
                        continue;
                    }
                    let Some((ti, head, tail)) = self.track_item(n, kind, frame, &seq_name) else { continue };
                    let clip = ti.id;
                    if let Some(h) = head {
                        heads.insert(h, clip);
                    }
                    if let Some(t) = tail {
                        tails.insert(t, clip);
                    }
                    by_obj.insert(k, clip);
                    if self.b.place(&mut seq, kind, idx, ti) != idx {
                        self.report.warn(format!("sequence \"{seq_name}\": overlapping clips were moved to another track"));
                    }
                }
                for n in self.g.list(ct.and_then(|c| at(c, &["TransitionItems", "TrackItems"])), "TrackItem") {
                    let Some(k) = n.attribute("ObjectID") else { continue };
                    if !seen.insert(k) {
                        continue;
                    }
                    let Some(track) = seq.tracks(kind).get(idx) else { continue };
                    if let Some(tr) = self.transition(n, kind, track, heads.get(k).copied(), tails.get(k).copied(), &seq_name)
                        && let Some(track) = seq.tracks_mut(kind).get_mut(idx)
                    {
                        track.transitions.push(tr);
                    }
                }
            }
        }
        // links (video ↔ audio of one source) and groups
        let pgc = child(node, "PersistentGroupContainer");
        let containers = [(pgc.and_then(|p| at(p, &["LinkContainer", "Links"])), "Link", true), (pgc.and_then(|p| child(p, "Groups")), "Group", false)];
        for (list, item, is_link) in containers {
            for obj in self.g.list(list, item) {
                let clips: Vec<ClipId> = self
                    .g
                    .list(at(obj, &["TrackItemGroup", "TrackItems"]), "TrackItem")
                    .iter()
                    .filter_map(|n| n.attribute("ObjectID").and_then(|k| by_obj.get(k)).copied())
                    .collect();
                if clips.len() < 2 {
                    continue;
                }
                let tag = self.b.link_id();
                for c in clips {
                    if let Some((_, it)) = seq.find_item_mut(c) {
                        if is_link {
                            it.link = Some(tag);
                        } else {
                            it.group = Some(tag);
                        }
                    }
                }
            }
        }
        seq.markers = self.markers(self.g.follow(node, &["MarkerOwner", "Markers"]), 0);
        let prop = |n: &str| path_text(node, &["Node", "Properties", n]).and_then(|s| s.parse::<i64>().ok()).filter(|t| (0..=Tick::MAX.0).contains(t));
        seq.mark_in = prop("MZ.InPoint").map(Tick);
        seq.mark_out = prop("MZ.OutPoint").map(Tick).filter(|o| seq.mark_in.is_none_or(|i| *o > i));
        self.b.put_sequence(id, seq);
    }

    fn track(&mut self, tnode: Node<'a, 'i>, kind: TrackKind, idx: usize) -> Track {
        let mut t = self.b.track(kind, idx);
        let Some(tr) = at(tnode, &["ClipTrack", "Track"]) else { return t };
        if let Some(n) = path_text(tr, &["Node", "Properties", "MZ.TrackName"]).filter(|n| !n.is_empty()) {
            t.name = n.to_string();
        }
        t.locked = child_bool(tr, "IsLocked") == Some(true);
        t.sync_lock = child_bool(tr, "IsSyncLocked").unwrap_or(true);
        if child_bool(tr, "IsMuted") == Some(true) {
            match kind {
                TrackKind::Video => t.enabled = false,
                TrackKind::Audio => t.muted = true,
            }
        }
        if let Some(audio) = child(tnode, "AudioTrack") {
            self.audio_track(&mut t, audio);
        }
        t
    }

    /// An audio track's mixer strip: its fader (volume, where 1.0 is 0 dB, and the M button), pan
    /// and inserts.
    fn audio_track(&mut self, t: &mut Track, audio: Node<'a, 'i>) {
        if let Some(chain) = self.g.follow(audio, &["ComponentOwner", "Components"]) {
            for comp in self.components(chain) {
                let core = at(comp, &["AudioComponent", "Component"]);
                match comp.tag_name().name() {
                    "AudioFader" => {
                        for p in self.params(core) {
                            match p.name {
                                "Volume" => {
                                    if let Some(v) = p.value.and_then(num) {
                                        let db = if v <= 1e-15 { -287.5 } else { 20.0 * v.log10() };
                                        t.volume_db = (db.clamp(-287.5, 15.0) * 1000.0).round() / 1000.0;
                                    }
                                    if !p.keys.is_empty() {
                                        self.report.warn("audio track volume automation was not imported");
                                    }
                                }
                                "Mute" if p.value.and_then(boolean) == Some(true) => t.muted = true,
                                _ => {}
                            }
                        }
                    }
                    "AudioFilterComponent" => match audio_filter_name(comp, core) {
                        Some(name) => self.report.warn(format!("audio track effect \"{name}\" (a mixer insert) was not imported")),
                        None => self.report.warn("audio track plug-ins (mixer inserts) were not imported"),
                    },
                    _ => {}
                }
            }
        }
        if let Some(pan) = self.g.follow(audio, &["Panner"]) {
            for p in self.params(at(pan, &["PanProcessor", "AudioComponent", "Component"])) {
                if p.name != "Balance" {
                    continue;
                }
                if let Some(v) = p.value.and_then(num) {
                    t.pan = ((v - 0.5) * 200.0).clamp(-100.0, 100.0);
                }
                if !p.keys.is_empty() {
                    self.report.warn("audio track pan automation was not imported (the pan at its start was)");
                }
            }
        }
    }

    /// The project item a clip shows (a media item, or a sequence when nested).
    fn source_item(&mut self, clip: Node<'a, 'i>, sub: Node<'a, 'i>) -> Option<ItemId> {
        let master = child(sub, "MasterClip").and_then(|m| m.attribute("ObjectURef"));
        match self.clip_source(clip)? {
            Source::Sequence(seq) => {
                let uid = seq.attribute("ObjectUID")?;
                if clip.tag_name().name() == "RemixClip" {
                    self.report.info("Remix clips were imported as nested sequences holding the remixed edit");
                }
                if let Some(&id) = self.seqs.get(uid) {
                    return Some(id);
                }
                let name = child_text(seq, "Name").filter(|n| !n.is_empty()).unwrap_or("Nested Sequence").to_string();
                Some(self.reserve_sequence(seq, &name, None, None))
            }
            Source::Media(media) => {
                let muid = media.attribute("ObjectUID");
                if let Some(m) = master
                    && let Some(&id) = self.masters.get(m)
                    && (self.master_media.get(m).copied() == muid || self.file_item(media) == Some(id))
                {
                    return Some(id);
                }
                if let Some(id) = muid.and_then(|m| self.media.get(m).copied()).or_else(|| self.file_item(media)) {
                    return Some(id);
                }
                // Media a sequence uses but no bin lists: add it to the root bin. (Graphics are
                // never listed; FilmCraft keeps them out of the Project panel too.)
                let mc = master.and_then(|m| self.g.by_uid.get(m).copied());
                let fp = child_text(media, "FilePath").unwrap_or("");
                let name = child_text(media, "Title").filter(|t| !t.is_empty()).unwrap_or_else(|| file_name(fp)).to_string();
                let id = self.media_item(media, mc, &name, None, None)?;
                if !self.synthetic.contains(&id) {
                    self.report.info("media used in sequences but not listed in any bin was added to the root bin");
                }
                if let (Some(m), Some(mu)) = (master, muid) {
                    self.masters.entry(m).or_insert(id);
                    self.master_media.entry(m).or_insert(mu);
                }
                Some(id)
            }
        }
    }

    /// A clip on a track, with the ids of its head and tail transitions.
    fn track_item(&mut self, n: Node<'a, 'i>, kind: TrackKind, frame: (f64, f64), seq_name: &str) -> Option<(TrackItem, Option<&'a str>, Option<&'a str>)> {
        let tag = n.tag_name().name();
        if !matches!(tag, "VideoClipTrackItem" | "AudioClipTrackItem") {
            self.report.info(format!("track items of type {tag} were skipped"));
            return None;
        }
        let cti = child(n, "ClipTrackItem");
        let ti = cti.and_then(|c| child(c, "TrackItem"));
        let start = ti.and_then(|t| child_i64(t, "Start")).unwrap_or(0);
        let end = ti.and_then(|t| child_i64(t, "End")).unwrap_or(-1);
        let sub = cti.and_then(|c| self.g.follow(c, &["SubClip"]));
        let clip = sub.and_then(|s| self.g.follow(s, &["Clip"]));
        let name = sub.and_then(|s| child_text(s, "Name")).filter(|s| !s.is_empty()).unwrap_or("Clip");
        if start < 0 || end <= start || end > Tick::MAX.0 {
            self.report.warn(format!("sequence \"{seq_name}\": clip \"{name}\" has an invalid position and was skipped"));
            return None;
        }
        let body = clip.map(clip_body);
        let (Some(cti), Some(sub), Some(clip), Some(body), Some(c)) = (cti, sub, clip, body, body.and_then(|b| child(b, "Clip"))) else {
            self.report.warn(format!("sequence \"{seq_name}\": clip \"{name}\" has no source and was skipped"));
            return None;
        };
        let Some(item) = self.source_item(clip, sub) else {
            self.report.warn(format!("sequence \"{seq_name}\": the source of clip \"{name}\" is not in the project; the clip was skipped"));
            return None;
        };
        let dur = end - start;
        let in_pt = child_i64(c, "InPoint").unwrap_or(0).clamp(Tick::MIN.0, Tick::MAX.0);
        let out_pt = child_i64(c, "OutPoint").filter(|o| *o > in_pt && *o <= Tick::MAX.0);
        let measured = out_pt.map(|o| (o - in_pt) as f64 / dur as f64).filter(|s| s.is_finite() && *s > 0.0);
        let raw_speed = child_f64(c, "PlaybackSpeed").unwrap_or(1.0);
        let mut speed = raw_speed.abs();
        if child(c, "TimeRemapping").is_some() {
            self.report.warn("speed ramps (time remapping) were imported as a constant speed");
            speed = measured.unwrap_or(1.0);
        } else if !(speed.is_finite() && speed > 0.0) {
            speed = measured.unwrap_or(1.0);
        }
        let speed = speed.clamp(1e-4, 1e4);
        let offset = if self.synthetic.contains(&item) { in_pt } else { 0 };
        let mut t = self.b.clip(item, kind, name, Tick(start), Tick(dur), Tick(in_pt - offset));
        t.speed = speed;
        t.reverse = child_bool(c, "PlayBackwards") == Some(true) || raw_speed < 0.0;
        t.enabled = child_bool(cti, "IsMuted") != Some(true);
        if let Some(l) = label_of(path_text(c, &["Node", "Properties", "asl.clip.label.name"])) {
            t.label = l;
        }
        if kind == TrackKind::Audio
            && let Some(g) = child_f64(body, "Gain").filter(|g| g.is_finite() && *g >= 0.0)
            && (g - 1.0).abs() > 1e-6
        {
            t.gain_db = (gain_to_db(g) * 1000.0).round() / 1000.0;
        }
        // Frame Hold Options: 4 = hold on a source time (`FrameHoldStart`; what Add Frame Hold
        // writes). 1 / 2 carry no time: In / Out point (inferred; the two are not distinguishable
        // from the files seen so far).
        let hold = match child_i64(body, "FrameHold") {
            None | Some(0) => None,
            Some(1) => Some(in_pt),
            Some(2) => out_pt.map(|o| o - 1),
            Some(4) => child_i64(body, "FrameHoldStart").filter(|h| (0..=Tick::MAX.0).contains(h)),
            Some(_) => {
                self.report.warn("a frame hold of an unknown kind was not imported");
                None
            }
        };
        t.frame_hold = hold.and_then(|h| h.checked_sub(offset)).map(Tick);
        t.scale_to_frame = child_i64(body, "ScaleToFramePolicy") == Some(1);
        // clip markers, unless they are the source's (Premiere shares those with every instance)
        if let Some(mk) = self.g.follow(c, &["MarkerOwner", "Markers"])
            && mk.attribute("ObjectID") != self.source_markers.get(&item).copied()
        {
            t.markers = self.markers(Some(mk), offset);
        }
        if let Some(chain) = self.g.follow(cti, &["ComponentOwner", "Components"]) {
            match kind {
                TrackKind::Video => {
                    let src = self.source_size(item).unwrap_or(frame);
                    self.video_effects(&mut t, chain, frame, src, offset);
                }
                TrackKind::Audio => self.audio_effects(&mut t, chain, offset),
            }
        }
        if matches!(self.b.p.item(item).map(|i| &i.kind), Some(ItemKind::Graphic { .. })) {
            self.graphic_clips += 1;
        }
        let head = child(cti, "HeadTransition").and_then(|h| h.attribute("ObjectRef"));
        let tail = child(cti, "TailTransition").and_then(|h| h.attribute("ObjectRef"));
        Some((t, head, tail))
    }

    fn source_size(&self, item: ItemId) -> Option<(f64, f64)> {
        let (w, h) = match &self.b.p.item(item)?.kind {
            ItemKind::Media(m) => m.info.video.as_ref().map(|v| (v.width, v.height))?,
            ItemKind::Sequence(s) => (s.settings.width, s.settings.height),
            ItemKind::AdjustmentLayer { width, height, .. } | ItemKind::Graphic { width, height, .. } => (*width, *height),
            ItemKind::Subclip { .. } => return None,
        };
        Some((f64::from(w), f64::from(h)))
    }

    fn transition(
        &mut self,
        n: Node<'a, 'i>,
        kind: TrackKind,
        track: &Track,
        incoming: Option<ClipId>,
        outgoing: Option<ClipId>,
        seq_name: &str,
    ) -> Option<Transition> {
        let tti = child(n, "TransitionTrackItem")?;
        let ti = child(tti, "TrackItem")?;
        let start = child_i64(ti, "Start").unwrap_or(0);
        let end = child_i64(ti, "End")?;
        if start < 0 || end <= start || end > Tick::MAX.0 {
            self.report.warn(format!("sequence \"{seq_name}\": a transition with an invalid position was skipped"));
            return None;
        }
        let dur = end - start;
        let align = child_i64(tti, "Alignment").unwrap_or(dur / 2).clamp(0, dur);
        let cut = start + align;
        let from = if child_bool(tti, "HasOutgoingClip") != Some(false) {
            outgoing.or_else(|| track.items.iter().find(|c| c.end().0 == cut).map(|c| c.id))
        } else {
            None
        };
        let to = if child_bool(tti, "HasIncomingClip") != Some(false) {
            incoming.or_else(|| track.items.iter().find(|c| c.start.0 == cut).map(|c| c.id))
        } else {
            None
        };
        if from.is_none() && to.is_none() {
            self.report.warn(format!("sequence \"{seq_name}\": a transition not attached to any clip was skipped"));
            return None;
        }
        let name = child_text(tti, "DisplayName").filter(|s| !s.is_empty()).or_else(|| child_text(tti, "MatchName")).unwrap_or("Cross Dissolve");
        let effect = transition_effect(transition_alias(name), kind == TrackKind::Audio, &mut self.report);
        let align = if align == 0 {
            TransitionAlign::StartAtCut
        } else if align == dur {
            TransitionAlign::EndAtCut
        } else {
            TransitionAlign::CenterAtCut
        };
        let reverse = child_bool(n, "Reverse").or_else(|| child_bool(tti, "Reverse")) == Some(true);
        Some(Transition { id: TransitionId(self.b.alloc()), effect, start: Tick(start), duration: Tick(dur), from, to, align, reverse })
    }

    // ---- markers ----

    fn markers(&mut self, owner: Option<Node<'a, 'i>>, offset: i64) -> Vec<Marker> {
        let mut out = Vec::new();
        let Some(list) = owner.and_then(|o| child(o, "Markers")) else { return out };
        for m in children(list, "Marker") {
            let Some(text) = self.g.follow(m, &["Second"]).and_then(|o| child_text(o, "DVAMarker")) else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
                self.report.warn("a marker could not be read and was skipped");
                continue;
            };
            let d = &v["DVAMarker"];
            let ticks = |k: &str| {
                let t = &d[k]["ticks"];
                t.as_i64().or_else(|| t.as_f64().filter(|f| f.is_finite()).map(|f| f.round() as i64)).or_else(|| t.as_str().and_then(|s| s.parse().ok()))
            };
            let Some(start) = ticks("mStartTime").and_then(|s| s.checked_sub(offset)).filter(|s| (Tick::MIN.0..=Tick::MAX.0).contains(s)) else { continue };
            let duration = ticks("mDuration").unwrap_or(0).clamp(0, Tick::MAX.0);
            let kind = match d["mType"].as_str().unwrap_or("Comment") {
                "Chapter" => MarkerKind::Chapter,
                "Segmentation" => MarkerKind::Segmentation,
                "WebLink" => MarkerKind::WebLink,
                "FlashCuePoint" | "Cue" => MarkerKind::FlashCue,
                _ => MarkerKind::Comment,
            };
            let color = d["mCuePointList"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|e| e["mKey"].as_str().is_some_and(|k| k.starts_with("keywordExtDVAv1_")))
                .find_map(|e| serde_json::from_str::<serde_json::Value>(e["mValue"].as_str()?).ok()?["color"].as_u64())
                .map(marker_label)
                .unwrap_or(Label::Green);
            out.push(Marker {
                id: MarkerId(self.b.alloc()),
                start: Tick(start),
                duration: Tick(duration),
                name: d["mName"].as_str().unwrap_or("").to_string(),
                comment: d["mComment"].as_str().unwrap_or("").to_string(),
                kind,
                color,
            });
        }
        out.sort_by_key(|m| m.start);
        out
    }

    // ---- effects ----

    fn params(&mut self, core: Option<Node<'a, 'i>>) -> Vec<PParam<'a>> {
        let params: Vec<PParam<'a>> = self.g.list(core.and_then(|c| child(c, "Params")), "Param").into_iter().map(parse_param).collect();
        for p in params.iter().filter(|p| p.truncated) {
            let name = if p.name.is_empty() { "a parameter" } else { p.name };
            self.report.warn(format!("\"{name}\" has more than {MAX_KEYFRAMES} keyframes; only the first {MAX_KEYFRAMES} were imported"));
        }
        params
    }

    fn components(&self, chain: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
        self.g.list(at(chain, &["ComponentChain", "Components"]), "Component")
    }

    fn video_effects(&mut self, t: &mut TrackItem, chain: Node<'a, 'i>, frame: (f64, f64), src: (f64, f64), offset: i64) {
        for comp in self.components(chain) {
            if comp.tag_name().name() != "VideoFilterComponent" {
                continue;
            }
            let mn = child_text(comp, "MatchName").unwrap_or("");
            let core = child(comp, "Component");
            let enabled = core.and_then(|c| child_bool(c, "Bypass")) != Some(true);
            let params = self.params(core);
            match mn {
                "AE.ADBE Motion" => self.motion(t, &params, enabled, frame, src, offset),
                "AE.ADBE Opacity" => self.opacity(t, &params, enabled, offset),
                // source settings, and the layers of graphic clips (reported per clip)
                "AE.ADBE MPEG.SourceSettings" | "AE.ADBE Text" | "AE.ADBE Shape" | "AE.ADBE Graphic Group" => {}
                "AE.ADBE AEMask" | "AE.ADBE AEMask2" => self.report.warn("masks were not imported"),
                _ => match MAPPED.iter().find(|(m, ..)| *m == mn) {
                    Some((_, id, table)) => {
                        if *id == "lumetri" {
                            self.report.info("Lumetri Color: the Basic Correction and Creative sliders were imported; curves, color wheels, HSL Secondary, vignette and LUTs were not");
                        }
                        self.mapped(t, id, table, &params, enabled, offset);
                    }
                    None => {
                        let name = core.and_then(|c| child_text(c, "DisplayName")).filter(|n| !n.is_empty()).unwrap_or_else(|| mn.trim_start_matches("AE."));
                        self.report.warn(format!("effect \"{name}\" is not supported and was not imported"));
                    }
                },
            }
        }
    }

    fn motion(&mut self, t: &mut TrackItem, params: &[PParam], enabled: bool, frame: (f64, f64), src: (f64, f64), offset: i64) {
        let mut crop: Vec<(&str, &PParam)> = Vec::new();
        for p in params {
            match p.pid {
                Some(1) if !is_centre(p) => {
                    if let Some(v) = to_param(p, offset, |s| point(s).and_then(|(x, y)| pixels(x * frame.0, y * frame.1))) {
                        set_param(t, "motion", "position", v);
                    }
                }
                Some(2) => self.put(t, "motion", "scale", to_param(p, offset, float_in("motion", "scale"))),
                Some(3) => self.put(t, "motion", "scale_width", to_param(p, offset, float_in("motion", "scale_width"))),
                Some(4) => self.put(t, "motion", "uniform_scale", to_param(p, offset, |s| boolean(s).map(ParamValue::Bool))),
                Some(5) => self.put(t, "motion", "rotation", to_param(p, offset, float_in("motion", "rotation"))),
                Some(6) if !is_centre(p) => {
                    if let Some(v) = to_param(p, offset, |s| point(s).and_then(|(x, y)| pixels(x * src.0, y * src.1))) {
                        set_param(t, "motion", "anchor", v);
                    }
                }
                Some(7) => self.put(t, "motion", "anti_flicker", to_param(p, offset, float_in("motion", "anti_flicker"))),
                Some(8) => crop.push(("left", p)),
                Some(9) => crop.push(("top", p)),
                Some(10) => crop.push(("right", p)),
                Some(11) => crop.push(("bottom", p)),
                _ => {}
            }
        }
        // Motion's crop (Premiere 2025) is our Crop effect.
        if crop.iter().any(|(_, p)| !is_zero(p))
            && let Some(def) = find_effect("crop")
        {
            let mut e = def.instance();
            for (id, p) in crop {
                if let Some(v) = to_param(p, offset, |s| num(s).map(|v| ParamValue::Float(v.clamp(0.0, 100.0)))) {
                    e.params.insert(id.to_string(), v);
                }
            }
            t.effects.push(e);
        }
        if !enabled && let Some(m) = t.effect_mut("motion") {
            m.enabled = false;
        }
    }

    fn opacity(&mut self, t: &mut TrackItem, params: &[PParam], enabled: bool, offset: i64) {
        for p in params {
            match p.pid {
                Some(1) => self.put(t, "opacity", "opacity", to_param(p, offset, |s| num(s).map(|v| ParamValue::Float(v.clamp(0.0, 100.0))))),
                // Blend Mode, by Premiere's menu order (which is ours)
                Some(3) => match p.value.and_then(num).map(|v| v.round() as i64) {
                    Some(0) | None => {}
                    Some(v) if usize::try_from(v).is_ok_and(|v| v < BLEND_MODES.len()) => {
                        set_param(t, "opacity", "blend", Param::new(ParamValue::Choice(v as u32)));
                    }
                    Some(_) => self.report.warn("an unknown blend mode was imported as Normal"),
                },
                _ => {}
            }
        }
        if !enabled && let Some(o) = t.effect_mut("opacity") {
            o.enabled = false;
        }
    }

    fn put(&mut self, t: &mut TrackItem, effect: &str, param: &str, value: Option<Param>) {
        if let Some(v) = value {
            set_param(t, effect, param, v);
        }
    }

    /// An effect mapped parameter by parameter through `table`.
    fn mapped(&mut self, t: &mut TrackItem, id: &str, table: &[(u32, &str, Conv)], params: &[PParam], enabled: bool, offset: i64) {
        let Some(def) = find_effect(id) else { return };
        let mut e = def.instance();
        e.enabled = enabled;
        for p in params {
            let Some(&(_, pid, conv)) = table.iter().find(|(n, ..)| Some(*n) == p.pid) else { continue };
            let Some(pd) = def.param(pid) else { continue };
            let value = match conv {
                Conv::Float(scale) => {
                    let (lo, hi) = match &pd.kind {
                        ParamKind::Float { min, max, .. } => (*min, *max),
                        _ => (f64::MIN, f64::MAX),
                    };
                    to_param(p, offset, |s| num(s).map(|v| ParamValue::Float((v * scale).clamp(lo, hi))))
                }
                Conv::Bool => to_param(p, offset, |s| boolean(s).map(ParamValue::Bool)),
                Conv::Choice => {
                    let n = match &pd.kind {
                        ParamKind::Choice(opts) => opts.len(),
                        _ => 0,
                    };
                    to_param(p, offset, |s| num(s).map(|v| v.round()).filter(|v| *v >= 0.0 && (*v as usize) < n).map(|v| ParamValue::Choice(v as u32)))
                }
                Conv::Color => to_param(p, offset, |s| color(s).map(ParamValue::Color)),
            };
            if let Some(v) = value {
                e.params.insert(pid.to_string(), v);
            }
        }
        t.effects.push(e);
    }

    fn audio_effects(&mut self, t: &mut TrackItem, chain: Node<'a, 'i>, offset: i64) {
        for comp in self.components(chain) {
            if comp.tag_name().name() != "AudioFilterComponent" {
                continue;
            }
            let fm = child_text(comp, "FilterMatchName").unwrap_or("");
            let core = at(comp, &["AudioComponent", "Component"]);
            let enabled = core.and_then(|c| child_bool(c, "Bypass")) != Some(true);
            let params = self.params(core);
            let (effect, max) = if fm.starts_with("Internal Volume") {
                ("volume", 15.0)
            } else if fm.starts_with("Internal Channel Volume") {
                ("channel_volume", 6.0)
            } else {
                match audio_filter_name(comp, core) {
                    Some(n) => self.report.warn(format!("audio effect \"{n}\" is not supported and was not imported")),
                    None => self.report.warn("audio plug-ins and built-in audio effects other than Volume and Channel Volume were not imported"),
                }
                continue;
            };
            for p in &params {
                let pid = match (effect, p.name) {
                    (_, "Mute" | "Bypass") => "bypass",
                    ("volume", "Level") => "level",
                    ("channel_volume", "Left") => "left",
                    ("channel_volume", "Right") => "right",
                    _ => continue,
                };
                let v = if pid == "bypass" { to_param(p, offset, |s| boolean(s).map(ParamValue::Bool)) } else { to_param(p, offset, |s| level_db(s, max)) };
                self.put(t, effect, pid, v);
            }
            if !enabled && let Some(e) = t.effect_mut(effect) {
                e.enabled = false;
            }
        }
    }

    // ---- finishing ----

    /// Sequences directly nested in `seq`.
    fn nested_in(&self, seq: ItemId) -> Vec<ItemId> {
        let Some(s) = self.b.p.item(seq).and_then(|i| i.as_sequence()) else { return Vec::new() };
        s.all_tracks().flat_map(|t| t.items.iter()).map(|c| c.item).filter(|i| self.b.p.item(*i).is_some_and(|it| it.as_sequence().is_some())).collect()
    }

    fn reaches(&self, from: ItemId, to: ItemId) -> bool {
        let mut stack = vec![from];
        let mut seen = HashSet::new();
        while let Some(s) = stack.pop() {
            if s == to {
                return true;
            }
            if seen.insert(s) {
                stack.extend(self.nested_in(s));
            }
        }
        false
    }

    /// Remove clips that would nest a sequence inside itself (only a damaged file has them).
    fn break_cycles(&mut self) {
        let seqs: Vec<ItemId> = self.pending.iter().map(|(_, id)| *id).collect();
        for &a in &seqs {
            let bad: HashSet<ItemId> = self.nested_in(a).into_iter().filter(|b| self.reaches(*b, a)).collect();
            if bad.is_empty() {
                continue;
            }
            if let Some(s) = self.b.p.sequence_mut(a) {
                for t in s.all_tracks_mut() {
                    t.items.retain(|c| !bad.contains(&c.item));
                    // a transition keeps the side whose clip is still there (a dissolve becomes a
                    // fade); one with neither side goes
                    let left: HashSet<ClipId> = t.items.iter().map(|c| c.id).collect();
                    for tr in &mut t.transitions {
                        tr.from = tr.from.filter(|c| left.contains(c));
                        tr.to = tr.to.filter(|c| left.contains(c));
                    }
                    t.transitions.retain(|tr| tr.from.is_some() || tr.to.is_some());
                }
            }
            self.report.warn("clips that nested a sequence inside itself were removed");
        }
    }

    /// Adjustment layers last at least as long as their longest use.
    fn grow_synthetic(&mut self) {
        let mut need: HashMap<ItemId, Tick> = HashMap::new();
        for it in self.b.p.items.values() {
            if let Some(seq) = it.as_sequence() {
                for c in seq.all_tracks().flat_map(|t| t.items.iter()) {
                    let e = need.entry(c.item).or_insert(Tick::ZERO);
                    *e = (*e).max(c.source_out());
                }
            }
        }
        for (id, end) in need {
            if let Some(ItemKind::AdjustmentLayer { duration, .. }) = self.b.p.item_mut(id).map(|i| &mut i.kind)
                && *duration < end
            {
                *duration = end;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
        out.extend(miniz_oxide::deflate::compress_to_vec(data, 6));
        out.extend(crc32(data).to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out
    }

    #[test]
    fn crc_and_gzip() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        let xml = b"<?xml version=\"1.0\"?><PremiereData Version=\"3\"></PremiereData>";
        let gz = gzip(xml);
        assert!(sniff(&gz));
        assert!(sniff(xml));
        assert!(!sniff(b"<xmeml version=\"4\"/>"));
        assert!(!sniff(&gzip(b"hello")));
        let mut r = Report::default();
        assert_eq!(gunzip(&gz, &mut r).unwrap(), xml);
        assert!(r.is_empty());
        // a flipped trailer byte is reported, not fatal
        let mut bad = gz.clone();
        let n = bad.len();
        bad[n - 5] ^= 1;
        assert_eq!(gunzip(&bad, &mut r).unwrap(), xml);
        assert!(r.mentions("checksum"));
        // optional header fields (FEXTRA, FNAME, FCOMMENT, FHCRC) are skipped
        let mut full = vec![0x1f, 0x8b, 8, 0x1e, 0, 0, 0, 0, 0, 0xff, 2, 0, 0xaa, 0xbb];
        full.extend(b"name.xml\0comment\0\x12\x34");
        full.extend_from_slice(&gz[10..]);
        assert_eq!(gunzip(&full, &mut Report::default()).unwrap(), xml);
        // reserved flag bits and truncated headers are not gzip
        assert!(gzip_body(&[0x1f, 0x8b, 8, 0x20, 0, 0, 0, 0, 0, 0, 0]).is_none());
        assert!(gzip_body(&[0x1f, 0x8b, 8, 0x08, 0, 0, 0, 0, 0, 0, b'a']).is_none());
    }

    #[test]
    fn small_decoders() {
        assert_eq!(base64("EhETAAEAAAA="), vec![0x12, 0x11, 0x13, 0, 1, 0, 0, 0]);
        assert_eq!(base64("AOMXAAEAAAA="), vec![0x00, 0xe3, 0x17, 0, 1, 0, 0, 0]);
        assert_eq!(rate_from_ticks(8_475_667_200), Some(FrameRate::FPS_29_97));
        assert_eq!(rate_from_ticks(4_233_600_000), Some(FrameRate::FPS_60));
        assert_eq!(rate_from_ticks(4_237_833_600), Some(FrameRate::FPS_59_94));
        assert_eq!(rate_from_ticks(0), None);
        assert_eq!(rate_from_ticks(-5), None);
        assert_eq!(sample_rate_from_ticks(5_292_000), Some(48_000));
        assert_eq!(sample_rate_from_ticks(5_760_000), Some(44_100));
        assert_eq!(sample_rate_from_ticks(1), None);
        assert_eq!(parse_rect("0,0,3840,2160"), Some((3840, 2160)));
        assert_eq!(parse_rect("0,0,0,2160"), None);
        assert_eq!(parse_rect("10,10,5,5"), None);
        assert_eq!(parse_rect("0,0,9999999999,1"), None);
        assert_eq!(channel_count(r#"[{"channellabel":100},{"channellabel":101}]"#), Some(2));
        assert_eq!(label_of(Some("BE.Prefs.LabelColors.7")), Some(Label::Mango));
        assert_eq!(label_of(Some("BE.Prefs.LabelColors.99")), None);
        assert_eq!(marker_label(0xff37_8671), Label::Green);
        assert_eq!(marker_label(0xffff_ffff), Label::Lavender);
        assert_eq!(marker_label(0xff3d_3cc1), Label::Rose);
        assert_eq!(color("18374704073938046720").map(|c| [c[0], c[1], c[2]].map(|v| (v * 255.0).round() as u8)), Some([16, 124, 15]));
        assert_eq!(color("18374686479671623680"), Some([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(num("100."), Some(100.0));
        assert_eq!(boolean("true"), Some(true));
        assert_eq!(point("0.5:0.25"), Some((0.5, 0.25)));
        assert_eq!(transition_alias("Cross Dissolve (Legacy)"), "Cross Dissolve");
        assert_eq!(transition_alias("FI: Push Impacts"), "Push");
        let db = |v: f64| match level_db(&v.to_string(), 15.0) {
            Some(ParamValue::Float(d)) => d,
            _ => f64::NAN,
        };
        assert!(db(0.177_827_939_391_136_17).abs() < 1e-6);
        assert!((db(1.0) - 15.0).abs() < 1e-9);
        assert_eq!(db(0.0), -287.5);
    }

    #[test]
    fn nesting() {
        let d = |s: &str| nesting_depth(s.as_bytes(), 1000);
        assert_eq!(d("<a><b/><c x=\"1\"><d></d></c></a>"), 3);
        assert_eq!(d("<?xml version=\"1.0\"?><!-- <x><x><x> --><a><![CDATA[<b><c>]]></a>"), 1);
        // '>' and "/>" inside attribute values do not end the tag
        assert_eq!(d("<a t=\"x/>\"><b t='>'><c/></b></a>"), 2);
        assert_eq!(d(&format!("{}{}", "<a>".repeat(50), "</a>".repeat(50))), 50);
        assert_eq!(nesting_depth("<a>".repeat(10_000).as_bytes(), 128), 129, "stops counting past the limit");
        // unterminated constructs end the scan (an unfinished tag counts: over, never under)
        assert_eq!(d("<a><!-- open"), 1);
        assert_eq!(d("<a b=\"unclosed><c><d>"), 1);
    }

    #[test]
    fn keyframes() {
        let (k, truncated) = parse_keys("4819827589,0.,0,0,0,0.16666666666666666,252.57,0.16666666666666666;105392315449,100.,5,4,252.5,0.5,0,0.5;bad;;");
        assert!(!truncated);
        let many = "0,1.;".repeat(MAX_KEYFRAMES + 1);
        let (all, cut) = parse_keys(&many);
        assert_eq!((all.len(), cut), (MAX_KEYFRAMES, true));
        assert!(!parse_keys(&"0,1.;".repeat(MAX_KEYFRAMES)).1, "exactly the limit is complete");
        assert_eq!(k.len(), 2);
        assert_eq!(k[0].time, 4_819_827_589);
        assert_eq!(k[0].interp, Interpolation::Linear);
        assert_eq!(k[1].interp, Interpolation::AutoBezier);
        assert!((k[1].in_influence - 0.5).abs() < 1e-12);
        let p = PParam { pid: Some(1), name: "Opacity", value: Some("100."), keys: k, truncated };
        let param = to_param(&p, 1_000, |s| num(s).map(ParamValue::Float)).unwrap();
        assert_eq!(param.keyframes.len(), 2);
        assert_eq!(param.keyframes[0].time, Tick(4_819_826_589));
        assert_eq!(param.value, ParamValue::Float(0.0));
        assert_eq!(interpolation("4", "0"), Interpolation::Hold);
        assert_eq!(interpolation("5", "0"), Interpolation::Bezier);
    }
}
