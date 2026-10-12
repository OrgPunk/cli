//! A small XML tree for recomposing metadata, written the way SDR writes it.
//!
//! SDR parses with fast-xml-parser and builds with its XMLBuilder, which (1) trims text,
//! (2) groups repeated sibling elements at the position of the first one, and (3) indents by
//! four spaces with text kept inline. Recomposed files follow the same rules so they are XML
//! equivalent to SDR's output.

pub const NS: &str = "http://soap.sforce.com/2006/04/metadata";
const DECL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
    CData(String),
    Comment(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Element {
    /// Qualified name: `fields`, `xsi:type`-style prefixes kept.
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Trimmed text of the first child element with this name.
    pub fn child_text(&self, name: &str) -> Option<String> {
        self.children.iter().find_map(|n| match n {
            Node::Element(e) if e.name == name => Some(e.text()),
            _ => None,
        })
    }

    pub fn text(&self) -> String {
        self.children
            .iter()
            .filter_map(|n| match n {
                Node::Text(t) | Node::CData(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<String>()
    }

    /// Child elements only.
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            _ => None,
        })
    }

    /// Whether the element has anything besides namespace declarations.
    pub fn has_content(&self) -> bool {
        !self.children.is_empty() || self.attrs.iter().any(|(k, _)| !is_ns_decl(k))
    }
}

fn is_ns_decl(name: &str) -> bool {
    name == "xmlns" || name.starts_with("xmlns:")
}

/// Parses a document and returns its root element; whitespace-only text is dropped, text trimmed.
pub fn parse(text: &str) -> Result<Element, String> {
    let opts = roxmltree::ParsingOptions {
        allow_dtd: false,
        ..Default::default()
    };
    let doc = roxmltree::Document::parse_with_options(text, opts).map_err(|e| e.to_string())?;
    Ok(convert(doc.root_element(), true))
}

fn convert(node: roxmltree::Node, is_root: bool) -> Element {
    let qualified = |ns: Option<&str>, local: &str| match ns.and_then(|ns| node.lookup_prefix(ns)) {
        Some(p) if !p.is_empty() => format!("{p}:{local}"),
        _ => local.to_owned(),
    };
    let mut e = Element::new(qualified(
        node.tag_name().namespace(),
        node.tag_name().name(),
    ));
    if is_root {
        for ns in node.namespaces() {
            match ns.name() {
                Some(p) => e.attrs.push((format!("xmlns:{p}"), ns.uri().to_owned())),
                None => e.attrs.push(("xmlns".to_owned(), ns.uri().to_owned())),
            }
        }
    }
    for a in node.attributes() {
        e.attrs
            .push((qualified(a.namespace(), a.name()), a.value().to_owned()));
    }
    for child in node.children() {
        match child.node_type() {
            roxmltree::NodeType::Element => e.children.push(Node::Element(convert(child, false))),
            roxmltree::NodeType::Text => {
                let raw = child.text().unwrap_or_default();
                let t = raw.trim();
                if t.is_empty() {
                    continue;
                }
                // roxmltree reports CDATA as text; keep it as CDATA when the source had it.
                let start = child.range().start;
                if child.document().input_text()[start..].starts_with("<![CDATA[") {
                    e.children.push(Node::CData(raw.to_owned()));
                } else {
                    e.children.push(Node::Text(t.to_owned()));
                }
            }
            roxmltree::NodeType::Comment => e
                .children
                .push(Node::Comment(child.text().unwrap_or_default().to_owned())),
            _ => {}
        }
    }
    e
}

/// The whole document: declaration plus the root element.
pub fn write(root: &Element) -> String {
    let mut out = String::from(DECL);
    write_element(&mut out, root, 0);
    out
}

fn write_element(out: &mut String, e: &Element, depth: usize) {
    let indent = "    ".repeat(depth);
    out.push_str(&indent);
    out.push('<');
    out.push_str(&e.name);
    for (k, v) in &e.attrs {
        out.push_str(&format!(" {k}=\"{}\"", escape(v, true)));
    }
    out.push('>');
    let has_elements = e
        .children
        .iter()
        .any(|n| matches!(n, Node::Element(_) | Node::Comment(_)));
    if !has_elements {
        for n in &e.children {
            match n {
                Node::Text(t) => out.push_str(&escape(t, false)),
                Node::CData(t) => out.push_str(&format!("<![CDATA[{t}]]>")),
                _ => {}
            }
        }
    } else {
        out.push('\n');
        for n in grouped(&e.children) {
            match n {
                Node::Element(c) => write_element(out, c, depth + 1),
                Node::Text(t) => {
                    out.push_str(&format!("{indent}    {}\n", escape(t, false)));
                }
                Node::CData(t) => out.push_str(&format!("{indent}    <![CDATA[{t}]]>\n")),
                Node::Comment(t) => out.push_str(&format!("{indent}    <!--{t}-->\n")),
            }
        }
        out.push_str(&indent);
    }
    out.push_str(&format!("</{}>\n", e.name));
}

/// Siblings with the same name moved next to the first one, as fast-xml-parser does.
fn grouped(children: &[Node]) -> Vec<&Node> {
    let key = |n: &Node| -> String {
        match n {
            Node::Element(e) => e.name.clone(),
            Node::Text(_) | Node::CData(_) => "#text".to_owned(),
            Node::Comment(_) => "#comment".to_owned(),
        }
    };
    let mut order: Vec<String> = Vec::new();
    for n in children {
        let k = key(n);
        if !order.contains(&k) {
            order.push(k);
        }
    }
    order
        .iter()
        .flat_map(|k| children.iter().filter(move |n| key(n) == *k))
        .collect()
}

fn escape(s: &str, attr: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Ordering like JavaScript's `localeCompare` (ICU root collation), close enough for the
/// metadata names SDR sorts: punctuation before digits before letters, letters compared
/// case-insensitively first and lowercase first on a tie.
pub fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn class(c: char) -> u8 {
        if c.is_alphabetic() {
            2
        } else if c.is_numeric() {
            1
        } else {
            0
        }
    }
    let primary = |s: &str| -> Vec<(u8, char)> {
        s.chars()
            .map(|c| (class(c), c.to_lowercase().next().unwrap_or(c)))
            .collect()
    };
    primary(a).cmp(&primary(b)).then_with(|| {
        // Tertiary: lowercase before uppercase at the first difference.
        let flip = |s: &str| -> Vec<bool> { s.chars().map(char::is_uppercase).collect() };
        flip(a).cmp(&flip(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn roundtrip_trims_and_groups() {
        let src = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
            <CustomObject xmlns=\"http://soap.sforce.com/2006/04/metadata\">\n\
            <label>  Invoice  </label>\n\
            <actionOverrides><actionName>View</actionName></actionOverrides>\n\
            <description>a &amp; b &lt; c</description>\n\
            <actionOverrides><actionName>Edit</actionName></actionOverrides>\n\
            <formula><![CDATA[ x < 1 ]]></formula>\n\
            <empty/>\n\
            </CustomObject>";
        let root = parse(src).unwrap();
        assert_eq!(
            write(&root),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <CustomObject xmlns=\"http://soap.sforce.com/2006/04/metadata\">\n\
             \x20   <label>Invoice</label>\n\
             \x20   <actionOverrides>\n\
             \x20       <actionName>View</actionName>\n\
             \x20   </actionOverrides>\n\
             \x20   <actionOverrides>\n\
             \x20       <actionName>Edit</actionName>\n\
             \x20   </actionOverrides>\n\
             \x20   <description>a &amp; b &lt; c</description>\n\
             \x20   <formula><![CDATA[ x < 1 ]]></formula>\n\
             \x20   <empty></empty>\n\
             </CustomObject>\n"
        );
    }

    #[test]
    fn keeps_prefixed_attributes() {
        let src = "<CustomMetadata xmlns=\"http://soap.sforce.com/2006/04/metadata\" \
                   xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\
                   <values><value xsi:type=\"xsd:string\">x</value></values></CustomMetadata>";
        let out = write(&parse(src).unwrap());
        assert!(out.contains("xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\""));
        assert!(out.contains("<value xsi:type=\"xsd:string\">x</value>"));
    }

    #[test]
    fn locale_order() {
        let mut v = vec!["b__c", "B__c", "a__c", "_x", "1a", "A__c", "Zed"];
        v.sort_by(|a, b| locale_cmp(a, b));
        assert_eq!(v, ["_x", "1a", "a__c", "A__c", "b__c", "B__c", "Zed"]);
        assert_eq!(locale_cmp("Name", "name"), Ordering::Greater);
    }
}
