//! Minimal XML writer and read helpers over `roxmltree`.

use std::collections::HashMap;
use std::fmt::Write as _;

use roxmltree::Node;

pub(crate) fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c if (c as u32) < 0x20 && !matches!(c, '\n' | '\r' | '\t') => {}
            c => o.push(c),
        }
    }
    o
}

/// Pretty-printing XML writer (two-space indent, one element per line).
pub(crate) struct XmlWriter {
    out: String,
    stack: Vec<String>,
}

impl XmlWriter {
    pub fn new(doctype: Option<&str>) -> Self {
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        if let Some(d) = doctype {
            let _ = writeln!(out, "<!DOCTYPE {d}>");
        }
        Self { out, stack: Vec::new() }
    }

    fn indent(&mut self) {
        for _ in 0..self.stack.len() {
            self.out.push_str("  ");
        }
    }

    fn start_tag(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.indent();
        self.out.push('<');
        self.out.push_str(name);
        for (k, v) in attrs {
            let _ = write!(self.out, " {k}=\"{}\"", escape(v));
        }
    }

    pub fn open(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.start_tag(name, attrs);
        self.out.push_str(">\n");
        self.stack.push(name.to_string());
    }

    pub fn close(&mut self) {
        let Some(name) = self.stack.pop() else { return };
        self.indent();
        let _ = writeln!(self.out, "</{name}>");
    }

    pub fn empty(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.start_tag(name, attrs);
        self.out.push_str("/>\n");
    }

    /// `<name attrs>value</name>`.
    pub fn text_attrs(&mut self, name: &str, attrs: &[(&str, &str)], value: &str) {
        self.start_tag(name, attrs);
        let _ = writeln!(self.out, ">{}</{name}>", escape(value));
    }

    pub fn text(&mut self, name: &str, value: impl std::fmt::Display) {
        self.text_attrs(name, &[], &value.to_string());
    }

    pub fn finish(mut self) -> String {
        while !self.stack.is_empty() {
            self.close();
        }
        self.out
    }
}

pub(crate) fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

pub(crate) fn children<'a, 'i>(n: Node<'a, 'i>, name: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

pub(crate) fn elements<'a, 'i>(n: Node<'a, 'i>) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children().filter(|c| c.is_element())
}

/// Trimmed text of a node's direct text content.
pub(crate) fn text<'a>(n: Node<'a, '_>) -> &'a str {
    n.text().map(str::trim).unwrap_or("")
}

/// Text of the element at `path` below `n`.
pub(crate) fn path_text<'a>(n: Node<'a, '_>, path: &[&str]) -> Option<&'a str> {
    let mut cur = n;
    for p in path {
        cur = child(cur, p)?;
    }
    Some(text(cur))
}

pub(crate) fn child_text<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    child(n, name).map(text)
}

pub(crate) fn child_i64(n: Node<'_, '_>, name: &str) -> Option<i64> {
    child_text(n, name).and_then(|t| t.parse::<i64>().ok().or_else(|| t.parse::<f64>().ok().map(|f| f.round() as i64)))
}

pub(crate) fn child_f64(n: Node<'_, '_>, name: &str) -> Option<f64> {
    child_text(n, name).and_then(|t| t.parse::<f64>().ok())
}

pub(crate) fn child_bool(n: Node<'_, '_>, name: &str) -> Option<bool> {
    child_text(n, name).map(|t| t.eq_ignore_ascii_case("true") || t == "1")
}

pub(crate) fn bool_str(b: bool) -> &'static str {
    if b { "TRUE" } else { "FALSE" }
}

/// Format an f64 compactly (no trailing zeros, no exponent for normal ranges).
pub(crate) fn num(v: f64) -> String {
    if v == v.round() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    if !v.is_finite() {
        return "0".into();
    }
    // Shortest representation that parses back to the same f64.
    format!("{v}")
}

/// Deepest element nesting [`parse`] accepts. Real FCP7 XML and FCPXML documents nest 11–16 deep
/// (FCP7 adds about five levels per nested sequence written inline and two per bin). The XML parser
/// and the importers recurse per level, up to 2 KB of stack each in debug builds (FCPXML connected
/// clips), so a deeper document is refused before it can exhaust the stack.
pub const MAX_XML_DEPTH: usize = 128;

/// Entity expansions the XML parser nests inside one another (roxmltree's documented limit).
const ENTITY_NESTING: usize = 10;

/// Nesting each entity reference expands to, by entity name.
type Expansions<'a> = HashMap<&'a [u8], usize>;

