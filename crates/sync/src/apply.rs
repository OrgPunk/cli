//! Writing pulled components into the working copy.
//!
//! A file the project already has is overwritten where it is, in whatever package directory
//! and layout; a new file of a known component goes next to that component (or its parent);
//! a new component goes to the default package directory under `main/default`. Only files of
//! the pulled members are written, so retrieving an object does not overwrite local edits of
//! fields that were not pulled. Labels merge into the file that holds each of them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use orgpunk_config::sfdx_project::SfdxProject;
use orgpunk_convert::registry;
use orgpunk_convert::to_source::SourceTree;
use orgpunk_convert::xml::{self, Element, Node};

use crate::local::{LocalMember, file_of};
use crate::{Error, Result, key};

const LABELS_MEMBER: &str = "CustomLabels:CustomLabels";
const LABELS_FILE: &str = "labels/CustomLabels.labels-meta.xml";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Project-relative paths.
    pub written: Vec<String>,
    pub removed: Vec<String>,
}

/// Writes the `pulled` members (by key) of `tree` and removes the `deleted` members' files.
/// `before` is the working copy as scanned before.
pub fn apply(
    project: &SfdxProject,
    before: &BTreeMap<String, LocalMember>,
    tree: &SourceTree,
    pulled: &BTreeSet<String>,
    deleted: &[String],
) -> Result<Applied> {
    let layout = Layout::new(project, before);
    let mut out = Applied::default();

    for (rel, bytes) in &tree.files {
        let member = &tree.members[rel];
        let k = key(member);
        if member == LABELS_MEMBER || !pulled.contains(&k) || is_empty_parent(member, bytes) {
            continue;
        }
        let target = layout.target(rel, member);
        write(&project.root, &target, bytes)?;
        out.written.push(target);
    }

    // Files of pulled members the org no longer has (e.g. a static resource's old files). A
    // member the org did not return at all is left alone: it is not in the org, not empty.
    let written: BTreeSet<&str> = out.written.iter().map(String::as_str).collect();
    let returned: BTreeSet<String> = tree.members.values().map(|m| key(m)).collect();
    let mut stale = Vec::new();
    for k in pulled.iter().filter(|k| returned.contains(*k)) {
        if let Some(l) = before.get(k).filter(|l| !is_label(&l.member)) {
            stale.extend(
                l.files
                    .keys()
                    .map(|f| file_of(f).to_owned())
                    .filter(|f| !written.contains(f.as_str())),
            );
        }
    }
    for f in stale {
        remove(&project.root, &f)?;
        out.removed.push(f);
    }

    // Labels: element by element into the file that has each label.
    let mut label_edits: BTreeMap<String, Vec<Edit>> = BTreeMap::new();
    if let Some(bytes) = tree
        .members
        .iter()
        .find(|(_, m)| *m == LABELS_MEMBER)
        .and_then(|(p, _)| tree.files.get(p))
    {
        let root = parse(LABELS_FILE, bytes)?;
        for e in root.elements().filter(|e| e.name == "labels") {
            let Some(name) = e.child_text("fullName") else {
                continue;
            };
            let member = format!("CustomLabel:{name}");
            if !pulled.contains(&key(&member)) {
                continue;
            }
            let file = layout.label_file(before, &member);
            label_edits
                .entry(file)
                .or_default()
                .push(Edit::Put(name, e.clone()));
        }
    }
    for m in deleted {
        if is_label(m) {
            if let Some(file) = before.get(&key(m)).and_then(|l| l.files.keys().next()) {
                let name = m.split_once(':').map_or("", |(_, n)| n).to_owned();
                label_edits
                    .entry(file_of(file).to_owned())
                    .or_default()
                    .push(Edit::Remove(name));
            }
            continue;
        }
        if let Some(l) = before.get(&key(m)) {
            for f in l.files.keys() {
                remove(&project.root, file_of(f))?;
                out.removed.push(file_of(f).to_owned());
            }
        }
    }
    for (file, edits) in label_edits {
        edit_labels(&project.root, &file, edits)?;
        out.written.push(file);
    }
    out.written.sort();
    out.written.dedup();
    out.removed.sort();
    out.removed.dedup();
    Ok(out)
}

