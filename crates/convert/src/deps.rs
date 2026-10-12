//! Dependencies between components of one package, found by name.
//!
//! A component depends on another when its files mention the other's name as an identifier
//! (comments are skipped, and in metadata XML only element text counts), plus implicit links:
//! `Account-Account Layout` and `Account.MyAction` need the `Account` object, a report needs its
//! folder. The scan over-approximates: an extra edge only constrains the order, a missed one
//! can break a deploy, so when in doubt it adds the edge.

use std::collections::HashMap;

use crate::mdapi::Component;
use crate::registry;

pub struct Scanner<'a> {
    components: &'a [Component],
    /// Lowercase name → components that can be referenced by it.
    index: HashMap<String, Vec<u32>>,
    edges: Vec<Vec<u32>>,
    word: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Apex and JavaScript: skip comments.
    Code,
    /// Metadata XML: only element text.
    MetaXml,
    /// Visualforce, Aura and LWC markup: everything but comments, plus `c-my-comp` tags.
    Markup,
}

impl<'a> Scanner<'a> {
    pub fn new(components: &'a [Component]) -> Self {
        let mut index: HashMap<String, Vec<u32>> = HashMap::new();
        let mut objects: HashMap<String, u32> = HashMap::new();
        let mut by_key: HashMap<(&str, &str), u32> = HashMap::new();
        for (i, c) in components.iter().enumerate() {
            let i = i as u32;
            by_key.insert((&c.kind, &c.name), i);
            if c.kind == "CustomObject" {
                objects.insert(c.name.to_lowercase(), i);
            }
            if c.kind != "Settings" && is_word(&c.name) {
                index.entry(c.name.to_lowercase()).or_default().push(i);
            }
        }

        let mut edges = vec![Vec::new(); components.len()];
        for (i, c) in components.iter().enumerate() {
            if c.kind == "CustomObject" || c.kind == "Settings" {
                continue;
            }
            let prefix = c.name.split(['-', '.']).next().unwrap_or(&c.name);
            let mut candidates = vec![c.name.to_lowercase(), prefix.to_lowercase()];
            if c.kind == "CustomMetadata" {
                candidates.push(format!("{}__mdt", prefix.to_lowercase()));
            }
            for name in candidates {
                if let Some(&o) = objects.get(&name) {
                    edges[i].push(o);
                }
            }
            if registry::get()
                .by_name(&c.kind)
                .is_some_and(|t| t.in_folder)
            {
                let mut folder = c.name.as_str();
                while let Some((parent, _)) = folder.rsplit_once('/') {
                    if let Some(&f) = by_key.get(&(c.kind.as_str(), parent)) {
                        edges[i].push(f);
                    }
                    folder = parent;
                }
            }
        }

        Self {
            components,
            index,
            edges,
            word: String::new(),
        }
    }

    /// Adds references from one file of component `component`.
    pub fn scan_file(&mut self, component: usize, path: &str, bytes: &[u8]) {
        let c = &self.components[component];
        if c.kind == "Settings" || (registry::is_binary(&c.kind) && !path.ends_with("-meta.xml")) {
            return;
        }
        let text = String::from_utf8_lossy(bytes);
        let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        let mode = match ext {
            "cls" | "trigger" | "js" => Mode::Code,
            "page" | "component" | "cmp" | "app" | "evt" | "intf" | "html" | "design"
            | "auradoc" | "tokens" => Mode::Markup,
            _ if text
                .trim_start_matches('\u{feff}')
                .trim_start()
                .starts_with('<') =>
            {
                Mode::MetaXml
            }
            _ => Mode::Markup,
        };
        let visible = match mode {
            Mode::Code => strip_code_comments(&text),
            Mode::MetaXml => xml_text(&text),
            Mode::Markup => strip_markup_comments(&text),
        };

        let source_sink = registry::is_sink(&c.kind);
        let Self {
            components,
            index,
            edges,
            word,
            ..
        } = self;
        let mut on_word = |w: &str| {
            word.clear();
            word.extend(w.chars().map(|ch| ch.to_ascii_lowercase()));
            if let Some(targets) = index.get(word.as_str()) {
                for &t in targets {
                    if t as usize != component
                        && (source_sink || !registry::is_sink(&components[t as usize].kind))
                    {
                        edges[component].push(t);
                    }
                }
            }
        };
        words(&visible, &mut on_word);
        if mode == Mode::Markup {
            kebab_tags(&visible, &mut on_word);
        }
    }

    /// For each component, the components it depends on (sorted, without duplicates).
    pub fn finish(self) -> Vec<Vec<usize>> {
        self.edges
            .into_iter()
            .map(|mut e| {
                e.sort_unstable();
                e.dedup();
                e.into_iter().map(|t| t as usize).collect()
            })
            .collect()
    }
}

fn is_word(s: &str) -> bool {
    s.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Identifiers: a letter followed by letters, digits and `_`.
fn words(text: &str, f: &mut impl FnMut(&str)) {
    for w in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if w.len() > 1 && w.as_bytes()[0].is_ascii_alphabetic() {
            f(w);
        }
    }
}

/// LWC tags: `<c-my-comp>` refers to the bundle `myComp`, matched case-insensitively as `mycomp`.
fn kebab_tags(text: &str, f: &mut impl FnMut(&str)) {
    for w in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
        if let Some(rest) = w.strip_prefix("c-").filter(|r| !r.is_empty()) {
            f(&rest.replace('-', ""));
        }
    }
}

