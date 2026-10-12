//! mdapi format → source format, as SDR converts into a fresh directory (`sf project convert
//! mdapi`): the input is the files of an mdapi directory or a retrieve zip, the output the
//! files below a package directory's `main/default`.
//!
//! Per strategy: matching content files and bundles are copied, default types get their
//! `-meta.xml`, folders become `<Folder>.<folderSuffix>-meta.xml`, decomposed types
//! (CustomObject, CustomObjectTranslation, Bot) are split into a parent and one file per
//! child, CustomLabels are written into one file, static resources are unzipped or named by
//! content type, Documents keep their content and get a `.document-meta.xml`. Anything else
//! is an explicit error.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use crate::registry::{self, MetadataType};
use crate::xml::{self, Element, Node};
use crate::{Error, Result};

const META: &str = "-meta.xml";
const DIGITAL_EXPERIENCES: &str = "digitalExperiences";
/// Static resource content types SDR unzips.
const ARCHIVES: &[&str] = &[
    "application/zip",
    "application/x-zip-compressed",
    "application/jar",
];
/// SDR's fallbacks for content types mime does not know.
const FALLBACK_EXT: &[(&str, &str)] = &[
    ("text/javascript", "js"),
    ("application/x-javascript", "js"),
    ("application/x-zip-compressed", "zip"),
    ("text/x-haml", "haml"),
    ("image/x-png", "png"),
    ("text/xml", "xml"),
];

/// mdapi path (with `/`) → bytes.
pub type Files = BTreeMap<String, Vec<u8>>;

#[derive(Debug, Default)]
pub struct SourceTree {
    /// Path below `main/default` (with `/`) → bytes.
    pub files: Files,
    /// Path → the `Type:Name` member the file belongs to, at the granularity the org tracks:
    /// a field file belongs to its CustomField, the labels file to `CustomLabels:CustomLabels`.
    pub members: BTreeMap<String, String>,
}

impl SourceTree {
    fn put(&mut self, path: String, member: String, bytes: Vec<u8>) {
        self.members.insert(path.clone(), member);
        self.files.insert(path, bytes);
    }
}

/// The member name of a file path below a type directory: `A.cls-meta.xml` → `A`,
/// `Sales/Q1.report` → `Sales/Q1`.
fn member_name(t: &MetadataType, rest: &str) -> String {
    let rest = rest.strip_suffix(META).unwrap_or(rest);
    match t.suffix.as_deref() {
        Some(sfx) => rest
            .strip_suffix(&format!(".{sfx}"))
            .unwrap_or(rest)
            .to_owned(),
        None => rest.to_owned(),
    }
}

