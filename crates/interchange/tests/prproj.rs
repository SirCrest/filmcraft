//! Premiere Pro project import on synthetic projects written here (the object graph as Premiere
//! lays it out), plus hostile input: truncation, corruption, cycles and deep nesting.

use filmcraft_interchange::{ImportOptions, prproj};
use filmcraft_media::Generator;
use filmcraft_project::{BinEntry, Interpolation, ItemId, ItemKind, Label, MarkerKind, MediaRef, ParamValue, Project, TrackItem, TrackKind, TransitionAlign};
use filmcraft_time::{FrameRate, Tick};
use proptest::prelude::*;

const S: i64 = 254_016_000_000;
const FPS25: i64 = S / 25;
const VIDEO: &str = "228cda18-3625-4d2d-951e-348879e4ed93";
const AUDIO: &str = "80b8e3d5-6dca-4195-aefb-cb5f407ab009";
const DATA: &str = "d8143ffe-eec4-4d2a-a909-d5f7bf094dc5";
/// Premiere's "no keyframe" time in `StartKeyframe`.
const NO_TIME: &str = "-91445760000000000";

fn obj(tag: &str, id: &str, body: &str) -> String {
    let attr = if id.contains('-') { "ObjectUID" } else { "ObjectID" };
    format!("<{tag} {attr}=\"{id}\" ClassID=\"00000000-0000-0000-0000-000000000000\" Version=\"1\">{body}</{tag}>\n")
}

fn r(tag: &str, id: &str) -> String {
    if id.contains('-') { format!("<{tag} ObjectURef=\"{id}\"/>") } else { format!("<{tag} ObjectRef=\"{id}\"/>") }
}

fn items(list: &str, item: &str, ids: &[&str]) -> String {
    let inner: String = ids.iter().enumerate().map(|(i, id)| r(item, id).replacen(&format!("<{item} "), &format!("<{item} Index=\"{i}\" "), 1)).collect();
    format!("<{list} Version=\"1\">{inner}</{list}>")
}

fn project_item(tag: &str, uid: &str, name: &str, label: Option<usize>, extra: &str) -> String {
    let label = label.map(|l| format!("<Column.PropertyText.Label>BE.Prefs.LabelColors.{l}</Column.PropertyText.Label>")).unwrap_or_default();
    obj(
        tag,
        uid,
        &format!(
            "<ProjectItem Version=\"1\"><Node Version=\"1\"><Properties Version=\"1\">{label}</Properties></Node><Name>{name}</Name></ProjectItem>{extra}"
        ),
    )
}

fn bin(uid: &str, name: &str, children: &[&str]) -> String {
    project_item(
        "BinProjectItem",
        uid,
        name,
        Some(7),
        &format!("<ProjectItemContainer Version=\"1\">{}</ProjectItemContainer>", items("Items", "Item", children)),
    )
}

fn clip_item(uid: &str, name: &str, label: Option<usize>, master: &str) -> String {
    project_item("ClipProjectItem", uid, name, label, &r("MasterClip", master))
}

fn master(uid: &str, name: &str, clips: &[&str], extra: &str) -> String {
    obj("MasterClip", uid, &format!("{}{extra}<Name>{name}</Name>", items("Clips", "Clip", clips)))
}

/// A `VideoClip` / `AudioClip` with its `Clip` body.
fn clip(tag: &str, id: &str, source: &str, in_out: Option<(i64, i64)>, inner: &str, outer: &str) -> String {
    let io = in_out.map(|(i, o)| format!("<InPoint>{i}</InPoint><OutPoint>{o}</OutPoint>")).unwrap_or_default();
    obj(tag, id, &format!("<Clip Version=\"18\">{inner}{}{io}</Clip>{outer}", r("Source", source)))
}

fn media_source(tag: &str, id: &str, media: &str) -> String {
    obj(tag, id, &format!("<MediaSource Version=\"4\">{}</MediaSource>", r("Media", media)))
}

fn media(uid: &str, path: &str, video: Option<&str>, audio: Option<&str>, extra: &str) -> String {
    let v = video.map(|v| r("VideoStream", v)).unwrap_or_default();
    let a = audio.map(|a| r("AudioStream", a)).unwrap_or_default();
    obj("Media", uid, &format!("{v}{a}<FilePath>{path}</FilePath><Title>{}</Title>{extra}", path.rsplit(['\\', '/']).next().unwrap_or(path)))
}

fn video_stream(id: &str, seconds: i64) -> String {
    obj("VideoStream", id, &format!("<FrameRate>{FPS25}</FrameRate><Duration>{}</Duration><FrameRect>0,0,1920,1080</FrameRect>", seconds * S))
}

fn audio_stream(id: &str, seconds: i64) -> String {
    obj(
        "AudioStream",
        id,
        &format!(
            "<FrameRate>5292000</FrameRate><Duration>{}</Duration><AudioChannelLayout>[{{\"channellabel\":100}},{{\"channellabel\":101}}]</AudioChannelLayout>",
            seconds * S
        ),
    )
}

fn param(tag: &str, id: &str, pid: u32, name: &str, value: &str, keys: Option<&str>) -> String {
    let k = keys.map(|k| format!("<IsTimeVarying>true</IsTimeVarying><Keyframes>{k}</Keyframes>")).unwrap_or_default();
    obj(tag, id, &format!("<StartKeyframe>{NO_TIME},{value},0,0,0,0,0,0</StartKeyframe>{k}<Name>{name}</Name><ParameterID>{pid}</ParameterID>"))
}

fn video_filter(id: &str, match_name: &str, display: &str, params: &[&str]) -> String {
    obj(
        "VideoFilterComponent",
        id,
        &format!(
            "<Component Version=\"7\">{}<DisplayName>{display}</DisplayName></Component><MatchName>{match_name}</MatchName>",
            items("Params", "Param", params)
        ),
    )
}