/// Where the project keeps things.
struct Layout {
    /// Lowercase tail below the package layout (`classes/A.cls`) → project path.
    tails: HashMap<String, String>,
    /// Member key → the base its files are under (`force-app/main/default`).
    bases: HashMap<String, String>,
    /// `<default package dir>/main/default`.
    default_base: String,
}

impl Layout {
    fn new(project: &SfdxProject, before: &BTreeMap<String, LocalMember>) -> Self {
        let dirs: Vec<String> = project
            .package_directories
            .iter()
            .map(|d| {
                d.path
                    .trim_start_matches("./")
                    .trim_end_matches(['/', '\\'])
                    .replace('\\', "/")
            })
            .collect();
        let default = project
            .package_directories
            .iter()
            .position(|d| d.default)
            .unwrap_or(0);
        let mut layout = Layout {
            tails: HashMap::new(),
            bases: HashMap::new(),
            default_base: format!(
                "{}/main/default",
                dirs.get(default).map_or("force-app", String::as_str)
            ),
        };
        for (k, l) in before {
            for f in l.files.keys() {
                let f = file_of(f);
                if let Some((base, tail)) = split(&dirs, f) {
                    layout.tails.insert(tail.to_lowercase(), f.to_owned());
                    layout.bases.entry(k.clone()).or_insert(base);
                }
            }
        }
        layout
    }

    fn target(&self, rel: &str, member: &str) -> String {
        if let Some(existing) = self.tails.get(&rel.to_lowercase()) {
            return existing.clone();
        }
        let base = self
            .bases
            .get(&key(member))
            .or_else(|| parent_member(member).and_then(|p| self.bases.get(&key(&p))))
            .unwrap_or(&self.default_base);
        format!("{base}/{rel}")
    }

    /// The labels file that has `member`, else the default package's.
    fn label_file(&self, before: &BTreeMap<String, LocalMember>, member: &str) -> String {
        if let Some(f) = before.get(&key(member)).and_then(|l| l.files.keys().next()) {
            return file_of(f).to_owned();
        }
        self.tails
            .get(LABELS_FILE)
            .filter(|p| p.starts_with(self.default_base.trim_end_matches("/main/default")))
            .cloned()
            .unwrap_or_else(|| format!("{}/{LABELS_FILE}", self.default_base))
    }
}

/// `force-app/main/default/classes/A.cls` → (`force-app/main/default`, `classes/A.cls`): the
/// tail starts at the first directory that is a metadata type's.
fn split(dirs: &[String], path: &str) -> Option<(String, String)> {
    let reg = registry::get();
    let dir = dirs
        .iter()
        .filter(|d| path.starts_with(&format!("{d}/")))
        .max_by_key(|d| d.len())?;
    let rest = &path[dir.len() + 1..];
    let segs: Vec<&str> = rest.split('/').collect();
    let i = segs[..segs.len().saturating_sub(1)]
        .iter()
        .position(|s| *s == "digitalExperiences" || reg.in_dir(s).next().is_some())?;
    let base = if i == 0 {
        dir.clone()
    } else {
        format!("{dir}/{}", segs[..i].join("/"))
    };
    Some((base, segs[i..].join("/")))
}

/// `CustomField:Account.F__c` → `CustomObject:Account`.
fn parent_member(member: &str) -> Option<String> {
    let (kind, name) = member.split_once(':')?;
    let parent = registry::get().parent(kind)?;
    let (pname, _) = name.split_once('.')?;
    Some(format!("{}:{pname}", parent.name))
}

fn is_label(member: &str) -> bool {
    member
        .split_once(':')
        .is_some_and(|(k, _)| k == "CustomLabel")
}

