//! The working copy as members with file hashes, at the granularity the org tracks changes:
//! an object and each of its fields, every custom label, every other component whole.

use std::collections::BTreeMap;
use std::path::Path;

use orgpunk_config::sfdx_project::SfdxProject;
use orgpunk_convert::source::{self, Output};
use orgpunk_convert::xml;
use sha2::{Digest, Sha256};

use crate::{Error, Result, key};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMember {
    /// `Type:Name`.
    pub member: String,
    /// Project-relative path (with `/`) → sha256.
    pub files: BTreeMap<String, String>,
}

/// Every member of the project's package directories, by `key(Type:Name)`.
pub fn scan(project: &SfdxProject) -> Result<BTreeMap<String, LocalMember>> {
    // A package directory that does not exist yet (a pull into a new project) is empty.
    let dirs: Vec<_> = project
        .package_paths()
        .into_iter()
        .filter(|p| p.is_dir())
        .collect();
    let resolved = source::resolve(&dirs)?;
    let mut out = BTreeMap::new();
    let mut add = |member: String, files: BTreeMap<String, String>| {
        out.entry(key(&member))
            .or_insert_with(|| LocalMember {
                member,
                files: BTreeMap::new(),
            })
            .files
            .extend(files);
    };
    let rel = |p: &Path| -> String {
        let r = p.strip_prefix(&project.root).unwrap_or(p);
        r.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    };
    for c in &resolved.components {
        let own = format!("{}:{}", c.kind, c.full_name);
        for (_, o) in &c.files {
            match o {
                // An object and each addressable child on its own; children the org does not
                // track alone (field translations) count for the parent.
                Output::Recompose(r) => {
                    let mut parent = BTreeMap::new();
                    if let Some(p) = &r.parent {
                        parent.insert(rel(p), hash_file(p)?);
                    }
                    for ch in &r.children {
                        let file = (rel(&ch.path), hash_file(&ch.path)?);
                        match c
                            .children
                            .iter()
                            .find(|m| m.path.as_ref() == Some(&ch.path))
                        {
                            Some(m) => add(
                                format!("{}:{}", m.kind, m.full_name),
                                BTreeMap::from([file]),
                            ),
                            None => {
                                parent.insert(file.0, file.1);
                            }
                        }
                    }
                    if !parent.is_empty() {
                        add(own.clone(), parent);
                    }
                }
                // CustomLabels: every label by its element.
                Output::Merge(m) => {
                    for f in &m.files {
                        let text = read(f)?;
                        let root =
                            xml::parse(&String::from_utf8_lossy(&text)).map_err(|reason| {
                                orgpunk_convert::Error::Xml {
                                    path: f.clone(),
                                    reason,
                                }
                            })?;
                        for e in root.elements().filter(|e| e.name == m.group) {
                            let Some(name) = e.child_text(&m.unique_id) else {
                                continue;
                            };
                            let mut one = xml::Element::new(&e.name);
                            one.children = e.children.clone();
                            let h = hash(xml::write(&one).as_bytes());
                            // The label's file, with the label's own hash.
                            add(
                                format!("CustomLabel:{name}"),
                                BTreeMap::from([(format!("{}#{name}", rel(f)), h)]),
                            );
                        }
                    }
                }
                o => {
                    let mut files = BTreeMap::new();
                    for p in o.sources() {
                        files.insert(rel(p), hash_file(p)?);
                    }
                    add(own.clone(), files);
                }
            }
        }
    }
    Ok(out)
}

fn read(p: &Path) -> Result<Vec<u8>> {
    std::fs::read(p).map_err(|source| Error::Io {
        path: p.to_owned(),
        source,
    })
}

fn hash_file(p: &Path) -> Result<String> {
    Ok(hash(&read(p)?))
}

pub fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The file part of a state path: labels are kept as `<file>#<label>`.
pub fn file_of(path: &str) -> &str {
    path.split_once('#').map_or(path, |(f, _)| f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn members_at_tracking_granularity() {
        let d = tempfile::tempdir().unwrap();
        let w = |rel: &str, body: &str| {
            let p = d.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        w(
            "sfdx-project.json",
            r#"{"packageDirectories":[{"path":"fa"}]}"#,
        );
        w("fa/classes/A.cls", "class A {}");
        w("fa/classes/A.cls-meta.xml", "<ApexClass/>");
        w(
            "fa/objects/Inv__c/Inv__c.object-meta.xml",
            "<CustomObject><label>I</label></CustomObject>",
        );
        w(
            "fa/objects/Inv__c/fields/F__c.field-meta.xml",
            "<CustomField><fullName>F__c</fullName></CustomField>",
        );
        w(
            "fa/labels/CustomLabels.labels-meta.xml",
            "<CustomLabels><labels><fullName>One</fullName><value>1</value></labels><labels><fullName>Two</fullName><value>2</value></labels></CustomLabels>",
        );
        let project = SfdxProject::load(d.path()).unwrap();
        let m = scan(&project).unwrap();
        let names: Vec<&str> = m.values().map(|l| l.member.as_str()).collect();
        assert_eq!(
            names,
            [
                "ApexClass:A",
                "CustomField:Inv__c.F__c",
                "CustomLabel:One",
                "CustomLabel:Two",
                "CustomObject:Inv__c"
            ]
        );
        assert_eq!(m["apexclass:a"].files.len(), 2);
        assert!(
            m["customlabel:one"]
                .files
                .keys()
                .next()
                .unwrap()
                .ends_with("CustomLabels.labels-meta.xml#One")
        );

        // Changing one label changes only its hash.
        let before = scan(&project).unwrap();
        w(
            "fa/labels/CustomLabels.labels-meta.xml",
            "<CustomLabels><labels><fullName>One</fullName><value>1</value></labels><labels><fullName>Two</fullName><value>22</value></labels></CustomLabels>",
        );
        let after = scan(&project).unwrap();
        assert_eq!(before["customlabel:one"], after["customlabel:one"]);
        assert_ne!(before["customlabel:two"], after["customlabel:two"]);
    }
}