/// Converts mdapi files (package.xml and destructive manifests are skipped).
pub fn convert(input: &Files) -> Result<SourceTree> {
    let reg = registry::get();
    let mut out = SourceTree::default();
    // CustomLabels: all labels by full name, first position, last value (a JS Map).
    let mut labels: Vec<(String, Element)> = Vec::new();

    for (path, bytes) in input {
        let Some((dir, rest)) = path.split_once('/') else {
            if path == "package.xml" || crate::mdapi::DESTRUCTIVE_FILES.contains(&path.as_str()) {
                continue;
            }
            return Err(Error::StrayFile(path.clone()));
        };
        if dir == DIGITAL_EXPERIENCES {
            let segs: Vec<&str> = rest.split('/').collect();
            let member = match segs.as_slice() {
                [base, site, ctype, cname, _, ..] => {
                    format!("DigitalExperience:{base}/{site}.{ctype}/{cname}")
                }
                [base, site, ..] => format!("DigitalExperienceBundle:{base}/{site}"),
                _ => format!("DigitalExperienceBundle:{rest}"),
            };
            out.put(path.clone(), member, bytes.clone());
            continue;
        }
        let file_name = rest.rsplit('/').next().unwrap_or(rest);
        let (base, is_meta) = match file_name.strip_suffix(META) {
            Some(b) => (b, true),
            None => (file_name, false),
        };
        let t = pick_type(dir, base, is_meta).ok_or_else(|| Error::UnknownDir {
            dir: dir.to_owned(),
        })?;

        // A folder: `reports/Sales-meta.xml` → `reports/Sales.reportFolder-meta.xml`.
        let content_beside = path
            .strip_suffix(META)
            .is_some_and(|stem| input.contains_key(stem));
        if t.in_folder && is_meta && !content_beside && !has_type_suffix(t, base) {
            let folder = t
                .folder_type
                .as_deref()
                .and_then(|id| reg.by_id(id))
                .and_then(|f| f.suffix.as_deref())
                .ok_or_else(|| Error::UnknownSuffix(path.into()))?;
            let stem = path.strip_suffix(META).expect("is meta");
            let name = rest.strip_suffix(META).unwrap_or(rest);
            // Folders of these types are named with a trailing `/` in package.xml.
            let slash = matches!(t.name.as_str(), "Report" | "Dashboard" | "EmailTemplate");
            let member = format!("{}:{name}{}", t.name, if slash { "/" } else { "" });
            out.put(format!("{stem}.{folder}{META}"), member, bytes.clone());
            continue;
        }

        let strategy = t.strategies.as_ref();
        match (t.adapter(), strategy.and_then(|s| s.transformer.as_deref())) {
            ("decomposed", _) => decompose(t, path, bytes, &mut out)?,
            (_, Some("nonDecomposed")) => collect_labels(t, path, bytes, &mut labels)?,
            (_, Some("staticResource")) => {
                if is_meta {
                    static_resource(input, path, bytes, &mut out)?;
                }
            }
            ("mixedContent", _) if t.name == "Document" => {
                let target = match path.strip_suffix(META) {
                    // `X.png-meta.xml` → `X.document-meta.xml`
                    Some(stem) => format!(
                        "{}.{}{META}",
                        stem.rsplit_once('.').map_or(stem, |(s, _)| s),
                        t.suffix.as_deref().unwrap_or_default()
                    ),
                    None => path.clone(),
                };
                // `Pub/logo.png` → `Pub/logo`
                let name = rest.strip_suffix(META).unwrap_or(rest);
                let name = name.rsplit_once('.').map_or(name, |(n, _)| n);
                out.put(target, format!("Document:{name}"), bytes.clone());
            }
            ("mixedContent" | "bundle", _) => {
                // The first directory below the type: `lwc/c/c.js` → `c`, `experiences/site1.site-meta.xml` → `site1`.
                let first = rest.split('/').next().unwrap_or(rest);
                let name = first.strip_suffix(META).unwrap_or(first);
                let name = name.rsplit_once('.').map_or(name, |(n, _)| n);
                out.put(path.clone(), format!("{}:{name}", t.name), bytes.clone());
            }
            ("matchingContentFile", _) => {
                out.put(
                    path.clone(),
                    format!("{}:{}", t.name, member_name(t, rest)),
                    bytes.clone(),
                );
            }
            ("default", None) => {
                let target = if is_meta {
                    path.clone()
                } else {
                    format!("{path}{META}")
                };
                out.put(
                    target,
                    format!("{}:{}", t.name, member_name(t, rest)),
                    bytes.clone(),
                );
            }
            (adapter, _) => {
                return Err(Error::Unsupported {
                    kind: t.name.clone(),
                    strategy: adapter.to_owned(),
                    path: path.into(),
                });
            }
        }
    }

    if !labels.is_empty() {
        let t = reg.by_name("CustomLabels").expect("registered");
        let mut root = Element::new(&t.name);
        root.attrs.push(("xmlns".into(), xml::NS.into()));
        root.children = labels.into_iter().map(|(_, e)| Node::Element(e)).collect();
        let suffix = t.suffix.as_deref().unwrap_or_default();
        out.put(
            format!("{}/{}.{suffix}{META}", t.directory_name, t.name),
            format!("{0}:{0}", t.name),
            xml::write(&root).into_bytes(),
        );
    }
    Ok(out)
}

/// The type of a file in an mdapi directory: by suffix among the directory's types, else the
/// directory's only (or in-folder) type.
fn pick_type(dir: &str, base: &str, is_meta: bool) -> Option<&'static MetadataType> {
    let candidates: Vec<&'static MetadataType> = registry::get()
        .in_dir(dir)
        // Folder types share the directory; their files carry the content type's layout.
        .filter(|t| t.folder_content_type.is_none())
        .collect();
    let ext = base.rsplit_once('.').map(|(_, e)| e);
    if let Some(t) = ext.and_then(|e| candidates.iter().find(|t| t.has_suffix(e))) {
        return Some(t);
    }
    if is_meta {
        if let Some(t) = candidates.iter().find(|t| t.in_folder) {
            return Some(t);
        }
    }
    match candidates.as_slice() {
        [only] => Some(only),
        _ => candidates.iter().find(|t| t.in_folder).copied(),
    }
}

fn has_type_suffix(t: &MetadataType, base: &str) -> bool {
    base.rsplit_once('.').is_some_and(|(_, e)| t.has_suffix(e))
}