/// A decomposed parent with nothing of its own (only its children were retrieved): writing
/// it would blank the local parent.
fn is_empty_parent(member: &str, bytes: &[u8]) -> bool {
    let decomposed = member
        .split_once(':')
        .and_then(|(k, _)| registry::get().by_name(k))
        .is_some_and(|t| t.adapter() == "decomposed");
    decomposed
        && xml::parse(&String::from_utf8_lossy(bytes))
            .is_ok_and(|root| root.elements().next().is_none())
}

enum Edit {
    Put(String, Element),
    Remove(String),
}

fn edit_labels(root: &Path, file: &str, edits: Vec<Edit>) -> Result<()> {
    let path = root.join(file);
    let mut doc = match std::fs::read(&path) {
        Ok(b) => parse(file, &b)?,
        Err(_) => {
            let mut e = Element::new("CustomLabels");
            e.attrs.push(("xmlns".into(), xml::NS.into()));
            e
        }
    };
    for edit in edits {
        let pos = |doc: &Element, name: &str| {
            doc.children.iter().position(|n| {
                matches!(n, Node::Element(e) if e.name == "labels" && e.child_text("fullName").as_deref() == Some(name))
            })
        };
        match edit {
            Edit::Put(name, e) => match pos(&doc, &name) {
                Some(i) => doc.children[i] = Node::Element(e),
                None => doc.children.push(Node::Element(e)),
            },
            Edit::Remove(name) => {
                if let Some(i) = pos(&doc, &name) {
                    doc.children.remove(i);
                }
            }
        }
    }
    write(root, file, xml::write(&doc).as_bytes())
}

fn parse(path: &str, bytes: &[u8]) -> Result<Element> {
    xml::parse(&String::from_utf8_lossy(bytes)).map_err(|reason| {
        orgpunk_convert::Error::Xml {
            path: PathBuf::from(path),
            reason,
        }
        .into()
    })
}

fn write(root: &Path, rel: &str, bytes: &[u8]) -> Result<()> {
    let path = root.join(rel);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;
    }
    std::fs::write(&path, bytes).map_err(|source| Error::Io { path, source })
}