/// Drops `//` and `/* */` comments outside string literals; the rest is kept as is.
fn strip_code_comments(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let mut start = 0;
    while i < b.len() {
        match quote {
            Some(q) => {
                if b[i] == b'\\' {
                    i += 1;
                } else if b[i] == q || (b[i] == b'\n' && q != b'`') {
                    quote = None;
                }
                i += 1;
            }
            None => {
                if b[i..].starts_with(b"//") || b[i..].starts_with(b"/*") {
                    out.push_str(&text[start..i]);
                    out.push(' ');
                    let end = if b[i + 1] == b'/' {
                        find(b, i, b"\n").unwrap_or(b.len())
                    } else {
                        find(b, i + 2, b"*/").map_or(b.len(), |e| e + 2)
                    };
                    i = end;
                    start = end;
                } else {
                    if matches!(b[i], b'\'' | b'"' | b'`') {
                        quote = Some(b[i]);
                    }
                    i += 1;
                }
            }
        }
    }
    out.push_str(&text[start.min(text.len())..]);
    out
}

/// Text content of XML: markup and comments are dropped, CDATA content is kept.
fn xml_text(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len() / 2);
    let mut i = 0;
    while let Some(lt) = find(b, i, b"<") {
        out.push_str(&text[i..lt]);
        out.push(' ');
        i = if b[lt..].starts_with(b"<!--") {
            find(b, lt, b"-->").map_or(b.len(), |e| e + 3)
        } else if b[lt..].starts_with(b"<![CDATA[") {
            let end = find(b, lt, b"]]>").unwrap_or(b.len());
            out.push_str(&text[lt + 9..end]);
            out.push(' ');
            (end + 3).min(b.len())
        } else {
            find(b, lt, b">").map_or(b.len(), |e| e + 1)
        };
    }
    out.push_str(&text[i.min(text.len())..]);
    out
}

fn strip_markup_comments(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(start) = find(b, i, b"<!--") {
        out.push_str(&text[i..start]);
        out.push(' ');
        i = find(b, start, b"-->").map_or(b.len(), |e| e + 3);
    }
    out.push_str(&text[i.min(text.len())..]);
    out
}

fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdapi::MdapiFile;

    fn comp(kind: &str, name: &str) -> Component {
        Component {
            kind: kind.into(),
            name: name.into(),
            files: Vec::<MdapiFile>::new(),
        }
    }

    fn deps(components: &[Component], files: &[(usize, &str, &str)]) -> Vec<Vec<usize>> {
        let mut s = Scanner::new(components);
        for (c, path, body) in files {
            s.scan_file(*c, path, body.as_bytes());
        }
        s.finish()
    }

    #[test]
    fn apex_references_skip_comments_keep_strings() {
        let cs = [
            comp("ApexClass", "Service"),
            comp("ApexClass", "Helper"),
            comp("ApexClass", "Unused"),
            comp("CustomObject", "Invoice__c"),
        ];
        let body = "public class Service { // Unused here\n /* Unused */ \
                    Helper h; String q = 'SELECT Id FROM Invoice__c // not a comment'; }";
        let d = deps(&cs, &[(0, "classes/Service.cls", body)]);
        assert_eq!(d[0], [1, 3]);
    }

    #[test]
    fn xml_text_only_and_sinks() {
        let cs = [
            comp("Profile", "Admin"),
            comp("ApexClass", "fields"),
            comp("ApexClass", "Ctrl"),
            comp("PermissionSet", "Ops"),
            comp("PermissionSetGroup", "All"),
        ];
        let profile = "<?xml version=\"1.0\"?><Profile><!-- Ctrl --><fields/>\
                       <classAccesses><apexClass>Ctrl</apexClass></classAccesses></Profile>";
        let class = "class Ctrl { String p = 'Admin'; }";
        let psg = "<PermissionSetGroup><permissionSets>Ops</permissionSets></PermissionSetGroup>";
        let d = deps(
            &cs,
            &[
                (0, "profiles/Admin.profile", profile),
                (2, "classes/Ctrl.cls", class),
                (4, "permissionsetgroups/All.permissionsetgroup", psg),
            ],
        );
        assert_eq!(d[0], [2], "tag names and comments are not references");
        assert!(d[2].is_empty(), "a class never depends on a profile");
        assert_eq!(d[4], [3], "sinks may depend on each other");
    }

    #[test]
    fn implicit_object_and_folder_links() {
        let cs = [
            comp("CustomObject", "Account"),
            comp("CustomObject", "Conf__mdt"),
            comp("Layout", "Account-Account Layout"),
            comp("QuickAction", "Account.New"),
            comp("CustomMetadata", "Conf.Default"),
            comp("Report", "Sales"),
            comp("Report", "Sales/Pipeline"),
            comp("Workflow", "Account"),
        ];
        let d = deps(&cs, &[]);
        assert_eq!(d[2], [0]);
        assert_eq!(d[3], [0]);
        assert_eq!(d[4], [1]);
        assert_eq!(d[6], [5]);
        assert_eq!(d[7], [0]);
    }

    #[test]
    fn lwc_markup_and_imports() {
        let cs = [
            comp("LightningComponentBundle", "parentCmp"),
            comp("LightningComponentBundle", "childCmp"),
            comp("ApexClass", "AccountCtrl"),
        ];
        let html = "<template><!-- <c-ghost> --><c-child-cmp></c-child-cmp></template>";
        let js = "import get from '@salesforce/apex/AccountCtrl.get';";
        let d = deps(
            &cs,
            &[
                (0, "lwc/parentCmp/parentCmp.html", html),
                (0, "lwc/parentCmp/parentCmp.js", js),
            ],
        );
        assert_eq!(d[0], [1, 2]);
    }

    #[test]
    fn binary_content_is_not_scanned() {
        let cs = [comp("StaticResource", "lib"), comp("ApexClass", "Ctrl")];
        let d = deps(&cs, &[(0, "staticresources/lib.resource", "Ctrl")]);
        assert!(d[0].is_empty());
    }
}