fn audio_filter(id: &str, filter: &str, display: Option<&str>, params: &[&str]) -> String {
    let d = display.map(|d| format!("<DisplayName>{d}</DisplayName>")).unwrap_or_default();
    obj(
        "AudioFilterComponent",
        id,
        &format!(
            "<AudioComponent Version=\"3\"><Component Version=\"7\">{}{d}</Component></AudioComponent><FilterMatchName>{filter}</FilterMatchName>",
            items("Params", "Param", params)
        ),
    )
}

fn chain(tag: &str, id: &str, components: &[&str]) -> String {
    obj(tag, id, &format!("<ComponentChain Version=\"3\">{}</ComponentChain>", items("Components", "Component", components)))
}

/// A clip on a track.
fn track_item(tag: &str, id: &str, start: i64, end: i64, sub: &str, chain: Option<&str>, extra: &str) -> String {
    let c = chain.map(|c| format!("<ComponentOwner Version=\"1\">{}</ComponentOwner>", r("Components", c))).unwrap_or_default();
    let st = if start == 0 { String::new() } else { format!("<Start>{start}</Start>") };
    obj(
        tag,
        id,
        &format!("<ClipTrackItem Version=\"8\">{c}<TrackItem Version=\"4\">{st}<End>{end}</End></TrackItem>{}{extra}</ClipTrackItem>", r("SubClip", sub)),
    )
}

fn sub_clip(id: &str, clip: &str, master: &str, name: &str) -> String {
    obj("SubClip", id, &format!("{}{}<Name>{name}</Name>", r("Clip", clip), r("MasterClip", master)))
}

fn transition(tag: &str, id: &str, start: i64, end: i64, align: i64, out: bool, inc: bool, name: &str) -> String {
    obj(
        tag,
        id,
        &format!(
            "<TransitionTrackItem Version=\"3\"><TrackItem Version=\"4\"><Start>{start}</Start><End>{end}</End></TrackItem><DisplayName>{name}</DisplayName><MatchName>{name}</MatchName><Alignment>{align}</Alignment><HasOutgoingClip>{out}</HasOutgoingClip><HasIncomingClip>{inc}</HasIncomingClip></TransitionTrackItem>"
        ),
    )
}

fn track(tag: &str, uid: &str, name: Option<&str>, flags: &str, clips: &[&str], transitions: &[&str]) -> String {
    let n = name.map(|n| format!("<MZ.TrackName>{n}</MZ.TrackName>")).unwrap_or_default();
    obj(
        tag,
        uid,
        &format!(
            "<ClipTrack Version=\"2\"><Track Version=\"4\"><Node Version=\"1\"><Properties Version=\"1\">{n}</Properties></Node>{flags}</Track><ClipItems Version=\"3\">{}</ClipItems><TransitionItems Version=\"3\">{}</TransitionItems></ClipTrack>",
            items("TrackItems", "TrackItem", clips),
            items("TrackItems", "TrackItem", transitions)
        ),
    )
}

fn marker(id: &str, json: &str) -> String {
    obj("Marker", id, &format!("<DVAMarker>{}</DVAMarker>", json.replace('&', "&amp;").replace('<', "&lt;").replace('"', "&quot;")))
}

fn markers(id: &str, ms: &[&str]) -> String {
    let inner: String =
        ms.iter().enumerate().map(|(i, m)| format!("<Marker Version=\"1\" Index=\"{i}\"><First>g{i}</First>{}</Marker>", r("Second", m))).collect();
    obj("Markers", id, &format!("<Markers Version=\"1\">{inner}</Markers>"))
}

fn sequence(uid: &str, name: &str, props: &str, groups: &[(&str, &str)], links: &[&str], marker_obj: Option<&str>) -> String {
    let tg: String = groups
        .iter()
        .enumerate()
        .map(|(i, (guid, id))| format!("<TrackGroup Version=\"1\" Index=\"{i}\"><First>{guid}</First>{}</TrackGroup>", r("Second", id)))
        .collect();
    let mk = marker_obj.map(|m| format!("<MarkerOwner Version=\"1\">{}</MarkerOwner>", r("Markers", m))).unwrap_or_default();
    obj(
        "Sequence",
        uid,
        &format!(
            "<Node Version=\"1\"><Properties Version=\"1\">{props}</Properties></Node>{mk}<PersistentGroupContainer Version=\"1\"><LinkContainer Version=\"1\">{}</LinkContainer></PersistentGroupContainer><TrackGroups Version=\"1\">{tg}</TrackGroups><Name>{name}</Name>",
            items("Links", "Link", links)
        ),
    )
}

fn group(tag: &str, id: &str, tracks: &[&str], rate: i64, extra: &str) -> String {
    obj(tag, id, &format!("<TrackGroup Version=\"1\">{}<FrameRate>{rate}</FrameRate></TrackGroup>{extra}", items("Tracks", "Track", tracks)))
}

fn document(objects: &[String]) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<PremiereData Version=\"3\">\n{}</PremiereData>\n", objects.concat())
}