/// Removes a file and the directories it leaves empty.
fn remove(root: &Path, rel: &str) -> Result<()> {
    let path = root.join(rel);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(Error::Io { path, source }),
    }
    let mut dir = path.parent();
    while let Some(d) = dir.filter(|d| *d != root) {
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::scan;
    use orgpunk_convert::to_source;

    fn w(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn read(root: &Path, rel: &str) -> String {
        std::fs::read_to_string(root.join(rel)).unwrap_or_else(|_| panic!("{rel} missing"))
    }

    #[test]
    fn writes_pulled_members_where_the_project_keeps_them() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        w(
            r,
            "sfdx-project.json",
            r#"{"packageDirectories":[{"path":"app"},{"path":"main-app","default":true}]}"#,
        );
        w(r, "app/classes/A.cls", "// local A");
        w(r, "app/classes/A.cls-meta.xml", "<ApexClass/>");
        w(
            r,
            "app/objects/Inv__c/Inv__c.object-meta.xml",
            "<CustomObject><label>Local</label></CustomObject>",
        );
        w(
            r,
            "app/objects/Inv__c/fields/Mine__c.field-meta.xml",
            "<CustomField><fullName>Mine__c</fullName><label>local edit</label></CustomField>",
        );
        w(
            r,
            "app/labels/CustomLabels.labels-meta.xml",
            "<CustomLabels><labels><fullName>Keep</fullName><value>k</value></labels><labels><fullName>L</fullName><value>old</value></labels></CustomLabels>",
        );
        w(r, "app/staticresources/lib/old.js", "old");
        w(r, "app/classes/NotInOrg.cls", "// only here");
        w(r, "app/classes/NotInOrg.cls-meta.xml", "<ApexClass/>");
        w(
            r,
            "app/staticresources/lib.resource-meta.xml",
            "<StaticResource><contentType>application/zip</contentType></StaticResource>",
        );
        let project = SfdxProject::load(r).unwrap();
        let before = scan(&project).unwrap();

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("new.js", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, b"new").unwrap();
        let zipped = zip.finish().unwrap().into_inner();
        let mdapi: to_source::Files = [
            ("classes/A.cls", b"// org A".to_vec()),
            ("classes/A.cls-meta.xml", b"<ApexClass/>".to_vec()),
            ("classes/B.cls", b"// org B".to_vec()),
            ("classes/B.cls-meta.xml", b"<ApexClass/>".to_vec()),
            (
                "objects/Inv__c.object",
                b"<CustomObject xmlns=\"http://soap.sforce.com/2006/04/metadata\"><fields><fullName>Mine__c</fullName><label>org</label></fields><fields><fullName>New__c</fullName></fields></CustomObject>".to_vec(),
            ),
            ("labels/CustomLabels.labels", b"<CustomLabels xmlns=\"http://soap.sforce.com/2006/04/metadata\"><labels><fullName>L</fullName><value>org</value></labels><labels><fullName>N</fullName><value>n</value></labels></CustomLabels>".to_vec()),
            ("staticresources/lib.resource", zipped),
            ("staticresources/lib.resource-meta.xml", b"<StaticResource><contentType>application/zip</contentType></StaticResource>".to_vec()),
        ]
        .into_iter()
        .map(|(p, b)| (p.to_owned(), b))
        .collect();
        let tree = to_source::convert(&mdapi).unwrap();
        let pulled: BTreeSet<String> = [
            "ApexClass:A",
            "ApexClass:B",
            "ApexClass:NotInOrg",
            "CustomField:Inv__c.New__c",
            "CustomLabel:L",
            "CustomLabel:N",
            "StaticResource:lib",
        ]
        .iter()
        .map(|m| key(m))
        .collect();
        let applied = apply(&project, &before, &tree, &pulled, &[]).unwrap();

        assert_eq!(
            read(r, "app/classes/A.cls"),
            "// org A",
            "overwritten in place"
        );
        assert_eq!(
            read(r, "app/classes/NotInOrg.cls"),
            "// only here",
            "not returned by the org, not touched"
        );
        assert_eq!(
            read(r, "main-app/main/default/classes/B.cls"),
            "// org B",
            "new: default package"
        );
        assert!(
            read(r, "app/objects/Inv__c/fields/New__c.field-meta.xml").contains("New__c"),
            "next to its object"
        );
        assert!(
            read(r, "app/objects/Inv__c/fields/Mine__c.field-meta.xml").contains("local edit"),
            "not pulled, kept"
        );
        assert!(
            read(r, "app/objects/Inv__c/Inv__c.object-meta.xml").contains("Local"),
            "empty parent not written"
        );
        let labels = read(r, "app/labels/CustomLabels.labels-meta.xml");
        assert!(
            labels.contains("<value>org</value>")
                && labels.contains("Keep")
                && !labels.contains("old")
        );
        assert!(
            read(
                r,
                "main-app/main/default/labels/CustomLabels.labels-meta.xml"
            )
            .contains("<fullName>N</fullName>")
        );
        assert_eq!(read(r, "app/staticresources/lib/new.js"), "new");
        assert!(
            !r.join("app/staticresources/lib/old.js").exists(),
            "stale file removed"
        );
        assert!(
            applied
                .removed
                .contains(&"app/staticresources/lib/old.js".to_owned())
        );

        // Deletions: a class and a label.
        let before = scan(&project).unwrap();
        apply(
            &project,
            &before,
            &Default::default(),
            &BTreeSet::new(),
            &["ApexClass:A".into(), "CustomLabel:Keep".into()],
        )
        .unwrap();
        assert!(!r.join("app/classes/A.cls").exists());
        assert!(!read(r, "app/labels/CustomLabels.labels-meta.xml").contains("Keep"));
    }
}