fn parse(path: &str, bytes: &[u8]) -> Result<Element> {
    let text = std::str::from_utf8(bytes).map_err(|e| Error::Xml {
        path: path.into(),
        reason: e.to_string(),
    })?;
    xml::parse(text).map_err(|reason| Error::Xml {
        path: path.into(),
        reason,
    })
}

/// `objects/Account.object` → `objects/Account/Account.object-meta.xml` and one file per
/// child: `objects/Account/fields/F__c.field-meta.xml` (CustomObject puts each child type in
/// its folder, CustomObjectTranslation and Bot keep children next to the parent).
fn decompose(t: &MetadataType, path: &str, bytes: &[u8], out: &mut SourceTree) -> Result<()> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let suffix = t.suffix.as_deref().unwrap_or_default();
    let name = file
        .strip_suffix(&format!(".{suffix}"))
        .ok_or_else(|| Error::UnknownSuffix(path.into()))?;
    let dir = format!("{}/{name}", t.directory_name);
    let per_type = t
        .strategies
        .as_ref()
        .and_then(|s| s.decomposition.as_deref())
        == Some("folderPerType");
    let children = t.children.as_ref().expect("decomposed types have children");
    let child_type = |tag: &str| -> Option<&MetadataType> {
        children
            .types
            .values()
            .find(|c| c.xml_element_name.as_deref() == Some(tag))
            .or_else(|| {
                children
                    .directories
                    .get(tag)
                    .and_then(|id| children.types.get(id))
            })
    };

    let root = parse(path, bytes)?;
    let mut parent = Element::new(&t.name);
    parent.attrs = root.attrs.clone();
    if !parent.attrs.iter().any(|(k, _)| k == "xmlns") {
        parent.attrs.insert(0, ("xmlns".into(), xml::NS.into()));
    }
    for node in root.children {
        let Node::Element(e) = node else {
            parent.children.push(node);
            continue;
        };
        let Some(ct) = child_type(&e.name) else {
            parent.children.push(Node::Element(e));
            continue;
        };
        let id = ct.unique_id_element.as_deref().unwrap_or("fullName");
        let entry = e.child_text(id).ok_or_else(|| Error::Xml {
            path: path.into(),
            reason: format!("<{}> without <{id}>", e.name),
        })?;
        let mut child = Element::new(&ct.name);
        child.attrs.push(("xmlns".into(), xml::NS.into()));
        child
            .attrs
            .extend(e.attrs.into_iter().filter(|(k, _)| k != "xmlns"));
        child.children = e.children;
        let folder = if per_type {
            format!("{}/", ct.directory_name)
        } else {
            String::new()
        };
        let child_suffix = ct.suffix.as_deref().unwrap_or_default();
        // Children the org does not track alone (field translations) go with the parent.
        let member = if ct.is_addressable == Some(false) {
            format!("{}:{name}", t.name)
        } else {
            format!("{}:{name}.{entry}", ct.name)
        };
        out.put(
            format!("{dir}/{folder}{entry}.{child_suffix}{META}"),
            member,
            xml::write(&child).into_bytes(),
        );
    }
    out.put(
        format!("{dir}/{name}.{suffix}{META}"),
        format!("{}:{name}", t.name),
        xml::write(&parent).into_bytes(),
    );
    Ok(())
}

fn collect_labels(
    t: &MetadataType,
    path: &str,
    bytes: &[u8],
    labels: &mut Vec<(String, Element)>,
) -> Result<()> {
    let child = t
        .children
        .as_ref()
        .and_then(|c| c.types.values().next())
        .expect("CustomLabels has CustomLabel");
    let group = child.xml_element_name.as_deref().unwrap_or("labels");
    let id = child.unique_id_element.as_deref().unwrap_or("fullName");
    for e in parse(path, bytes)?.elements().filter(|e| e.name == group) {
        let name = e.child_text(id).ok_or_else(|| Error::Xml {
            path: path.into(),
            reason: format!("<{group}> without <{id}>"),
        })?;
        match labels.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = e.clone(),
            None => labels.push((name, e.clone())),
        }
    }
    Ok(())
}