/// A project exercising most of what the importer maps.
fn sample() -> String {
    let link = obj("Link", "53", &format!("<TrackItemGroup Version=\"1\">{}</TrackItemGroup>", items("TrackItems", "TrackItem", &["100", "110"])));
    let props = format!(
        "<MZ.Sequence.VideoTimeDisplayFormat>101</MZ.Sequence.VideoTimeDisplayFormat><MZ.InPoint>{S}</MZ.InPoint><MZ.OutPoint>-101606400000000000</MZ.OutPoint>"
    );
    document(&[
        obj("Project", "1", &r("RootProjectItem", "a0000000-0000-0000-0000-000000000001")),
        project_item(
            "RootProjectItem",
            "a0000000-0000-0000-0000-000000000001",
            "Root Bin",
            None,
            &format!(
                "<ProjectItemContainer Version=\"1\">{}</ProjectItemContainer>",
                items(
                    "Items",
                    "Item",
                    &[
                        "b0000000-0000-0000-0000-000000000001",
                        "c0000000-0000-0000-0000-00000000000a",
                        "c0000000-0000-0000-0000-00000000000b",
                        "c0000000-0000-0000-0000-00000000000c"
                    ]
                )
            ),
        ),
        bin("b0000000-0000-0000-0000-000000000001", "Footage", &["c0000000-0000-0000-0000-000000000001", "c0000000-0000-0000-0000-000000000002"]),
        clip_item("c0000000-0000-0000-0000-000000000001", "Interview", Some(1), "d0000000-0000-0000-0000-000000000001"),
        clip_item("c0000000-0000-0000-0000-000000000002", "Music", None, "d0000000-0000-0000-0000-000000000002"),
        // the bin item's name is stale; the master clip's is current
        clip_item("c0000000-0000-0000-0000-00000000000a", "Old Name", Some(5), "d0000000-0000-0000-0000-00000000000a"),
        clip_item("c0000000-0000-0000-0000-00000000000b", "Blue Matte", None, "d0000000-0000-0000-0000-00000000000b"),
        clip_item("c0000000-0000-0000-0000-00000000000c", "Adjustment Layer", None, "d0000000-0000-0000-0000-00000000000c"),
        // media
        media("e0000000-0000-0000-0000-000000000001", "C:\\media\\interview.mov", Some("10"), Some("11"), "<RelativePath>media\\interview.mov</RelativePath>"),
        video_stream("10", 10),
        audio_stream("11", 10),
        media("e0000000-0000-0000-0000-000000000002", "music.wav", None, Some("12"), ""),
        audio_stream("12", 30),
        media("e0000000-0000-0000-0000-00000000000b", "1129270354", Some("13"), None, "<ImporterPrefs Encoding=\"base64\">ABfjAAEAAAA=</ImporterPrefs>"),
        video_stream("13", 3600),
        media("e0000000-0000-0000-0000-00000000000c", "1112293707", Some("14"), None, ""),
        video_stream("14", 43200),
        // master clips
        master("d0000000-0000-0000-0000-000000000001", "Interview", &["20", "22"], ""),
        clip("VideoClip", "20", "21", Some((0, 10 * S)), &format!("<MarkerOwner Version=\"1\">{}</MarkerOwner>", r("Markers", "24")), ""),
        media_source("VideoMediaSource", "21", "e0000000-0000-0000-0000-000000000001"),
        clip("AudioClip", "22", "23", Some((0, 10 * S)), "", ""),
        media_source("AudioMediaSource", "23", "e0000000-0000-0000-0000-000000000001"),
        markers("24", &["60"]),
        marker(
            "60",
            r#"{"DVAMarker":{"mMarkerID":"m1","mStartTime":{"ticks":254016000000},"mType":"Comment","mName":"Slate","mCuePointList":[{"mKey":"keywordExtDVAv1_x","mValue":"{\"color\":4282203329}"}]}}"#,
        ),
        master("d0000000-0000-0000-0000-000000000002", "Music", &["25"], ""),
        clip("AudioClip", "25", "26", Some((0, 30 * S)), "", ""),
        media_source("AudioMediaSource", "26", "e0000000-0000-0000-0000-000000000002"),
        master("d0000000-0000-0000-0000-00000000000b", "Blue Matte", &["27"], ""),
        clip("VideoClip", "27", "28", Some((0, 5 * S)), "", ""),
        media_source("VideoMediaSource", "28", "e0000000-0000-0000-0000-00000000000b"),
        master("d0000000-0000-0000-0000-00000000000c", "Adjustment Layer", &["29"], "<IsAdjustmentLayer>true</IsAdjustmentLayer>"),
        clip("VideoClip", "29", "30", Some((0, 5 * S)), "", "<AdjustmentLayer>true</AdjustmentLayer>"),
        media_source("VideoMediaSource", "30", "e0000000-0000-0000-0000-00000000000c"),
        master("d0000000-0000-0000-0000-00000000000a", "Main Edit", &["31"], ""),
        clip("VideoClip", "31", "32", None, "", ""),
        obj("VideoSequenceSource", "32", &format!("<SequenceSource Version=\"4\">{}</SequenceSource>", r("Sequence", "f0000000-0000-0000-0000-000000000001"))),
        // the sequence
        sequence("f0000000-0000-0000-0000-000000000001", "Main Edit", &props, &[(VIDEO, "50"), (AUDIO, "51"), (DATA, "52")], &["53"], Some("40")),
        link,
        markers("40", &["61"]),
        marker(
            "61",
            r#"{"DVAMarker":{"mMarkerID":"m2","mStartTime":{"ticks":508032000000},"mDuration":{"ticks":254016000000},"mType":"Chapter","mName":"Chapter 1","mComment":"intro"}}"#,
        ),
        group(
            "VideoTrackGroup",
            "50",
            &["f1000000-0000-0000-0000-000000000001", "f1000000-0000-0000-0000-000000000002"],
            FPS25,
            "<FrameRect>0,0,1920,1080</FrameRect>",
        ),
        group("AudioTrackGroup", "51", &["f1000000-0000-0000-0000-000000000003"], 5_292_000, ""),
        obj("DataTrackGroup", "52", "<TrackGroup Version=\"1\"><FrameRate>10160640000</FrameRate></TrackGroup>"),
        track("VideoClipTrack", "f1000000-0000-0000-0000-000000000001", Some("Picture"), "<IsLocked>true</IsLocked>", &["100", "101", "102"], &["120"]),
        track("VideoClipTrack", "f1000000-0000-0000-0000-000000000002", None, "<IsMuted>true</IsMuted>", &["103", "104"], &[]),
        // A1 is muted by its mixer fader's M button; the fader is at -6 dB, pan +50, with an insert
        track("AudioClipTrack", "f1000000-0000-0000-0000-000000000003", Some("Dialogue"), "", &["110", "111"], &["121"]).replace(
            "</ClipTrack>",
            &format!(
                "</ClipTrack><AudioTrack Version=\"12\"><ComponentOwner Version=\"1\">{}</ComponentOwner>{}</AudioTrack>",
                r("Components", "250"),
                r("Panner", "252")
            ),
        ),
        chain("AudioComponentChain", "250", &["251", "253"]),
        obj(
            "AudioFader",
            "251",
            &format!("<AudioComponent Version=\"3\"><Component Version=\"7\">{}</Component></AudioComponent>", items("Params", "Param", &["254", "255"])),
        ),
        param("AudioComponentParam", "254", 0, "Volume", "0.501187233627272", None),
        param("AudioComponentParam", "255", 0, "Mute", "true", None),
        audio_filter("253", "8edcd326-33de-4a4d-985b-f1bf48ed1427", Some("Track EQ"), &[]),
        obj(
            "StereoToStereoPanProcessor",
            "252",
            &format!(
                "<PanProcessor Version=\"3\"><AudioComponent Version=\"3\"><Component Version=\"7\">{}</Component></AudioComponent></PanProcessor>",
                items("Params", "Param", &["256"])
            ),
        ),
        // pan automation: the start value is kept, the envelope reported
        param("AudioComponentParam", "256", 0, "Balance", "0.75", Some(&format!("0,0.75,0,0,0,0.3333,0,0.3333;{S},0.25,0,0,0,0.3333,0,0.3333"))),
        // V1: an edit with Motion (position, scale, rotation keyframes, crop) and other effects
        track_item("VideoClipTrackItem", "100", 0, 4 * S, "300", Some("200"), &r("TailTransition", "120")),
        sub_clip("300", "301", "d0000000-0000-0000-0000-000000000001", "Interview"),
        clip(
            "VideoClip",
            "301",
            "21",
            Some((S, 5 * S)),
            "<Node Version=\"1\"><Properties Version=\"1\"><asl.clip.label.name>BE.Prefs.LabelColors.6</asl.clip.label.name></Properties></Node>",
            "",
        ),
        chain("VideoComponentChain", "200", &["210", "211", "212", "213"]),
        video_filter("210", "AE.ADBE Motion", "Motion", &["220", "221", "222", "223"]),
        obj(
            "PointComponentParam",
            "220",
            &format!("<StartKeyframe>{NO_TIME},0.25:0.75,0,0,0,0,0,0,5,4,0,0,0,0</StartKeyframe><Name>Position</Name><ParameterID>1</ParameterID>"),
        ),
        param("VideoComponentParam", "221", 2, "Scale", "150.", None),
        param("VideoComponentParam", "222", 5, "Rotation", "0.", Some(&format!("{S},0.,0,0,0,0.3333,0,0.3333;{},90.,4,0,0,0.3333,0,0.3333;", 3 * S))),
        param("VideoComponentParam", "223", 8, "Crop Left", "10.", None),
        video_filter("211", "AE.ADBE Drop Shadow", "Drop Shadow", &["224"]),
        param("VideoComponentParam", "224", 2, "Opacity", "127.5", None),
        video_filter("212", "AE.ADBE Lumetri", "Lumetri Color", &["225"]),
        param("VideoComponentParam", "225", 11, "Exposure", "1.5", None),
        video_filter("213", "AE.Some Plugin", "Fancy Glow", &[]),
        // a reversed, disabled clip at 200% after a cross dissolve
        track_item("VideoClipTrackItem", "101", 4 * S, 6 * S, "302", None, &format!("{}<IsMuted>true</IsMuted>", r("HeadTransition", "120"))),
        sub_clip("302", "303", "d0000000-0000-0000-0000-000000000001", "Interview Rev"),
        clip("VideoClip", "303", "21", Some((0, 4 * S)), "<PlaybackSpeed>2</PlaybackSpeed><PlayBackwards>true</PlayBackwards>", ""),
        transition("VideoTransitionTrackItem", "120", 3 * S + S / 2, 4 * S + S / 2, S / 2, true, true, "Cross Dissolve"),
        // a frame hold
        track_item("VideoClipTrackItem", "102", 6 * S, 8 * S, "304", None, ""),
        sub_clip("304", "305", "d0000000-0000-0000-0000-000000000001", "Interview Hold"),
        clip("VideoClip", "305", "21", Some((0, 2 * S)), "", &format!("<FrameHold>4</FrameHold><FrameHoldStart>{}</FrameHoldStart>", 2 * S)),
        // V2: an adjustment layer (media time origin at one hour) with opacity keyframes and Screen
        track_item("VideoClipTrackItem", "103", 0, 8 * S, "306", Some("201"), ""),
        sub_clip("306", "307", "d0000000-0000-0000-0000-00000000000c", "Adjustment Layer"),
        clip("VideoClip", "307", "30", Some((3600 * S, 3608 * S)), "", "<AdjustmentLayer>true</AdjustmentLayer>"),
        chain("VideoComponentChain", "201", &["214"]),
        video_filter("214", "AE.ADBE Opacity", "Opacity", &["226", "227"]),
        param(
            "VideoComponentParam",
            "226",
            1,
            "Opacity",
            "100.",
            Some(&format!("{},0.,0,0,0,0.1666,0,0.1666;{},100.,0,0,0,0.1666,0,0.1666", 3600 * S, 3601 * S)),
        ),
        param("VideoComponentParam", "227", 3, "Blend Mode", "8", None),
        track_item("VideoClipTrackItem", "104", 8 * S, 10 * S, "308", None, ""),
        sub_clip("308", "309", "d0000000-0000-0000-0000-00000000000b", "Blue Matte"),
        clip("VideoClip", "309", "28", Some((0, 2 * S)), "", ""),
        // A1: the interview's audio (linked, -6 dB gain, -10 dB volume) and music fading out
        track_item("AudioClipTrackItem", "110", 0, 4 * S, "310", Some("202"), ""),
        sub_clip("310", "311", "d0000000-0000-0000-0000-000000000001", "Interview"),
        clip("AudioClip", "311", "23", Some((S, 5 * S)), "", "<Gain>0.501187233627272</Gain>"),
        chain("AudioComponentChain", "202", &["215", "216"]),
        audio_filter("215", "Internal Volume Stereo", None, &["228", "229"]),
        param("AudioComponentParam", "228", 1, "Mute", "false", None),
        param("AudioComponentParam", "229", 2, "Level", "0.0562341325190349", None),
        audio_filter("216", "3:abcdef01 9182faeb 4d616e75 42776d72", Some("Elgato Voice Focus"), &[]),
        track_item("AudioClipTrackItem", "111", 4 * S, 10 * S, "312", Some("203"), &r("TailTransition", "121")),
        sub_clip("312", "313", "d0000000-0000-0000-0000-000000000002", "Music"),
        clip("AudioClip", "313", "26", Some((0, 6 * S)), "", ""),
        chain("AudioComponentChain", "203", &["217"]),
        audio_filter("217", "Internal Volume Stereo", None, &["230"]),
        // audio Level keyframes come without IsTimeVarying
        obj(
            "AudioComponentParam",
            "230",
            &format!(
                "<StartKeyframe>{NO_TIME},0.177827941,0,0,0,0,0,0</StartKeyframe><Keyframes>0,0.177827941,0,0,0,0.3333,0,0.3333;{},0.0177827941,0,0,0,0.3333,0,0.3333</Keyframes><Name>Level</Name>",
                5 * S
            ),
        ),
        transition("AudioTransitionTrackItem", "121", 9 * S, 10 * S, S, true, false, "Constant Power"),
    ])
}