/// An upper bound on the element nesting the XML parser reaches in `xml`; counting stops once it
/// passes `limit`. Exact for the well-formed prefix of a document, which is all the parser gets
/// through: quotes, comments, CDATA, processing instructions and the DOCTYPE are skipped like the
/// parser does, and entity references in text count the nesting their expansions reach.
pub(crate) fn nesting_depth(xml: &[u8], limit: usize) -> usize {
    content_depth(xml, limit, None)
}

/// Index just past the first `pat` at or after `from`.
fn find(xml: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    xml.get(from..)?.windows(pat.len()).position(|w| w == pat).map(|p| from + p + pat.len())
}

/// Length of the XML name `s` starts with (any non-ASCII byte counts: a superset of the parser's).
fn name_len(s: &[u8]) -> usize {
    s.iter().take_while(|&&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b':' | b'-' | b'.') || c >= 0x80).count()
}

/// Index just past the first of `stops` at or after `from` outside a quoted literal, and that byte.
/// `literal` sees each literal's contents.
fn skip_quoted<'a>(xml: &'a [u8], from: usize, stops: &[u8], mut literal: impl FnMut(&'a [u8])) -> Option<(usize, u8)> {
    let mut i = from;
    loop {
        let c = *xml.get(i)?;
        if c == b'"' || c == b'\'' {
            let end = i + 1 + xml.get(i + 1..)?.iter().position(|&q| q == c)?;
            literal(xml.get(i + 1..end)?);
            i = end;
        } else if stops.contains(&c) {
            return Some((i + 1, c));
        }
        i += 1;
    }
}

/// The deepest nesting the entity references (`&name;`) in `text`, at `depth`, expand to.
fn references(text: &[u8], depth: usize, expansions: &Expansions) -> usize {
    let mut max = 0;
    let mut rest = text;
    while let Some(at) = rest.iter().position(|&c| c == b'&') {
        rest = rest.get(at + 1..).unwrap_or_default();
        let n = name_len(rest);
        if rest.get(n) == Some(&b';')
            && let Some(&e) = rest.get(..n).and_then(|name| expansions.get(name))
        {
            max = max.max(depth.saturating_add(e));
        }
        rest = rest.get(n..).unwrap_or_default();
    }
    max
}

/// The deepest element nesting in `xml` read as element content, counting stops once it passes
/// `limit`. `entities` are the expansions of the entities of the document `xml` is an entity value
/// of; for the document itself (`None`), its DOCTYPE is read like the parser does.
fn content_depth<'a>(xml: &'a [u8], limit: usize, entities: Option<&Expansions<'a>>) -> usize {
    let mut declared = Expansions::new();
    let (mut depth, mut max, mut i) = (0usize, 0usize, 0usize);
    loop {
        let next = xml.get(i..).and_then(|s| s.iter().position(|&c| c == b'<')).map(|p| i + p);
        // text up to the next markup, where entity references expand
        if let Some(text) = xml.get(i..next.unwrap_or(xml.len())) {
            max = max.max(references(text, depth, entities.unwrap_or(&declared)));
            if max > limit {
                return max;
            }
        }
        let Some(at) = next else { break };
        let rest = xml.get(at..).unwrap_or_default();
        i = match rest.get(1) {
            None => break,
            Some(b'/') => {
                depth = depth.saturating_sub(1);
                at + 2
            }
            Some(b'?') => find(xml, at + 2, b"?>").unwrap_or(xml.len()),
            Some(b'!') if rest.starts_with(b"<!--") => find(xml, at + 4, b"-->").unwrap_or(xml.len()),
            Some(b'!') if rest.starts_with(b"<![CDATA[") => find(xml, at + 9, b"]]>").unwrap_or(xml.len()),
            Some(b'!') if entities.is_none() && rest.starts_with(b"<!DOCTYPE") => match doctype(xml, at + 9, limit) {
                Some((end, expansions)) => {
                    declared = expansions;
                    end
                }
                None => break,
            },
            Some(b'!') => find(xml, at + 2, b">").unwrap_or(xml.len()),
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
                max = max.max(depth + 1);
                if max > limit {
                    return max;
                }
                if !self_closing {
                    depth += 1;
                }
                j + 1
            }
        };
    }
    max
}