/// `staticresources/X.resource` with its meta: an archive is unzipped into
/// `staticresources/X/`, anything else becomes `staticresources/X.<ext>` by content type.
fn static_resource(
    input: &Files,
    meta_path: &str,
    meta: &[u8],
    out: &mut SourceTree,
) -> Result<()> {
    let content_path = meta_path.strip_suffix(META).expect("is meta");
    let content = input
        .get(content_path)
        .ok_or_else(|| Error::MissingContent {
            meta: meta_path.into(),
            content: content_path.into(),
        })?;
    let content_type = parse(meta_path, meta)?
        .child_text("contentType")
        .unwrap_or_else(|| "application/octet-stream".into());
    let base = content_path
        .rsplit_once('.')
        .map_or(content_path, |(b, _)| b);
    let member = format!("StaticResource:{}", base.rsplit('/').next().unwrap_or(base));
    out.put(meta_path.to_owned(), member.clone(), meta.to_vec());
    if ARCHIVES.contains(&content_type.as_str()) {
        let bad = |reason: String| Error::Xml {
            path: content_path.into(),
            reason,
        };
        let mut zip = zip::ZipArchive::new(Cursor::new(content)).map_err(|e| bad(e.to_string()))?;
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).map_err(|e| bad(e.to_string()))?;
            if f.is_dir() {
                continue;
            }
            let name = f.name().replace('\\', "/");
            if name.starts_with('/') || name.split('/').any(|s| s == "..") {
                return Err(bad(format!(
                    "entry `{name}` would be written outside the resource"
                )));
            }
            let mut data = Vec::new();
            f.read_to_end(&mut data).map_err(|e| bad(e.to_string()))?;
            out.put(format!("{base}/{name}"), member.clone(), data);
        }
    } else {
        out.put(
            format!("{base}.{}", extension(&content_type)),
            member,
            content.clone(),
        );
    }
    Ok(())
}

/// The extension SDR gives a content type: mime's, else SDR's fallbacks, else `bin`.
fn extension(content_type: &str) -> &'static str {
    let t = content_type
        .trim()
        .split([';', ' '])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    crate::mime_ext::TABLE
        .binary_search_by(|(k, _)| k.cmp(&t.as_str()))
        .ok()
        .map(|i| crate::mime_ext::TABLE[i].1)
        .or_else(|| FALLBACK_EXT.iter().find(|(k, _)| *k == t).map(|(_, e)| *e))
        .unwrap_or("bin")
}

