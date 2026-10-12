//! An mdapi directory as components: each file belongs to one `Type:Name`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::manifest::Manifest;
use crate::registry::{self, MetadataType};
use crate::{Error, Result};

/// OS junk files that are never sent.
pub const SKIP_FILES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];

pub const DESTRUCTIVE_FILES: &[&str] = &[
    "destructiveChanges.xml",
    "destructiveChangesPre.xml",
    "destructiveChangesPost.xml",
];

#[derive(Debug, Clone)]
pub struct MdapiFile {
    /// Path inside the package, with `/`.
    pub path: String,
    pub abs: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct Component {
    /// Metadata type, e.g. `ApexClass`.
    pub kind: String,
    /// Member name as in package.xml.
    pub name: String,
    pub files: Vec<MdapiFile>,
}

#[derive(Debug)]
pub struct MdapiDir {
    pub manifest: Manifest,
    pub package_xml: MdapiFile,
    /// Sorted by directory and name.
    pub components: Vec<Component>,
    /// destructiveChanges*.xml at the root.
    pub destructive: Vec<MdapiFile>,
}

/// How a file name turns into a member name.
#[derive(Clone, Copy)]
enum Strip<'a> {
    /// Drop the type's suffix.
    Type(&'a MetadataType),
    /// Unknown type: drop the last extension.
    LastExt,
}

pub fn read(dir: &Path) -> Result<MdapiDir> {
    let package_path = dir.join("package.xml");
    if !package_path.is_file() {
        return Err(Error::NoPackageXml(dir.to_owned()));
    }
    let text = std::fs::read_to_string(&package_path).map_err(|source| Error::Io {
        path: package_path.clone(),
        source,
    })?;
    let manifest = Manifest::parse(&text).map_err(|reason| Error::Manifest {
        path: package_path.clone(),
        reason,
    })?;

    let mut package_xml = None;
    let mut destructive = Vec::new();
    let mut known: BTreeMap<(&'static str, String), Vec<MdapiFile>> = BTreeMap::new();
    let mut unknown: BTreeMap<String, BTreeMap<String, Vec<MdapiFile>>> = BTreeMap::new();

    for entry in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let entry = entry.map_err(|e| Error::Io {
            path: e.path().unwrap_or(dir).to_owned(),
            source: e
                .into_io_error()
                .unwrap_or_else(|| std::io::Error::other("symlink loop")),
        })?;
        if !entry.file_type().is_file()
            || SKIP_FILES.contains(&entry.file_name().to_str().unwrap_or(""))
        {
            continue;
        }
        let rel = entry.path().strip_prefix(dir).expect("inside dir");
        let segs: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let size = entry
            .metadata()
            .map_err(|e| Error::Io {
                path: entry.path().to_owned(),
                source: e.into(),
            })?
            .len();
        let file = MdapiFile {
            path: segs.join("/"),
            abs: entry.into_path(),
            size,
        };
        match segs.as_slice() {
            [name] if name == "package.xml" => package_xml = Some(file),
            [name] if DESTRUCTIVE_FILES.contains(&name.as_str()) => destructive.push(file),
            [_] => return Err(Error::StrayFile(file.path)),
            [top, rest @ ..] => match resolve(top, rest) {
                Some((kind, name)) => known.entry((kind, name)).or_default().push(file),
                None => unknown
                    .entry(top.clone())
                    .or_default()
                    .entry(plain_name(rest, Strip::LastExt))
                    .or_default()
                    .push(file),
            },
            [] => unreachable!("a file has a name"),
        }
    }

    let mut components: Vec<Component> = known
        .into_iter()
        .map(|((kind, name), files)| Component {
            kind: kind.to_owned(),
            name,
            files,
        })
        .collect();
    for (dir_name, members) in unknown {
        let kind =
            guess_type(&manifest, members.keys()).ok_or(Error::UnknownDir { dir: dir_name })?;
        components.extend(members.into_iter().map(|(name, files)| Component {
            kind: kind.clone(),
            name,
            files,
        }));
    }
    components.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));

    Ok(MdapiDir {
        manifest,
        package_xml: package_xml.expect("checked above"),
        components,
        destructive,
    })
}

/// The type and member name of the file `dir/rest…`, by directory and suffix.
fn resolve(dir: &str, rest: &[String]) -> Option<(&'static str, String)> {
    let content: Vec<&'static MetadataType> = registry::get()
        .in_dir(dir)
        .filter(|t| t.folder_content_type.is_none())
        .collect();
    let t = match content.as_slice() {
        [] => return None,
        [t] => *t,
        // Several types share the directory (`wave/`): the suffix decides.
        many => {
            let last = rest.last()?;
            let base = last.strip_suffix("-meta.xml").unwrap_or(last);
            let ext = base.rsplit_once('.')?.1;
            *many.iter().find(|t| t.has_suffix(ext))?
        }
    };
    Some((t.name.as_str(), member_name(t, rest)))
}

