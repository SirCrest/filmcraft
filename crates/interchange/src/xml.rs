//! Minimal XML writer and read helpers over `roxmltree`.

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

/// An upper bound on the element nesting the XML parser reaches in `xml`; counting stops once it
/// passes `limit`. Exact for the well-formed prefix of a document, which is all the parser gets
/// through: quotes, comments, CDATA, processing instructions and the DOCTYPE are skipped like the
/// parser does. Elements inside internal-subset entities count once per expansion the parser nests.
pub(crate) fn nesting_depth(xml: &[u8], limit: usize) -> usize {
    let mut entities = 0;
    let depth = content_depth(xml, limit, Some(&mut entities));
    depth.saturating_add(entities.saturating_mul(ENTITY_NESTING))
}

/// Index just past the first `pat` at or after `from`.
fn find(xml: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    xml.get(from..)?.windows(pat.len()).position(|w| w == pat).map(|p| from + p + pat.len())
}

/// Index just past the first of `stops` at or after `from` outside a quoted literal, and that byte.
/// `literal` sees each literal's contents.
fn skip_quoted(xml: &[u8], from: usize, stops: &[u8], mut literal: impl FnMut(&[u8])) -> Option<(usize, u8)> {
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

/// The deepest element nesting in `xml` read as element content, counting stops once it passes
/// `limit`. With `entities`, a DOCTYPE is read like the parser does and the deepest nesting in its
/// entity values is recorded there.
fn content_depth(xml: &[u8], limit: usize, mut entities: Option<&mut usize>) -> usize {
    let (mut depth, mut max, mut i) = (0usize, 0usize, 0usize);
    while let Some(at) = xml.get(i..).and_then(|s| s.iter().position(|&c| c == b'<')).map(|p| i + p) {
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
            Some(b'!') => match entities.as_deref_mut() {
                Some(e) if rest.starts_with(b"<!DOCTYPE") => match doctype(xml, at + 9, limit, e) {
                    Some(end) => end,
                    None => break,
                },
                _ => find(xml, at + 2, b">").unwrap_or(xml.len()),
            },
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
/// may hold `>` or `<!--`), recording in `entities` the deepest nesting in an entity value of its
/// internal subset. Returns where the document continues, or `None` where the parser fails.
fn doctype(xml: &[u8], from: usize, limit: usize, entities: &mut usize) -> Option<usize> {
    let (mut i, end) = skip_quoted(xml, from, b"[>", |_| {})?;
    if end == b'>' {
        return Some(i);
    }
    let space = |i: usize| xml.get(i..).map_or(0, |s| s.iter().take_while(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r')).count());
    loop {
        i += space(i);
        let rest = xml.get(i..)?;
        i = if rest.starts_with(b"<!ENTITY") {
            skip_quoted(xml, i + 8, b">", |value| *entities = (*entities).max(content_depth(value, limit, None)))?.0
        } else if rest.starts_with(b"<!--") {
            find(xml, i + 4, b"-->")?
        } else if rest.starts_with(b"<?") {
            find(xml, i + 2, b"?>")?
        } else if rest.starts_with(b"<!ELEMENT") || rest.starts_with(b"<!ATTLIST") || rest.starts_with(b"<!NOTATION") {
            // the parser skips these to their first '>', quoted or not
            find(xml, i + 2, b">")?
        } else if rest.starts_with(b"]") {
            let j = i + 1 + space(i + 1);
            return (xml.get(j) == Some(&b'>')).then_some(j + 1);
        } else {
            return None;
        };
    }
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
        ] {
            assert_eq!(d(s), parsed(s), "{s}");
        }
        assert_eq!(d(&format!("{}{}", "<a>".repeat(50), "</a>".repeat(50))), 50);
        assert_eq!(nesting_depth("<a>".repeat(10_000).as_bytes(), 128), 129, "stops counting past the limit");
        // unterminated constructs end the scan (an unfinished tag counts: over, never under)
        assert_eq!(d("<a><!-- open"), 1);
        assert_eq!(d("<a b=\"unclosed><c><d>"), 1);
        // elements in entity values count once per expansion the parser can nest
        let s = "<!DOCTYPE x [<!ENTITY e '<a><b/></a>'>]><x>&e;</x>";
        assert_eq!((parsed(s), d(s)), (3, 1 + ENTITY_NESTING * 2));
        // ELEMENT, ATTLIST and NOTATION end at their first '>': a quote there hides no entity
        assert_eq!(d("<!DOCTYPE x [<!ELEMENT x '> <!ENTITY e \"<a><b>\"> <!ATTLIST x '>]><x/>"), 1 + ENTITY_NESTING * 2);
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