fn opts() -> ImportOptions {
    ImportOptions { base_dir: Some("C:\\proj".into()), name: Some("Synthetic".into()), ..Default::default() }
}

fn gzip(data: &[u8]) -> Vec<u8> {
    // gzip framing (RFC 1952) around raw DEFLATE, with the CRC-32 trailer
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xedb8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    out.extend(miniz_oxide::deflate::compress_to_vec(data, 6));
    out.extend((!crc).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

fn by_name<'a>(p: &'a Project, name: &str) -> (ItemId, &'a filmcraft_project::ProjectItem) {
    let it = p.items.values().find(|i| i.name == name).unwrap_or_else(|| panic!("no item {name}"));
    (it.id, it)
}

fn float(c: &TrackItem, effect: &str, param: &str) -> f64 {
    match c.effect(effect).and_then(|e| e.param(param)).map(|p| &p.value) {
        Some(ParamValue::Float(v)) => *v,
        other => panic!("{effect}.{param} = {other:?}"),
    }
}

#[test]
fn imports_a_project() {
    let xml = sample();
    let r = prproj::import(xml.as_bytes(), &opts()).unwrap();
    let p = &r.imported.project;
    assert_eq!(p.name, "Synthetic");

    // bins
    let names: Vec<String> = p
        .root
        .children
        .iter()
        .map(|c| match c {
            BinEntry::Bin(b) => format!("bin:{}", b.name),
            BinEntry::Item(i) => p.item(*i).map(|i| i.name.clone()).unwrap_or_default(),
        })
        .collect();
    assert_eq!(names, ["bin:Footage", "Main Edit", "Blue Matte", "Adjustment Layer"]);

    // media
    let (iv, interview) = by_name(p, "Interview");
    assert_eq!(interview.label, Label::Iris);
    let m = interview.as_media().unwrap();
    assert_eq!(m.media, MediaRef::File { path: "C:\\media\\interview.mov".into() });
    let v = m.info.video.as_ref().unwrap();
    assert_eq!((v.width, v.height, v.frame_rate), (1920, 1080, FrameRate::FPS_25));
    assert_eq!((m.info.audio_streams[0].sample_rate, m.info.audio_streams[0].channels), (48_000, 2));
    assert_eq!(m.info.duration, Tick(10 * S));
    assert_eq!(m.markers.len(), 1);
    assert_eq!((m.markers[0].start, m.markers[0].color, m.markers[0].name.as_str()), (Tick(S), Label::Rose, "Slate"));
    assert_eq!(r.relative_paths, vec![(iv, "C:\\proj\\media\\interview.mov".to_string())]);
    let (_, music) = by_name(p, "Music");
    assert_eq!(music.label, Label::Caribbean);
    assert_eq!(music.as_media().unwrap().media, MediaRef::File { path: "C:\\proj\\music.wav".into() });
    let (_, matte) = by_name(p, "Blue Matte");
    match &matte.as_media().unwrap().media {
        MediaRef::Generator(Generator::ColorMatte { color }) => assert_eq!(color.map(|c| (c * 255.0).round() as u8), [0, 23, 227, 255]),
        other => panic!("{other:?}"),
    }
    let (_, adj) = by_name(p, "Adjustment Layer");
    match adj.kind {
        ItemKind::AdjustmentLayer { width, height, rate, duration } => {
            assert_eq!((width, height, rate), (1920, 1080, FrameRate::FPS_25));
            assert!(duration >= Tick(8 * S), "grown to cover its use: {duration:?}");
        }
        ref other => panic!("{other:?}"),
    }

    // the sequence
    let (sid, seq_item) = by_name(p, "Main Edit");
    assert_eq!(r.imported.sequences, vec![sid]);
    assert_eq!(seq_item.label, Label::Forest);
    let seq = seq_item.as_sequence().unwrap();
    let st = &seq.settings;
    assert_eq!((st.width, st.height, st.frame_rate, st.sample_rate, st.drop_frame), (1920, 1080, FrameRate::FPS_25, 48_000, false));
    assert_eq!((seq.mark_in, seq.mark_out), (Some(Tick(S)), None));
    assert_eq!(seq.markers.len(), 1);
    let mk = &seq.markers[0];
    assert_eq!((mk.start, mk.duration, mk.kind, mk.color, mk.comment.as_str()), (Tick(2 * S), Tick(S), MarkerKind::Chapter, Label::Green, "intro"));
    assert_eq!((seq.video_tracks.len(), seq.audio_tracks.len()), (2, 1));

    // V1
    let v1 = &seq.video_tracks[0];
    assert_eq!((v1.name.as_str(), v1.locked, v1.enabled), ("Picture", true, true));
    assert_eq!(v1.items.len(), 3);
    let c0 = &v1.items[0];
    assert_eq!((c0.start, c0.duration, c0.source_in, c0.speed, c0.label), (Tick::ZERO, Tick(4 * S), Tick(S), 1.0, Label::Rose));
    assert_eq!(c0.item, iv);
    match c0.effect("motion").and_then(|e| e.param("position")).map(|p| &p.value) {
        Some(ParamValue::Vec2(v)) => assert_eq!((v.x, v.y), (480.0, 810.0)),
        other => panic!("{other:?}"),
    }
    assert_eq!(float(c0, "motion", "scale"), 150.0);
    let rot = c0.effect("motion").and_then(|e| e.param("rotation")).unwrap();
    assert_eq!(
        rot.keyframes.iter().map(|k| (k.time, k.value.clone(), k.interp)).collect::<Vec<_>>(),
        [(Tick(S), ParamValue::Float(0.0), Interpolation::Linear), (Tick(3 * S), ParamValue::Float(90.0), Interpolation::Hold)]
    );
    assert_eq!(float(c0, "crop", "left"), 10.0);
    assert!((float(c0, "drop_shadow", "opacity") - 50.0).abs() < 1e-9);
    assert_eq!(float(c0, "lumetri", "exposure"), 1.5);
    assert!(r.report.mentions("Fancy Glow"));
    let c1 = &v1.items[1];
    assert_eq!((c1.start, c1.duration, c1.speed, c1.reverse, c1.enabled), (Tick(4 * S), Tick(2 * S), 2.0, true, false));
    assert_eq!(v1.items[2].frame_hold, Some(Tick(2 * S)));
    assert_eq!(v1.transitions.len(), 1);
    let tr = &v1.transitions[0];
    assert_eq!((tr.start, tr.duration, tr.effect.effect.as_str(), tr.align), (Tick(3 * S + S / 2), Tick(S), "cross_dissolve", TransitionAlign::CenterAtCut));
    assert_eq!((tr.from, tr.to), (Some(c0.id), Some(c1.id)));

    // V2: output off; adjustment layer rebased to zero, keyframes with it
    let v2 = &seq.video_tracks[1];
    assert!(!v2.enabled);
    let adj_clip = &v2.items[0];
    assert_eq!(adj_clip.source_in, Tick::ZERO);
    let op = adj_clip.effect("opacity").and_then(|e| e.param("opacity")).unwrap();
    assert_eq!(
        op.keyframes.iter().map(|k| (k.time, k.value.clone())).collect::<Vec<_>>(),
        [(Tick::ZERO, ParamValue::Float(0.0)), (Tick(S), ParamValue::Float(100.0))]
    );
    assert_eq!(adj_clip.effect("opacity").and_then(|e| e.param("blend")).map(|p| p.value.clone()), Some(ParamValue::Choice(8)));
    assert_eq!(p.item(v2.items[1].item).map(|i| i.name.as_str()), Some("Blue Matte"));

    // A1: muted; linked to the video; gain and volume in dB
    let a1 = &seq.audio_tracks[0];
    assert_eq!((a1.name.as_str(), a1.muted, a1.kind, a1.pan), ("Dialogue", true, TrackKind::Audio, 50.0));
    assert!((a1.volume_db + 6.0).abs() < 1e-3, "{}", a1.volume_db);
    assert!(r.report.mentions("audio track effect \"Track EQ\""));
    let a0 = &a1.items[0];
    assert!(c0.link.is_some());
    assert_eq!(a0.link, c0.link);
    assert!((a0.gain_db + 6.0).abs() < 1e-3, "{}", a0.gain_db);
    assert!((float(a0, "volume", "level") + 10.0).abs() < 1e-6);
    assert!(r.report.mentions("Elgato Voice Focus"));
    let lv = a1.items[1].effect("volume").and_then(|e| e.param("level")).unwrap();
    let kf: Vec<(Tick, i64)> = lv.keyframes.iter().map(|k| (k.time, k.value.as_f64().unwrap_or(f64::NAN).round() as i64)).collect();
    assert_eq!(kf, [(Tick::ZERO, 0), (Tick(5 * S), -20)]);
    assert_eq!(a1.transitions.len(), 1);
    let fade = &a1.transitions[0];
    assert_eq!((fade.effect.effect.as_str(), fade.align, fade.from, fade.to), ("constant_power", TransitionAlign::EndAtCut, Some(a1.items[1].id), None));

    for t in seq.all_tracks() {
        t.check().unwrap();
    }
}

#[test]
fn gzip_and_plain_xml_import_alike() {
    let xml = sample();
    let gz = gzip(xml.as_bytes());
    assert!(prproj::sniff(&gz));
    assert!(prproj::sniff(xml.as_bytes()));
    let a = prproj::import(&gz, &opts()).unwrap();
    let b = prproj::import(xml.as_bytes(), &opts()).unwrap();
    // (auto anchors are NaN, which never compares equal)
    assert_eq!(format!("{:?}", a.imported), format!("{:?}", b.imported));
    assert!(a.report.entries.iter().all(|e| !e.message.contains("checksum")));
}

#[test]
fn not_a_premiere_project() {
    for bad in [&b""[..], b"hello", b"<xmeml version=\"5\"/>", b"\x1f\x8b\x08\x00garbage", b"<PremiereData Version=\"3\"><unclosed>"] {
        assert!(prproj::import(bad, &opts()).is_err(), "{:?}", String::from_utf8_lossy(bad));
    }
    // no RootProjectItem: an empty document, not a crash
    assert!(prproj::import(document(&[]).as_bytes(), &opts()).is_err());
}

#[test]
fn self_nesting_sequences_are_cut() {
    // A nests B and B nests A: Premiere never writes this; a damaged file could.
    let seq = |uid: &str, name: &str, vg: &str| sequence(uid, name, "", &[(VIDEO, vg)], &[], None);
    let xml = document(&[
        project_item(
            "RootProjectItem",
            "a0000000-0000-0000-0000-000000000001",
            "Root Bin",
            None,
            &format!(
                "<ProjectItemContainer Version=\"1\">{}</ProjectItemContainer>",
                items("Items", "Item", &["c0000000-0000-0000-0000-00000000000a", "c0000000-0000-0000-0000-00000000000b"])
            ),
        ),
        clip_item("c0000000-0000-0000-0000-00000000000a", "A", None, "d0000000-0000-0000-0000-00000000000a"),
        clip_item("c0000000-0000-0000-0000-00000000000b", "B", None, "d0000000-0000-0000-0000-00000000000b"),
        master("d0000000-0000-0000-0000-00000000000a", "A", &["31"], ""),
        clip("VideoClip", "31", "32", None, "", ""),
        obj("VideoSequenceSource", "32", &format!("<SequenceSource Version=\"4\">{}</SequenceSource>", r("Sequence", "f0000000-0000-0000-0000-00000000000a"))),
        master("d0000000-0000-0000-0000-00000000000b", "B", &["33"], ""),
        clip("VideoClip", "33", "34", None, "", ""),
        obj("VideoSequenceSource", "34", &format!("<SequenceSource Version=\"4\">{}</SequenceSource>", r("Sequence", "f0000000-0000-0000-0000-00000000000b"))),
        seq("f0000000-0000-0000-0000-00000000000a", "A", "50"),
        seq("f0000000-0000-0000-0000-00000000000b", "B", "51"),
        group("VideoTrackGroup", "50", &["f1000000-0000-0000-0000-00000000000a"], FPS25, "<FrameRect>0,0,1920,1080</FrameRect>"),
        group("VideoTrackGroup", "51", &["f1000000-0000-0000-0000-00000000000b"], FPS25, "<FrameRect>0,0,1920,1080</FrameRect>"),
        // the clip that gets cut has a fade out
        track("VideoClipTrack", "f1000000-0000-0000-0000-00000000000a", None, "", &["100"], &["120"]),
        transition("VideoTransitionTrackItem", "120", S / 2, S, S / 2, true, false, "Cross Dissolve"),
        track("VideoClipTrack", "f1000000-0000-0000-0000-00000000000b", None, "", &["101"], &[]),
        track_item("VideoClipTrackItem", "100", 0, S, "300", None, &r("TailTransition", "120")),
        sub_clip("300", "33", "d0000000-0000-0000-0000-00000000000b", "B"),
        track_item("VideoClipTrackItem", "101", 0, S, "301", None, ""),
        sub_clip("301", "31", "d0000000-0000-0000-0000-00000000000a", "A"),
    ]);
    let r = prproj::import(xml.as_bytes(), &opts()).unwrap();
    assert!(r.report.mentions("nested a sequence inside itself"));
    let p = &r.imported.project;
    let total: usize = p.sequences().filter_map(|i| i.as_sequence()).map(|s| s.all_tracks().map(|t| t.items.len()).sum::<usize>()).sum();
    assert_eq!(total, 1, "one of the two nestings is cut");
    // no transition is left pointing at a removed clip (the fade lost its only clip: gone)
    for t in p.sequences().filter_map(|i| i.as_sequence()).flat_map(|s| s.all_tracks()) {
        for tr in &t.transitions {
            assert!(tr.from.is_some() || tr.to.is_some());
            for c in [tr.from, tr.to].into_iter().flatten() {
                assert!(t.items.iter().any(|i| i.id == c), "transition points at a removed clip");
            }
        }
    }
    let transitions: usize = p.sequences().filter_map(|i| i.as_sequence()).flat_map(|s| s.all_tracks()).map(|t| t.transitions.len()).sum();
    assert_eq!(transitions, 0);
}

#[test]
fn invalid_utf8_is_refused() {
    let mut bytes = sample().into_bytes();
    let at = bytes.len() / 2;
    bytes[at] = 0xff;
    for b in [bytes.clone(), gzip(&bytes)] {
        let e = prproj::import(&b, &opts()).unwrap_err().to_string();
        assert!(e.contains("UTF-8"), "{e}");
    }
}

#[test]
fn far_away_points_and_endless_keyframes_are_bounded() {
    // a finite fraction that overflows once scaled to pixels: the position is not imported
    let xml = sample().replace("0.25:0.75", "1e308:0.75");
    let r = prproj::import(xml.as_bytes(), &opts()).unwrap();
    let seq = r.imported.project.sequences().find_map(|i| i.as_sequence()).unwrap();
    let c0 = &seq.video_tracks[0].items[0];
    match c0.effect("motion").and_then(|e| e.param("position")).map(|p| &p.value) {
        Some(ParamValue::Vec2(v)) => assert!(!(v.x.is_infinite() || v.y.is_infinite()), "{v:?}"),
        other => panic!("{other:?}"),
    }
    // a scale far beyond our range is clamped to it
    let xml = sample().replace(&format!("{NO_TIME},150.,"), &format!("{NO_TIME},1e300,"));
    let r = prproj::import(xml.as_bytes(), &opts()).unwrap();
    let seq = r.imported.project.sequences().find_map(|i| i.as_sequence()).unwrap();
    assert_eq!(float(&seq.video_tracks[0].items[0], "motion", "scale"), 10_000.0);
    // more keyframes than the cap: the first ones come in and the report says so
    let keys: String = (0..100_001).map(|i| format!("{},50.,0,0,0,0.3333,0,0.3333;", 3600 * S + i)).collect();
    let xml = sample().replace(&format!("{},0.,0,0,0,0.1666,0,0.1666;{},100.,0,0,0,0.1666,0,0.1666", 3600 * S, 3601 * S), &keys);
    let r = prproj::import(xml.as_bytes(), &opts()).unwrap();
    assert!(r.report.mentions("more than 100000 keyframes"), "{}", r.report);
    // track pan automation is reported (the sample's A1 pans over time)
    assert!(r.report.mentions("pan automation"));
}

#[test]
fn deep_and_self_containing_bins_terminate() {
    // 200 nested bins, the innermost listing the root again
    let mut objs = vec![];
    let uid = |i: usize| format!("b0000000-0000-0000-0000-{i:012}");
    objs.push(project_item(
        "RootProjectItem",
        "a0000000-0000-0000-0000-000000000001",
        "Root Bin",
        None,
        &format!("<ProjectItemContainer Version=\"1\">{}</ProjectItemContainer>", items("Items", "Item", &[&uid(0)])),
    ));
    for i in 0..200 {
        let next = if i == 199 { "a0000000-0000-0000-0000-000000000001".to_string() } else { uid(i + 1) };
        objs.push(bin(&uid(i), &format!("Bin {i}"), &[&next]));
    }
    let r = prproj::import(document(&objs).as_bytes(), &opts()).unwrap();
    assert!(r.report.mentions("nested deeper"));
}

#[test]
fn deeply_nested_xml_does_not_overflow_the_stack() {
    let nest = |n: usize| format!("{}{}", "<Node>".repeat(n), "</Node>".repeat(n));
    // far deeper than any project: refused before parsing (the XML parser recurses per level)
    let deep = nest(100_000);
    for xml in [
        sample().replace("<PremiereData Version=\"3\">\n", &format!("<PremiereData Version=\"3\">\n<Deep>{deep}</Deep>\n")),
        sample().replace("<Name>Main Edit</Name>", &format!("<Name>Main Edit</Name>{deep}")),
        format!("<PremiereData Version=\"3\">{}", "<a t=\"/>\">".repeat(100_000)),
    ] {
        let e = prproj::import(xml.as_bytes(), &opts()).unwrap_err().to_string();
        assert!(e.contains("nested"), "{e}");
    }
    // just under the limit parses, even on a small stack (the app opens projects on its UI thread)
    let ok = sample().replace("<Name>Main Edit</Name>", &format!("<Name>Main Edit</Name>{}", nest(prproj::MAX_XML_DEPTH - 2)));
    let r = std::thread::Builder::new().stack_size(256 << 10).spawn(move || prproj::import(ok.as_bytes(), &opts()).is_ok()).unwrap().join().unwrap();
    assert!(r);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// Damaged projects (truncated, bytes changed) import or fail cleanly; they never panic.
    #[test]
    fn damaged_projects_never_panic(cut in 0.0f64..1.0, flips in proptest::collection::vec((0.0f64..1.0, any::<u8>()), 0..8), compressed in any::<bool>()) {
        let xml = sample();
        let mut bytes = if compressed { gzip(xml.as_bytes()) } else { xml.into_bytes() };
        let n = bytes.len();
        for (at, v) in flips {
            let i = ((n as f64 * at) as usize).min(n - 1);
            bytes[i] ^= v;
        }
        bytes.truncate(((n as f64) * cut) as usize + 1);
        let _ = prproj::sniff(&bytes);
        let _ = prproj::import(&bytes, &opts());
    }

    /// Changed numbers anywhere in the document (huge, negative, fractional) never panic.
    #[test]
    fn hostile_numbers_never_panic(picks in proptest::collection::vec((0usize..400, prop_oneof![
        Just("-1".to_string()), Just("0".to_string()), Just("9223372036854775807".to_string()), Just("-9223372036854775808".to_string()),
        Just("1e308".to_string()), Just("NaN".to_string()), Just("0.5:0.5:0.5".to_string()), Just("".to_string())
    ]), 1..12)) {
        let xml = sample();
        // replace the k-th run of digits with a hostile value
        let mut runs = vec![];
        let b = xml.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i].is_ascii_digit() {
                let s = i;
                while i < b.len() && b[i].is_ascii_digit() { i += 1; }
                runs.push((s, i));
            } else {
                i += 1;
            }
        }
        let mut out = xml.clone();
        let mut chosen: Vec<(usize, usize, String)> = picks.into_iter().filter_map(|(k, v)| runs.get(k * runs.len() / 400).map(|&(s, e)| (s, e, v))).collect();
        chosen.sort_by_key(|c| std::cmp::Reverse(c.0));
        chosen.dedup_by_key(|c| c.0);
        for (s, e, v) in chosen {
            out.replace_range(s..e, &v);
        }
        let _ = prproj::import(out.as_bytes(), &opts());
    }
}