/// The member name of a file below a known type's directory (`rest` is the path below it).
fn member_name(t: &MetadataType, rest: &[String]) -> String {
    if !t.in_folder {
        return plain_name(rest, Strip::Type(t));
    }
    let joined = rest.join("/");
    match joined.strip_suffix("-meta.xml") {
        // `Folder-meta.xml` describes the folder itself; `Folder/Doc.png-meta.xml` a document.
        Some(base) => strip_name(base, Strip::Type(t)),
        None => strip_name(&joined, Strip::Type(t)),
    }
}

/// Non-folder types: the first segment is the component (a file or a bundle directory).
fn plain_name(rest: &[String], strip: Strip) -> String {
    let first = &rest[0];
    if rest.len() > 1 {
        return first.clone();
    }
    let base = first.strip_suffix("-meta.xml").unwrap_or(first);
    strip_name(base, strip)
}

fn strip_name(name: &str, strip: Strip) -> String {
    match name.rsplit_once('.') {
        Some((n, ext)) if !n.is_empty() => match strip {
            // Content with its own extension (`logo.png` of a Document) keeps it.
            Strip::Type(t) if !t.has_suffix(ext) => name.to_owned(),
            _ => n.to_owned(),
        },
        _ => name.to_owned(),
    }
}
/// The package.xml type that lists the most of these members.
fn guess_type<'a>(manifest: &Manifest, names: impl Iterator<Item = &'a String>) -> Option<String> {
    let mut hits: HashMap<&str, usize> = HashMap::new();
    for name in names {
        for (kind, members) in &manifest.types {
            if members.contains(name) {
                *hits.entry(kind).or_default() += 1;
            }
        }
    }
    hits.into_iter()
        .max_by_key(|(kind, n)| (*n, std::cmp::Reverse(*kind)))
        .map(|(kind, _)| kind.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn mdapi(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = dir.path().join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        dir
    }

    fn keys(d: &MdapiDir) -> Vec<String> {
        d.components
            .iter()
            .map(|c| format!("{}:{} ({})", c.kind, c.name, c.files.len()))
            .collect()
    }

    #[test]
    fn groups_files_into_components() {
        let d = mdapi(&[
            (
                "package.xml",
                "<Package><types><members>X</members><name>Gadget</name></types></Package>",
            ),
            ("destructiveChangesPost.xml", "<Package/>"),
            ("classes/A.cls", ""),
            ("classes/A.cls-meta.xml", ""),
            ("lwc/myComp/myComp.js", ""),
            ("lwc/myComp/myComp.js-meta.xml", ""),
            ("customMetadata/Conf.Default.md", ""),
            ("layouts/Account-Account Layout.layout", ""),
            ("reports/Sales-meta.xml", ""),
            ("reports/Sales/Pipeline.report", ""),
            ("documents/Pub-meta.xml", ""),
            ("documents/Pub/logo.png", ""),
            ("documents/Pub/logo.png-meta.xml", ""),
            ("email/T/Hello.email", ""),
            ("email/T/Hello.email-meta.xml", ""),
            ("wave/X.wdf", ""),
            ("gadgets/X.gadget", ""),
        ]);
        let m = read(d.path()).unwrap();
        assert_eq!(
            keys(&m),
            [
                "ApexClass:A (2)",
                "CustomMetadata:Conf.Default (1)",
                "Document:Pub (1)",
                "Document:Pub/logo.png (2)",
                "EmailTemplate:T/Hello (2)",
                "Gadget:X (1)",
                "Layout:Account-Account Layout (1)",
                "LightningComponentBundle:myComp (2)",
                "Report:Sales (1)",
                "Report:Sales/Pipeline (1)",
                "WaveDataflow:X (1)",
            ]
        );
        assert_eq!(m.destructive.len(), 1);
    }

    #[test]
    fn unknown_dir_without_manifest_match_is_an_error() {
        let d = mdapi(&[("package.xml", "<Package/>"), ("mystery/X.thing", "")]);
        assert!(matches!(read(d.path()), Err(Error::UnknownDir { .. })));
    }

    #[test]
    fn stray_root_file_is_an_error() {
        let d = mdapi(&[("package.xml", "<Package/>"), ("README.md", "")]);
        assert!(matches!(read(d.path()), Err(Error::StrayFile(_))));
    }
}