/// Skips a DOCTYPE from just after `<!DOCTYPE` as the parser reads it (its external ID's literals
/// may hold `>` or `<!--`). Returns where the document continues and the nesting a reference to
/// each internal-subset entity expands to, or `None` where the parser fails.
fn doctype(xml: &[u8], from: usize, limit: usize) -> Option<(usize, Expansions<'_>)> {
    let (mut i, end) = skip_quoted(xml, from, b"[>", |_| {})?;
    if end == b'>' {
        return Some((i, Expansions::new()));
    }
    let space = |i: usize| xml.get(i..).map_or(0, |s| s.iter().take_while(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r')).count());
    // (name, value) of each entity with a literal value (the parser loads no external ones)
    let mut entities = Vec::new();
    loop {
        i += space(i);
        let rest = xml.get(i..)?;
        i = if rest.starts_with(b"<!ENTITY") {
            // '<!ENTITY' S ('%' S)? Name S (EntityValue | ExternalID) S? '>'
            let mut j = i + 8;
            j += space(j);
            if xml.get(j) == Some(&b'%') {
                j += 1 + space(j + 1);
            }
            let name = xml.get(j..j + name_len(xml.get(j..)?))?;
            j += name.len();
            j += space(j);
            let quoted = matches!(xml.get(j), Some(b'"' | b'\''));
            let mut value = None;
            let end = skip_quoted(xml, j, b">", |lit| {
                value.get_or_insert(lit);
            })?
            .0;
            if let (true, Some(value)) = (quoted, value) {
                entities.push((name, value));
            }
            end
        } else if rest.starts_with(b"<!--") {
            find(xml, i + 4, b"-->")?
        } else if rest.starts_with(b"<?") {
            find(xml, i + 2, b"?>")?
        } else if rest.starts_with(b"<!ELEMENT") || rest.starts_with(b"<!ATTLIST") || rest.starts_with(b"<!NOTATION") {
            // the parser skips these to their first '>', quoted or not
            find(xml, i + 2, b">")?
        } else if rest.starts_with(b"]") {
            let j = i + 1 + space(i + 1);
            return (xml.get(j) == Some(&b'>')).then(|| (j + 1, expansions(&entities, limit)));
        } else {
            return None;
        };
    }
}

/// The nesting a reference to each entity expands to: its value's own, and through the entities it
/// references in turn, as deep as the parser nests expansions.
fn expansions<'a>(entities: &[(&'a [u8], &'a [u8])], limit: usize) -> Expansions<'a> {
    // after round k, each entity's nesting with k - 1 further expansions inside it
    let mut inner = Expansions::new();
    for _ in 0..ENTITY_NESTING {
        let mut next = Expansions::new();
        for &(name, value) in entities {
            let d = content_depth(value, limit, Some(&inner));
            let e = next.entry(name).or_insert(0);
            *e = (*e).max(d);
        }
        if next == inner {
            break;
        }
        inner = next;
    }
    inner
}

/// The deepest element nesting in a parsed document, without recursion.
fn tree_depth(doc: &roxmltree::Document<'_>) -> usize {
    // elements come in document order, so a node's ancestors are on `path` when it is reached
    let mut path: Vec<roxmltree::NodeId> = Vec::new();
    let mut max = 0;
    for n in doc.descendants().filter(|n| n.is_element()) {
        let parent = n.parent().map(|p| p.id());
        while path.last().is_some_and(|&top| Some(top) != parent) {
            path.pop();
        }
        path.push(n.id());
        max = max.max(path.len());
    }
    max
}

/// Parses `text`, refusing documents nested deeper than [`MAX_XML_DEPTH`].
pub(crate) fn parse(text: &str) -> Result<roxmltree::Document<'_>, String> {
    let too_deep = || format!("elements are nested more than {MAX_XML_DEPTH} deep (the file is damaged)");
    // the scan bounds the parser's recursion ...
    if nesting_depth(text.as_bytes(), MAX_XML_DEPTH) > MAX_XML_DEPTH {
        return Err(too_deep());
    }
    let opts = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(text, opts).map_err(|e| e.to_string())?;
    // ... and this the importers', which recurse over the tree, however the parser built it
    if tree_depth(&doc) > MAX_XML_DEPTH {
        return Err(too_deep());
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_roundtrip() {
        let mut w = XmlWriter::new(Some("xmeml"));
        w.open("a", &[("x", "1 & 2")]);
        w.text("b", "<hi>");
        w.empty("c", &[]);
        let s = w.finish();
        let d = parse(&s).unwrap();
        let a = d.root_element();
        assert_eq!(a.attribute("x"), Some("1 & 2"));
        assert_eq!(child_text(a, "b"), Some("<hi>"));
        assert!(child(a, "c").is_some());
        assert_eq!(num(1.5), "1.5");
        assert_eq!(num(100.0), "100");
        assert_eq!(num(-0.0), "0");
        assert_eq!(num(0.1 + 0.2).parse::<f64>().unwrap(), 0.1 + 0.2);
    }

    #[test]
    fn nesting() {
        let d = |s: &str| nesting_depth(s.as_bytes(), 1000);
        // the element nesting the parser reaches, for documents it accepts
        let parsed = |s: &str| {
            let doc = parse(s).unwrap();
            let depth = doc.descendants().map(|n| n.ancestors().filter(|a| a.is_element()).count()).max().unwrap_or(0);
            assert_eq!(tree_depth(&doc), depth, "{s}");
            depth
        };
        assert_eq!(parsed("<a><b><c/></b><d><e><f/></e></d><g/></a>"), 4);
        for s in [
            "<a><b/><c x=\"1\"><d></d></c></a>",
            "<?xml version=\"1.0\"?><!-- <x><x><x> --><a><![CDATA[<b><c>]]></a>",
            // '>' and "/>" inside attribute values do not end the tag
            "<a t=\"x/>\"><b t='>'><c/></b></a>",
            // a DOCTYPE's literals may hold '>' and comment openers
            "<!DOCTYPE x SYSTEM \"><!--\"><x><y/></x>",
            "<!DOCTYPE x PUBLIC '[' \"]>\" [<!ELEMENT x (#PCDATA)><!-- ]> --><?pi ]>?>]  ><x><y></y></x>",
            "<!DOCTYPE x [<!ENTITY e 'text'>]><x>&e;</x>",
            // entity references count the nesting their expansions reach, through other entities
            "<!DOCTYPE x [<!ENTITY e '<a><b/></a>'>]><x>&e;</x>",
            "<!DOCTYPE x [<!ENTITY a '<a/>'><!ENTITY b \"<b>&a;</b>\">]><x>&b;<y>&a;</y></x>",
            // ... but not in CDATA, comments or attribute values
            "<!DOCTYPE x [<!ENTITY e '<a/>'>]><x t='&amp;'><![CDATA[&e;]]><!-- &e; --></x>",
        ] {
            assert_eq!(d(s), parsed(s), "{s}");
        }
        // an entity that is never referenced never expands; one referenced once, once
        let deep = format!("{}{}", "<a>".repeat(13), "</a>".repeat(13));
        for (body, depth) in [("<x/>", 1), ("<x>&u;</x>", 14)] {
            let s = format!("<!DOCTYPE x [<!ENTITY u '{deep}'>]>{body}");
            assert_eq!((d(&s), parsed(&s)), (depth, depth), "{s}");
        }
        assert_eq!(d(&format!("{}{}", "<a>".repeat(50), "</a>".repeat(50))), 50);
        assert_eq!(nesting_depth("<a>".repeat(10_000).as_bytes(), 128), 129, "stops counting past the limit");
        // unterminated constructs end the scan (an unfinished tag counts: over, never under)
        assert_eq!(d("<a><!-- open"), 1);
        assert_eq!(d("<a b=\"unclosed><c><d>"), 1);
        // an entity referencing itself expands as deep as the parser nests expansions, then fails
        let s = "<!DOCTYPE x [<!ENTITY e '<a>&e;</a>'>]><x>&e;</x>";
        assert_eq!(d(s), 1 + ENTITY_NESTING);
        assert!(parse(s).is_err());
        // ELEMENT, ATTLIST and NOTATION end at their first '>': a quote there hides no entity
        assert_eq!(d("<!DOCTYPE x [<!ELEMENT x '> <!ENTITY e \"<a><b>\"> <!ATTLIST x '>]><x>&e;</x>"), 3);
        // the parser stops at an unknown DTD token, and so does the scan
        assert_eq!(d("<!DOCTYPE x [<!BOGUS> ]><x><y><z/></y></x>"), 0);
        assert!(parse("<!DOCTYPE x [<!BOGUS> ]><x><y><z/></y></x>").is_err());
        let e = parse(&"<a>".repeat(MAX_XML_DEPTH + 1)).unwrap_err();
        assert!(e.contains("nested"), "{e}");
        // an entity's markup must balance within it, so expansions cannot leave elements open to
        // nest deeper than the scan counts (and `tree_depth` would refuse such a tree anyway)
        let open = "<!DOCTYPE x [<!ENTITY o '<a>'><!ENTITY c '</a>'>]><x>";
        let n = 1000;
        for s in [
            format!("{open}{}</x>", "&o;".repeat(n)),
            format!("{open}{}{}</x>", "&o;".repeat(n), "&c;".repeat(n)),
            format!("{open}{}{}</x>", "&o;".repeat(n), "</a>".repeat(n)),
            format!("{open}{}", "&o;".repeat(n)),
        ] {
            assert!(parse(&s).is_err());
        }
    }
}