/// The files of an mdapi directory, by path with `/`; OS junk skipped.
pub fn read_dir(dir: &std::path::Path) -> Result<Files> {
    let mut out = Files::new();
    for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let entry = entry.map_err(|e| Error::Io {
            path: e.path().unwrap_or(dir).to_owned(),
            source: e
                .into_io_error()
                .unwrap_or_else(|| std::io::Error::other("symlink loop")),
        })?;
        let name = entry.file_name().to_string_lossy();
        if !entry.file_type().is_file() || crate::mdapi::SKIP_FILES.contains(&name.as_ref()) {
            continue;
        }
        let rel = entry.path().strip_prefix(dir).expect("inside dir");
        let key = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let bytes = std::fs::read(entry.path()).map_err(|source| Error::Io {
            path: entry.path().to_owned(),
            source,
        })?;
        out.insert(key, bytes);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[(&str, &[u8])]) -> Files {
        list.iter()
            .map(|(p, b)| ((*p).to_owned(), b.to_vec()))
            .collect()
    }

    fn text(t: &SourceTree, p: &str) -> String {
        String::from_utf8(
            t.files
                .get(p)
                .unwrap_or_else(|| panic!("{p} missing: {:?}", t.files.keys().collect::<Vec<_>>()))
                .clone(),
        )
        .unwrap()
    }

    #[test]
    fn simple_strategies() {
        let t = convert(&files(&[
            ("package.xml", b"<Package/>"),
            ("classes/A.cls", b"class A {}"),
            ("classes/A.cls-meta.xml", b"<ApexClass/>"),
            ("layouts/Account-L.layout", b"<Layout/>"),
            ("lwc/c/c.js", b"js"),
            ("reports/Sales-meta.xml", b"<ReportFolder/>"),
            ("reports/Sales/Q1.report", b"<Report/>"),
            ("documents/Pub/logo.png", b"png"),
            ("documents/Pub/logo.png-meta.xml", b"<Document/>"),
        ]))
        .unwrap();
        let members: Vec<&str> = t.members.values().map(String::as_str).collect();
        assert_eq!(
            members,
            [
                "ApexClass:A",
                "ApexClass:A",
                "Document:Pub/logo",
                "Document:Pub/logo",
                "Layout:Account-L",
                "LightningComponentBundle:c",
                "Report:Sales/",
                "Report:Sales/Q1",
            ]
        );
        let keys: Vec<&str> = t.files.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "classes/A.cls",
                "classes/A.cls-meta.xml",
                "documents/Pub/logo.document-meta.xml",
                "documents/Pub/logo.png",
                "layouts/Account-L.layout-meta.xml",
                "lwc/c/c.js",
                "reports/Sales.reportFolder-meta.xml",
                "reports/Sales/Q1.report-meta.xml",
            ]
        );
    }

    #[test]
    fn decomposes_objects() {
        let object = br#"<?xml version="1.0" encoding="UTF-8"?>
<CustomObject xmlns="http://soap.sforce.com/2006/04/metadata">
    <fields><fullName>A__c</fullName><type>Text</type></fields>
    <label>Inv</label>
    <fields><fullName>B__c</fullName></fields>
    <listViews><fullName>All</fullName></listViews>
</CustomObject>"#;
        let t = convert(&files(&[("objects/Inv__c.object", object)])).unwrap();
        let parent = text(&t, "objects/Inv__c/Inv__c.object-meta.xml");
        assert!(parent.contains("<label>Inv</label>") && !parent.contains("A__c"));
        let field = text(&t, "objects/Inv__c/fields/A__c.field-meta.xml");
        assert!(field.contains("<CustomField xmlns=\"http://soap.sforce.com/2006/04/metadata\">"));
        assert!(field.contains("<type>Text</type>"));
        assert!(
            t.files
                .contains_key("objects/Inv__c/listViews/All.listView-meta.xml")
        );
        assert_eq!(
            t.members["objects/Inv__c/fields/A__c.field-meta.xml"],
            "CustomField:Inv__c.A__c"
        );
        assert_eq!(
            t.members["objects/Inv__c/Inv__c.object-meta.xml"],
            "CustomObject:Inv__c"
        );

        let cot = br#"<CustomObjectTranslation xmlns="http://soap.sforce.com/2006/04/metadata"><fields><name>F__c</name><label>X</label></fields><gender>Feminine</gender></CustomObjectTranslation>"#;
        let t = convert(&files(&[(
            "objectTranslations/Inv__c-de.objectTranslation",
            cot,
        )]))
        .unwrap();
        assert!(
            t.files
                .contains_key("objectTranslations/Inv__c-de/F__c.fieldTranslation-meta.xml")
        );
        assert_eq!(
            t.members["objectTranslations/Inv__c-de/F__c.fieldTranslation-meta.xml"],
            "CustomObjectTranslation:Inv__c-de",
            "field translations go with their parent"
        );
        assert!(
            t.files
                .contains_key("objectTranslations/Inv__c-de/Inv__c-de.objectTranslation-meta.xml")
        );
    }

    #[test]
    fn labels_and_static_resources() {
        let labels = br#"<CustomLabels xmlns="http://soap.sforce.com/2006/04/metadata"><labels><fullName>B</fullName><value>b</value></labels><labels><fullName>A</fullName><value>a</value></labels></CustomLabels>"#;
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("js/app.js", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, b"app").unwrap();
        let zipped = zip.finish().unwrap().into_inner();
        let t = convert(&files(&[
            ("labels/CustomLabels.labels", labels),
            ("staticresources/lib.resource", &zipped),
            (
                "staticresources/lib.resource-meta.xml",
                b"<StaticResource><contentType>application/zip</contentType></StaticResource>",
            ),
            ("staticresources/cfg.resource", b"{}"),
            (
                "staticresources/cfg.resource-meta.xml",
                b"<StaticResource><contentType>application/json</contentType></StaticResource>",
            ),
            ("staticresources/x.resource", b"x"),
            (
                "staticresources/x.resource-meta.xml",
                b"<StaticResource><contentType>text/javascript</contentType></StaticResource>",
            ),
        ]))
        .unwrap();
        let l = text(&t, "labels/CustomLabels.labels-meta.xml");
        assert!(
            l.find("<fullName>B</fullName>") < l.find("<fullName>A</fullName>"),
            "file order kept"
        );
        assert_eq!(text(&t, "staticresources/lib/js/app.js"), "app");
        assert_eq!(text(&t, "staticresources/cfg.json"), "{}");
        assert_eq!(text(&t, "staticresources/x.js"), "x");
        assert!(
            t.files
                .contains_key("staticresources/lib.resource-meta.xml")
        );
    }

    #[test]
    fn mime_table_is_sorted() {
        assert!(crate::mime_ext::TABLE.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(extension("application/octet-stream"), "bin");
        assert_eq!(extension("Image/PNG"), "png");
        assert_eq!(extension("application/x-unknown"), "bin");
    }

    #[test]
    fn unknown_and_stray_files_are_errors() {
        assert!(matches!(
            convert(&files(&[("nope/x.y", b"")])),
            Err(Error::UnknownDir { .. })
        ));
        assert!(matches!(
            convert(&files(&[("README.md", b"")])),
            Err(Error::StrayFile(_))
        ));
    }
}
