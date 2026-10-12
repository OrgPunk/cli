//! Source format → components → an mdapi directory.
//!
//! Resolution follows SDR's `MetadataResolver`: a file below a strict directory (`lwc/`,
//! `objects/`, …) belongs to that directory's type; otherwise the `-meta.xml` suffix names the
//! type. Supported strategies: default (`X.suffix-meta.xml` → `X.suffix`), matching content
//! files (`X.cls` + `X.cls-meta.xml`), bundles, in-folder types, decomposed types (CustomObject,
//! CustomObjectTranslation, Bot), CustomLabels, and mixed content (StaticResource, Document,
//! ExperienceBundle). Anything else is an explicit error, never a silent skip.
//!
//! Decomposed components and CustomLabels merge across package directories, as in SDR: fields
//! of `Account` from two packages end up in one `objects/Account.object`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use crate::forceignore::ForceIgnore;
use crate::manifest::Manifest;
use crate::registry::{self, MetadataType};
use crate::replace::Replacements;
use crate::xml::{self, Element, Node};
use crate::{Error, Result};

const META: &str = "-meta.xml";

/// One metadata component found in source.
#[derive(Debug, Clone)]
pub struct SourceComponent {
    /// Type as in package.xml (folders carry their content type: `Report`).
    pub kind: String,
    pub full_name: String,
    /// mdapi path (with `/`) → how to produce it.
    pub files: Vec<(String, Output)>,
    /// Child members listed in package.xml: (type, full name), e.g. `CustomField Account.Foo__c`.
    pub children: Vec<ChildMember>,
    /// Whether package.xml lists the component itself (SDR omits an empty decomposed parent).
    pub listed: bool,
}

/// A child listed in package.xml on its own: `CustomField Account.Foo__c`.
#[derive(Debug, Clone)]
pub struct ChildMember {
    pub kind: String,
    pub full_name: String,
    /// Its own source file, for decomposed children.
    pub path: Option<PathBuf>,
}

/// How an mdapi file is produced.
#[derive(Debug, Clone)]
pub enum Output {
    Copy(PathBuf),
    /// A decomposed component put back together.
    Recompose(Recompose),
    /// Several files of a non-decomposed type merged (CustomLabels).
    Merge(Merge),
    /// An expanded static resource zipped: the directory and its files.
    Zip {
        dir: PathBuf,
        files: Vec<PathBuf>,
    },
}

#[derive(Debug, Clone)]
pub struct Recompose {
    /// Root element: the type name.
    pub kind: String,
    pub parent: Option<PathBuf>,
    pub children: Vec<ChildFile>,
}

#[derive(Debug, Clone)]
pub struct ChildFile {
    /// Element the child goes under: `fields`, `listViews`, …
    pub group: String,
    /// Element whose value orders the children; `fullName`, then `name` when unset.
    pub unique_id: Option<String>,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Merge {
    pub kind: String,
    /// Element of each child: `labels`.
    pub group: String,
    pub unique_id: String,
    pub files: Vec<PathBuf>,
}

impl Output {
    /// A source file to name in messages.
    pub fn origin(&self) -> PathBuf {
        match self {
            Output::Copy(p) => p.clone(),
            Output::Recompose(r) => r
                .parent
                .clone()
                .or_else(|| r.children.first().map(|c| c.path.clone()))
                .unwrap_or_default(),
            Output::Merge(m) => m.files.first().cloned().unwrap_or_default(),
            Output::Zip { dir, .. } => dir.clone(),
        }
    }

    /// Every source file this output reads.
    pub fn sources(&self) -> Vec<&Path> {
        match self {
            Output::Copy(p) => vec![p],
            Output::Recompose(r) => r
                .parent
                .iter()
                .map(PathBuf::as_path)
                .chain(r.children.iter().map(|c| c.path.as_path()))
                .collect(),
            Output::Merge(m) => m.files.iter().map(PathBuf::as_path).collect(),
            Output::Zip { files, .. } => files.iter().map(PathBuf::as_path).collect(),
        }
    }
    /// The file's bytes in mdapi format; source files are read through `rd`.
    pub fn render(&self, rd: &Replacements) -> Result<Vec<u8>> {
        match self {
            Output::Copy(p) => rd.read(p),
            Output::Recompose(r) => recompose(r, rd).map(String::into_bytes),
            Output::Merge(m) => merge(m, rd).map(String::into_bytes),
            Output::Zip { dir, files } => zip_dir(dir, files, rd),
        }
    }
}

#[derive(Debug, Default)]
pub struct Resolved {
    pub components: Vec<SourceComponent>,
    /// Files that are not part of any component, and .forceignore problems.
    pub warnings: Vec<String>,
    /// Applied to source files by [`Resolved::write_mdapi`]; none by default.
    pub replacements: Replacements,
}

impl Resolved {
    /// package.xml for these components.
    pub fn manifest(&self, api_version: Option<&str>) -> Manifest {
        let mut m = Manifest {
            version: api_version.map(str::to_owned),
            ..Manifest::default()
        };
        for c in &self.components {
            if c.listed {
                m.add(&c.kind, &c.full_name);
            }
            for ChildMember {
                kind,
                full_name: name,
                ..
            } in &c.children
            {
                m.add(kind, name);
            }
        }
        m
    }

    /// Writes an mdapi directory: the components' files and package.xml.
    pub fn write_mdapi(&self, out: &Path, api_version: Option<&str>) -> Result<()> {
        for c in &self.components {
            for (rel, output) in &c.files {
                let dst = out.join(rel);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                        path: parent.to_owned(),
                        source,
                    })?;
                }
                match output {
                    Output::Copy(src) if !self.replacements.applies_to(src) => {
                        std::fs::copy(src, &dst).map_err(|source| Error::Io {
                            path: src.clone(),
                            source,
                        })?;
                    }
                    other => write(&dst, &other.render(&self.replacements)?)?,
                }
            }
        }
        write(
            &out.join("package.xml"),
            self.manifest(api_version).to_xml().as_bytes(),
        )
    }
}

fn read(p: &Path) -> Result<Vec<u8>> {
    std::fs::read(p).map_err(|source| Error::Io {
        path: p.to_owned(),
        source,
    })
}

fn write(p: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(p, data).map_err(|source| Error::Io {
        path: p.to_owned(),
        source,
    })
}

fn parse_xml(p: &Path) -> Result<Element> {
    xml_from(p, read(p)?)
}

/// A source file as XML, after its replacements.
fn parse_source(rd: &Replacements, p: &Path) -> Result<Element> {
    xml_from(p, rd.read(p)?)
}

fn xml_from(p: &Path, bytes: Vec<u8>) -> Result<Element> {
    let text = String::from_utf8(bytes).map_err(|e| Error::Xml {
        path: p.to_owned(),
        reason: e.to_string(),
    })?;
    xml::parse(&text).map_err(|reason| Error::Xml {
        path: p.to_owned(),
        reason,
    })
}

/// Parts of components that merge across package directories.
#[derive(Default)]
struct Merging {
    /// (type, name) → parent file and children.
    decomposed: BTreeMap<(String, String), Decomposed>,
    /// CustomLabels name → files.
    labels: BTreeMap<String, Vec<PathBuf>>,
    /// Duplicates resolved the way SDR does, to report.
    warnings: Vec<String>,
}

struct Decomposed {
    t: &'static MetadataType,
    parent: Option<PathBuf>,
    /// (child type, child name, file)
    children: Vec<(&'static MetadataType, String, PathBuf)>,
}

/// Finds the components in the given package directories.
pub fn resolve(dirs: &[PathBuf]) -> Result<Resolved> {
    let mut out = Resolved::default();
    let mut merging = Merging::default();
    let mut components = Vec::new();
    for dir in dirs {
        let ignore = ForceIgnore::find(dir)?;
        for w in &ignore.warnings {
            if !out.warnings.contains(w) {
                out.warnings.push(w.clone());
            }
        }
        let files = list_files(dir, &ignore)?;
        components.extend(resolve_dir(dir, &files, &mut merging, &mut out.warnings)?);
    }
    components.extend(finish_decomposed(merging.decomposed)?);
    components.extend(finish_labels(merging.labels)?);
    out.warnings.extend(merging.warnings);

    // A component in several package directories: the last one wins, as in SDR.
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let mut unique: Vec<SourceComponent> = Vec::with_capacity(components.len());
    for c in components {
        let key = (c.kind.clone(), c.full_name.clone());
        match index.get(&key) {
            Some(&i) => {
                if !same_sources(&unique[i], &c)? {
                    out.warnings.push(format!(
                        "{}:{} is defined twice, using {} and ignoring {} (as sf does)",
                        c.kind,
                        c.full_name,
                        origin(&c).display(),
                        origin(&unique[i]).display()
                    ));
                }
                unique[i] = c;
            }
            None => {
                index.insert(key, unique.len());
                unique.push(c);
            }
        }
    }
    let mut components = unique;

    let mut by_mdapi_path: HashMap<String, PathBuf> = HashMap::new();
    for c in &components {
        for (rel, o) in &c.files {
            if let Some(prev) = by_mdapi_path.insert(rel.clone(), o.origin()) {
                return Err(Error::Duplicate {
                    component: rel.clone(),
                    first: prev,
                    second: o.origin(),
                });
            }
        }
    }
    components.sort_by(|a, b| (&a.kind, &a.full_name).cmp(&(&b.kind, &b.full_name)));
    out.components = components;
    Ok(out)
}

fn origin(c: &SourceComponent) -> PathBuf {
    c.files.first().map(|(_, o)| o.origin()).unwrap_or_default()
}

/// Whether two definitions of a component produce the same files from the same bytes.
fn same_sources(a: &SourceComponent, b: &SourceComponent) -> Result<bool> {
    let paths = |c: &SourceComponent| c.files.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
    let sources = |c: &SourceComponent| -> Vec<PathBuf> {
        c.files
            .iter()
            .flat_map(|(_, o)| o.sources().into_iter().map(Path::to_owned))
            .collect()
    };
    let (sa, sb) = (sources(a), sources(b));
    if paths(a) != paths(b) || sa.len() != sb.len() {
        return Ok(false);
    }
    for (x, y) in sa.iter().zip(&sb) {
        if read(x)? != read(y)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Files below `dir` that .forceignore accepts, as paths relative to `dir`, sorted.
fn list_files(dir: &Path, ignore: &ForceIgnore) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Err(Error::Io {
            path: dir.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "package directory not found",
            ),
        });
    }
    let mut files = Vec::new();
    let mut walk = walkdir::WalkDir::new(dir).sort_by_file_name().into_iter();
    while let Some(entry) = walk.next() {
        let entry = entry.map_err(|e| Error::Io {
            path: e.path().unwrap_or(dir).to_owned(),
            source: e
                .into_io_error()
                .unwrap_or_else(|| std::io::Error::other("symlink loop")),
        })?;
        let is_dir = entry.file_type().is_dir();
        if entry.depth() > 0 && ignore.denies(entry.path(), is_dir) {
            if is_dir {
                walk.skip_current_dir();
            }
            continue;
        }
        if entry.file_type().is_file() {
            files.push(
                entry
                    .path()
                    .strip_prefix(dir)
                    .expect("inside dir")
                    .to_owned(),
            );
        }
    }
    Ok(files)
}

fn segments(p: &Path) -> Vec<String> {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect()
}

/// `X.suffix-meta.xml` → (`X`, `suffix`).
fn split_meta(file_name: &str) -> Option<(&str, &str)> {
    file_name.strip_suffix(META)?.rsplit_once('.')
}

/// Mixed content of one component, gathered while walking.
#[derive(Default)]
struct Mixed {
    meta: Option<PathBuf>,
    /// Content files with their path below the component's own location.
    content: Vec<(String, PathBuf)>,
    /// The content is a directory (`staticresources/X/…`), not a single file.
    is_dir: bool,
    /// Name of the single content file, e.g. `logo.png`.
    file_name: Option<String>,
}

/// Components of one package directory; `files` are relative to it.
fn resolve_dir(
    root: &Path,
    files: &[PathBuf],
    merging: &mut Merging,
    warnings: &mut Vec<String>,
) -> Result<Vec<SourceComponent>> {
    let reg = registry::get();
    let present: BTreeSet<&Path> = files.iter().map(PathBuf::as_path).collect();
    let mut claimed: BTreeSet<&Path> = BTreeSet::new();
    let mut bundles: BTreeMap<(String, String), SourceComponent> = BTreeMap::new();
    let mut mixed: BTreeMap<(String, String), (&'static MetadataType, Mixed)> = BTreeMap::new();
    let mut experiences: BTreeMap<(String, String), SourceComponent> = BTreeMap::new();
    let mut out = Vec::new();

    for rel in files {
        let segs = segments(rel);
        let (file_name, parents) = segs.split_last().expect("a file has a name");
        let abs = root.join(rel);
        // Folder metadata (`documents/F.documentFolder-meta.xml`) resolves by suffix even
        // inside a strict directory.
        let is_folder_meta = split_meta(file_name)
            .and_then(|(_, s)| reg.by_suffix(s))
            .is_some_and(|t| t.folder_content_type.is_some());
        // The type of a strict directory with plain files (`sites/` → CustomSite) wins over
        // the suffix: `.site` alone would be SiteDotCom.
        let mut strict = None;

        if let Some(i) = parents.iter().rposition(|s| *s == DIGITAL_EXPERIENCES) {
            claimed.insert(rel);
            let (kind, name, mdapi) = digital_experience(&segs[i + 1..], &abs)?;
            experiences
                .entry((kind.to_owned(), name.clone()))
                .or_insert_with(|| simple(kind, &name, Vec::new()))
                .files
                .push((mdapi, Output::Copy(abs)));
            continue;
        }

        if let Some((i, t)) = parents
            .iter()
            .enumerate()
            .find_map(|(i, s)| reg.strict_dir(s).map(|t| (i, t)))
            .filter(|_| !is_folder_meta)
        {
            let below = &segs[i + 1..];
            match t.adapter() {
                "bundle" => {
                    claimed.insert(rel);
                    if below.len() < 2 {
                        warnings.push(format!(
                            "{}: not inside a {} bundle, skipped",
                            abs.display(),
                            t.name
                        ));
                        continue;
                    }
                    bundles
                        .entry((t.name.clone(), below[0].clone()))
                        .or_insert_with(|| SourceComponent {
                            kind: t.name.clone(),
                            full_name: below[0].clone(),
                            files: Vec::new(),
                            children: Vec::new(),
                            listed: true,
                        })
                        .files
                        .push((
                            format!("{}/{}", t.directory_name, below.join("/")),
                            Output::Copy(abs),
                        ));
                    continue;
                }
                "decomposed" => {
                    claimed.insert(rel);
                    add_decomposed(t, below, abs, merging)?;
                    continue;
                }
                "mixedContent" => {
                    claimed.insert(rel);
                    let (name, part) = mixed_key(t, below, &abs)?;
                    let entry = &mut mixed
                        .entry((t.name.clone(), name))
                        .or_insert_with(|| (t, Mixed::default()))
                        .1;
                    match part {
                        MixedPart::Meta => {
                            if entry.meta.replace(abs.clone()).is_some() {
                                return Err(Error::Duplicate {
                                    component: format!("{} metadata", t.name),
                                    first: entry.meta.clone().unwrap_or_default(),
                                    second: abs,
                                });
                            }
                        }
                        MixedPart::Dir(path) => {
                            entry.is_dir = true;
                            entry.content.push((path, abs));
                        }
                        MixedPart::File(name) => {
                            entry.file_name = Some(name.clone());
                            entry.content.push((name, abs));
                        }
                    }
                    continue;
                }
                "default" | "matchingContentFile" => strict = Some(t),
                _ => return Err(unsupported(t, abs)),
            }
        }

        let Some(base) = file_name.strip_suffix(META) else {
            continue; // content: claimed by its -meta.xml below, or reported as stray
        };
        let Some((name, suffix)) = base.rsplit_once('.') else {
            return Err(Error::UnknownSuffix(abs));
        };
        let Some(t) = strict
            .filter(|t| t.has_suffix(suffix))
            .or_else(|| reg.by_suffix(suffix))
        else {
            return Err(Error::UnknownSuffix(abs));
        };
        claimed.insert(rel);

        // A folder of an in-folder type: `reports/A/B.reportFolder-meta.xml` → `reports/A/B-meta.xml`.
        if let Some(content_id) = &t.folder_content_type {
            let content = reg.by_id(content_id).expect("registry is consistent");
            let folder = in_type_dir(content, parents, name, &abs)?;
            // These folders go into package.xml with a trailing `/`, so the Metadata API
            // tells them from components of the same name (SDR's `constructFullName`).
            let member = match t.name.as_str() {
                "DashboardFolder" | "ReportFolder" | "EmailTemplateFolder" => format!("{folder}/"),
                _ => folder.clone(),
            };
            out.push(simple(
                &content.name,
                &member,
                vec![(
                    format!("{}/{folder}{META}", content.directory_name),
                    Output::Copy(abs),
                )],
            ));
            continue;
        }

        if t.strategies.as_ref().and_then(|s| s.transformer.as_deref()) == Some("nonDecomposed") {
            // SDR merges every file into one component named after the type, whatever the
            // file name: `labels/CustomLabels.labels`.
            merging.labels.entry(t.name.clone()).or_default().push(abs);
            continue;
        }

        let full_name = if t.in_folder {
            in_type_dir(t, parents, name, &abs)?
        } else {
            name.to_owned()
        };
        let dir = &t.directory_name;
        let meta_path = abs.clone();
        let files = match t.adapter() {
            "default" => vec![(format!("{dir}/{full_name}.{suffix}"), Output::Copy(abs))],
            "matchingContentFile" => {
                let content_rel = rel.with_file_name(base);
                if !present.contains(content_rel.as_path()) {
                    return Err(Error::MissingContent {
                        meta: abs,
                        content: root.join(&content_rel),
                    });
                }
                claimed.insert(present.get(content_rel.as_path()).expect("present"));
                let mdapi = format!("{dir}/{full_name}.{suffix}");
                vec![
                    (mdapi.clone(), Output::Copy(root.join(&content_rel))),
                    (format!("{mdapi}{META}"), Output::Copy(abs)),
                ]
            }
            _ => return Err(unsupported(t, abs)),
        };
        let mut c = simple(&t.name, &full_name, files);
        c.children = xml_children(t, &full_name, &meta_path)?;
        out.push(c);
    }

    for rel in files {
        if !claimed.contains(rel.as_path()) {
            warnings.push(format!(
                "{}: not part of any metadata component, skipped",
                root.join(rel).display()
            ));
        }
    }
    out.extend(bundles.into_values());
    out.extend(finish_digital_experiences(experiences)?);
    for ((_, name), (t, m)) in mixed {
        out.push(finish_mixed(t, &name, m, root)?);
    }
    Ok(out)
}

fn simple(kind: &str, full_name: &str, files: Vec<(String, Output)>) -> SourceComponent {
    SourceComponent {
        kind: kind.to_owned(),
        full_name: full_name.to_owned(),
        files,
        children: Vec::new(),
        listed: true,
    }
}

/// Experience Cloud sites (LWR): `digitalExperiences/<base type>/<site>/`.
const DIGITAL_EXPERIENCES: &str = "digitalExperiences";
const DE_LAYOUT: &str = "digitalExperiences/<base type>/<site>/<content type>/<content name>";

/// Resolves a file below `digitalExperiences/` as SDR's digitalExperience adapter does: the
/// site's `<site>.digitalExperience-meta.xml` is the DigitalExperienceBundle `<base>/<site>`;
/// everything under `<content type>/<content name>/` (including mobile and tablet variants) is
/// the DigitalExperience `<base>/<site>.<content type>/<content name>`. The mdapi layout is the
/// same as in source. Returns the type, the full name and the mdapi path.
fn digital_experience(below: &[String], abs: &Path) -> Result<(&'static str, String, String)> {
    let not_in_layout = || Error::NotInTypeDir {
        path: abs.to_owned(),
        dir: DE_LAYOUT.to_owned(),
    };
    let [base, site, rest @ ..] = below else {
        return Err(not_in_layout());
    };
    let mdapi = format!("{DIGITAL_EXPERIENCES}/{}", below.join("/"));
    match rest {
        [file] if *file == format!("{site}.digitalExperience{META}") => {
            Ok(("DigitalExperienceBundle", format!("{base}/{site}"), mdapi))
        }
        [content_type, content_name, _, ..] => Ok((
            "DigitalExperience",
            format!("{base}/{site}.{content_type}/{content_name}"),
            mdapi,
        )),
        _ => Err(not_in_layout()),
    }
}

/// Every content folder needs its `_meta.json`, and its site the bundle's -meta.xml.
fn finish_digital_experiences(
    experiences: BTreeMap<(String, String), SourceComponent>,
) -> Result<Vec<SourceComponent>> {
    let sites: HashSet<&str> = experiences
        .keys()
        .filter(|(kind, _)| kind == "DigitalExperienceBundle")
        .map(|(_, name)| name.as_str())
        .collect();
    for ((kind, name), c) in &experiences {
        if kind != "DigitalExperience" {
            continue;
        }
        let (site, content) = name.split_once('.').expect("named <site>.<content>");
        let folder = format!("{DIGITAL_EXPERIENCES}/{site}/{content}/");
        let meta = format!("{folder}_meta.json");
        let origin = origin(c);
        if !c.files.iter().any(|(p, _)| *p == meta) {
            return Err(Error::MissingFile {
                path: origin,
                file: meta,
            });
        }
        if !sites.contains(site) {
            let (_, file) = site.split_once('/').expect("<base>/<site>");
            return Err(Error::MissingFile {
                path: origin,
                file: format!("{DIGITAL_EXPERIENCES}/{site}/{file}.digitalExperience{META}"),
            });
        }
    }
    Ok(experiences.into_values().collect())
}

/// Children declared inside a non-decomposed file (`Workflow` → `WorkflowRule Opp.Rule`), which
/// SDR lists in package.xml next to the parent; only child types with an element name and a
/// unique id element have them.
fn xml_children(t: &MetadataType, full_name: &str, path: &Path) -> Result<Vec<ChildMember>> {
    let Some(children) = &t.children else {
        return Ok(Vec::new());
    };
    let mut kinds: Vec<_> = children
        .types
        .values()
        .filter(|c| c.is_addressable != Some(false))
        .filter_map(|c| {
            Some((
                c,
                c.xml_element_name.as_deref()?,
                c.unique_id_element.as_deref()?,
            ))
        })
        .collect();
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    kinds.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    let root = parse_xml(path)?;
    let mut out = Vec::new();
    for (c, element, id) in kinds {
        for e in root.elements().filter(|e| e.name == element) {
            let Some(name) = e.child_text(id) else {
                return Err(Error::Xml {
                    path: path.to_owned(),
                    reason: format!("<{element}> without <{id}>"),
                });
            };
            out.push(ChildMember {
                kind: c.name.clone(),
                full_name: format!("{full_name}.{name}"),
                path: None,
            });
        }
    }
    Ok(out)
}

/// `objects/Account/Account.object-meta.xml` is the parent, `objects/Account/fields/X.field-meta.xml`
/// a child; `below` is the path under the strict directory.
fn add_decomposed(
    t: &'static MetadataType,
    below: &[String],
    abs: PathBuf,
    merging: &mut Merging,
) -> Result<()> {
    let (Some(name), Some(file_name)) = (below.first(), below.last()) else {
        return Err(unsupported(t, abs));
    };
    if below.len() < 2 {
        return Err(Error::NotInTypeDir {
            path: abs,
            dir: format!("{}/<name>", t.directory_name),
        });
    }
    let Some((child_name, suffix)) = split_meta(file_name) else {
        return Err(Error::UnknownSuffix(abs));
    };
    let entry = merging
        .decomposed
        .entry((t.name.clone(), name.clone()))
        .or_insert_with(|| Decomposed {
            t,
            parent: None,
            children: Vec::new(),
        });
    if t.has_suffix(suffix) {
        // The first parent file wins and the children merge, as in SDR.
        match &entry.parent {
            None => entry.parent = Some(abs),
            Some(first) => {
                if read(first)? != read(&abs)? {
                    merging.warnings.push(format!(
                        "{}:{name} is defined twice, using {} and ignoring {} (as sf does)",
                        t.name,
                        first.display(),
                        abs.display()
                    ));
                }
            }
        }
        return Ok(());
    }
    let child = t
        .children
        .as_ref()
        .and_then(|c| c.suffixes.get(suffix).and_then(|id| c.types.get(id)))
        .ok_or_else(|| Error::UnknownSuffix(abs.clone()))?;
    entry.children.push((child, child_name.to_owned(), abs));
    Ok(())
}

fn finish_decomposed(
    decomposed: BTreeMap<(String, String), Decomposed>,
) -> Result<Vec<SourceComponent>> {
    let mut out = Vec::new();
    for ((_, name), d) in decomposed {
        let t = d.t;
        // The first of duplicate children wins, as in SDR.
        let mut seen = HashSet::new();
        let children: Vec<_> = d
            .children
            .into_iter()
            .filter(|(ct, cn, _)| seen.insert((ct.name.clone(), cn.clone())))
            .collect();
        let addressable = |ct: &MetadataType| {
            ct.is_addressable != Some(false) && ct.unaddressable_without_parent != Some(true)
        };
        let has_addressable_children = t
            .children
            .as_ref()
            .is_some_and(|c| c.types.values().any(&addressable));
        // SDR lists the parent only when it has a file with content of its own; without
        // addressable children the parent is the only member there is.
        let listed = match &d.parent {
            _ if !has_addressable_children => true,
            Some(p) => parse_xml(p)?.has_content(),
            None => false,
        };
        let manifest_children = children
            .iter()
            .filter(|(ct, _, _)| addressable(ct))
            .map(|(ct, cn, path)| ChildMember {
                kind: ct.name.clone(),
                full_name: format!("{name}.{cn}"),
                path: Some(path.clone()),
            })
            .collect();
        let recompose = Recompose {
            kind: t.name.clone(),
            parent: d.parent,
            children: children
                .into_iter()
                .map(|(ct, _, path)| ChildFile {
                    group: ct
                        .xml_element_name
                        .clone()
                        .unwrap_or_else(|| ct.directory_name.clone()),
                    unique_id: ct.unique_id_element.clone(),
                    path,
                })
                .collect(),
        };
        let suffix = t.suffix.as_deref().unwrap_or_default();
        out.push(SourceComponent {
            kind: t.name.clone(),
            full_name: name.clone(),
            files: vec![(
                format!("{}/{name}.{suffix}", t.directory_name),
                Output::Recompose(recompose),
            )],
            children: manifest_children,
            listed,
        });
    }
    Ok(out)
}

fn finish_labels(labels: BTreeMap<String, Vec<PathBuf>>) -> Result<Vec<SourceComponent>> {
    let reg = registry::get();
    let t = reg
        .by_name("CustomLabels")
        .expect("CustomLabels is registered");
    let child = reg
        .by_name("CustomLabel")
        .expect("CustomLabel is registered");
    let group = child
        .xml_element_name
        .clone()
        .unwrap_or_else(|| child.directory_name.clone());
    let unique_id = child
        .unique_id_element
        .clone()
        .unwrap_or_else(|| "fullName".to_owned());
    let mut out = Vec::new();
    for (name, files) in labels {
        let mut names = BTreeSet::new();
        for f in &files {
            for e in parse_xml(f)?.elements().filter(|e| e.name == group) {
                if let Some(n) = e.child_text(&unique_id) {
                    names.insert(n);
                }
            }
        }
        let suffix = t.suffix.as_deref().unwrap_or_default();
        out.push(SourceComponent {
            kind: t.name.clone(),
            full_name: name.clone(),
            files: vec![(
                format!("{}/{name}.{suffix}", t.directory_name),
                Output::Merge(Merge {
                    kind: t.name.clone(),
                    group: group.clone(),
                    unique_id: unique_id.clone(),
                    files,
                }),
            )],
            children: names
                .into_iter()
                .map(|n| ChildMember {
                    kind: child.name.clone(),
                    full_name: n,
                    path: None,
                })
                .collect(),
            listed: true,
        });
    }
    Ok(out)
}

enum MixedPart {
    Meta,
    /// A file inside the content directory, with its path below that directory.
    Dir(String),
    /// The single content file, by file name.
    File(String),
}

/// Which component a mixed-content file belongs to, and what part of it the file is.
/// `below` is the path under the strict directory.
fn mixed_key(t: &MetadataType, below: &[String], abs: &Path) -> Result<(String, MixedPart)> {
    // In-folder (Document): the folder path is part of the name.
    let (folder, rest) = if t.in_folder {
        if below.len() < 2 {
            return Err(Error::NotInTypeDir {
                path: abs.to_owned(),
                dir: format!("{}/<folder>", t.directory_name),
            });
        }
        let last = below.len() - 1;
        (below[..last].join("/") + "/", &below[last..])
    } else {
        (String::new(), below)
    };
    let first = &rest[0];
    if rest.len() > 1 {
        return Ok((
            format!("{folder}{first}"),
            MixedPart::Dir(rest[1..].join("/")),
        ));
    }
    if let Some((stem, suffix)) = split_meta(first) {
        // Types without a suffix (ExperienceBundle) take any `X.<ext>-meta.xml` as their metadata.
        if t.suffix.is_none() || t.has_suffix(suffix) {
            return Ok((format!("{folder}{stem}"), MixedPart::Meta));
        }
    }
    let stem = first.rsplit_once('.').map_or(first.as_str(), |(s, _)| s);
    Ok((format!("{folder}{stem}"), MixedPart::File(first.clone())))
}

fn finish_mixed(t: &MetadataType, name: &str, m: Mixed, root: &Path) -> Result<SourceComponent> {
    let dir = &t.directory_name;
    let suffix = t.suffix.as_deref().unwrap_or_default();
    let Some(meta) = m.meta else {
        let content = m
            .content
            .first()
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| root.to_owned());
        return Err(Error::MissingMeta {
            content,
            kind: t.name.clone(),
        });
    };
    if m.content.is_empty() {
        return Err(Error::MissingContent {
            meta: meta.clone(),
            content: meta.with_file_name(name.rsplit('/').next().unwrap_or(name)),
        });
    }
    if !m.is_dir && m.content.len() > 1 {
        return Err(Error::Duplicate {
            component: format!("{}:{name} content", t.name),
            first: m.content[0].1.clone(),
            second: m.content[1].1.clone(),
        });
    }
    let (full_name, files) = match (t.name.as_str(), m.is_dir) {
        ("StaticResource", true) => {
            let content_dir = m.content[0]
                .1
                .ancestors()
                .nth(m.content[0].0.split('/').count())
                .expect("content is below its directory")
                .to_owned();
            (
                name.to_owned(),
                vec![
                    (
                        format!("{dir}/{name}.{suffix}"),
                        Output::Zip {
                            dir: content_dir,
                            files: m.content.into_iter().map(|(_, p)| p).collect(),
                        },
                    ),
                    (format!("{dir}/{name}.{suffix}{META}"), Output::Copy(meta)),
                ],
            )
        }
        ("StaticResource", false) => (
            name.to_owned(),
            vec![
                (
                    format!("{dir}/{name}.{suffix}"),
                    Output::Copy(m.content.into_iter().next().expect("one file").1),
                ),
                (format!("{dir}/{name}.{suffix}{META}"), Output::Copy(meta)),
            ],
        ),
        // Document: the files keep the content's extension, the member name does not (SDR).
        ("Document", false) => {
            let file = m.file_name.expect("single content file");
            let folder = name.rsplit_once('/').map_or("", |(f, _)| f);
            let full = format!("{folder}/{file}");
            (
                name.to_owned(),
                vec![
                    (
                        format!("{dir}/{full}"),
                        Output::Copy(m.content.into_iter().next().expect("one file").1),
                    ),
                    (format!("{dir}/{full}{META}"), Output::Copy(meta)),
                ],
            )
        }
        // ExperienceBundle: the directory as is, next to `X.site-meta.xml`.
        ("ExperienceBundle", true) => {
            let mut files: Vec<(String, Output)> = m
                .content
                .into_iter()
                .map(|(p, abs)| (format!("{dir}/{name}/{p}"), Output::Copy(abs)))
                .collect();
            let meta_name = meta.file_name().map(|n| n.to_string_lossy().into_owned());
            files.push((
                format!("{dir}/{}", meta_name.unwrap_or_default()),
                Output::Copy(meta),
            ));
            (name.to_owned(), files)
        }
        _ => return Err(unsupported(t, meta)),
    };
    Ok(simple(&t.name, &full_name, files))
}

/// The name of an in-folder component: its path below the type's directory, e.g.
/// `reports/Sales/Q1/Pipeline.report-meta.xml` → `Sales/Q1/Pipeline`.
fn in_type_dir(t: &MetadataType, parents: &[String], name: &str, abs: &Path) -> Result<String> {
    let Some(pos) = parents.iter().rposition(|s| *s == t.directory_name) else {
        return Err(Error::NotInTypeDir {
            path: abs.to_owned(),
            dir: t.directory_name.clone(),
        });
    };
    let mut parts: Vec<&str> = parents[pos + 1..].iter().map(String::as_str).collect();
    parts.push(name);
    Ok(parts.join("/"))
}

fn unsupported(t: &MetadataType, path: PathBuf) -> Error {
    Error::Unsupported {
        kind: t.name.clone(),
        strategy: t.adapter().to_owned(),
        path,
    }
}

/// Parent elements first, then one group per child type (groups by name, children ordered by
/// their unique id), as SDR's RecompositionFinalizer does.
fn recompose(r: &Recompose, rd: &Replacements) -> Result<String> {
    let mut root = Element::new(&r.kind);
    root.attrs.push(("xmlns".to_owned(), xml::NS.to_owned()));
    let mut groups: BTreeMap<&str, Vec<(String, Element)>> = BTreeMap::new();
    for c in &r.children {
        let child = parse_source(rd, &c.path)?;
        let key = c
            .unique_id
            .as_deref()
            .and_then(|u| child.child_text(u))
            .or_else(|| child.child_text("fullName"))
            .or_else(|| child.child_text("name"))
            .unwrap_or_default();
        let mut e = Element::new(&c.group);
        e.attrs = child
            .attrs
            .into_iter()
            .filter(|(k, _)| k != "xmlns" && !k.starts_with("xmlns:"))
            .collect();
        e.children = child.children;
        groups.entry(&c.group).or_default().push((key, e));
    }
    if let Some(p) = &r.parent {
        let parent = parse_source(rd, p)?;
        root.attrs
            .extend(parent.attrs.into_iter().filter(|(k, _)| k != "xmlns"));
        // A group replaces parent elements of the same name, like a key overwrite in SDR.
        root.children = parent
            .children
            .into_iter()
            .filter(|n| !matches!(n, Node::Element(e) if groups.contains_key(e.name.as_str())))
            .collect();
    }
    for (_, mut items) in groups {
        items.sort_by(|a, b| xml::locale_cmp(&a.0, &b.0));
        root.children
            .extend(items.into_iter().map(|(_, e)| Node::Element(e)));
    }
    Ok(xml::write(&root))
}

/// CustomLabels: every `labels` element of every file under one empty root, deduplicated by
/// full name (first wins) and sorted.
fn merge(m: &Merge, rd: &Replacements) -> Result<String> {
    let mut root = Element::new(&m.kind);
    root.attrs.push(("xmlns".to_owned(), xml::NS.to_owned()));
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for f in &m.files {
        for e in parse_source(rd, f)?
            .elements()
            .filter(|e| e.name == m.group)
        {
            let key = e.child_text(&m.unique_id).unwrap_or_default();
            if seen.insert(key.clone()) {
                items.push((key, e.clone()));
            }
        }
    }
    items.sort_by(|a, b| xml::locale_cmp(&a.0, &b.0));
    root.children = items.into_iter().map(|(_, e)| Node::Element(e)).collect();
    Ok(xml::write(&root))
}

/// An expanded static resource as a zip: paths relative to its directory, deflate level 9.
fn zip_dir(dir: &Path, files: &[PathBuf], rd: &Replacements) -> Result<Vec<u8>> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(9));
    let zip_err = |e: zip::result::ZipError| Error::Io {
        path: dir.to_owned(),
        source: std::io::Error::other(e),
    };
    for f in files {
        let rel = f
            .strip_prefix(dir)
            .expect("file is inside the resource directory");
        let name = segments(rel).join("/");
        zip.start_file(name, opts).map_err(zip_err)?;
        zip.write_all(&rd.read(f)?).map_err(|source| Error::Io {
            path: f.clone(),
            source,
        })?;
    }
    Ok(zip.finish().map_err(zip_err)?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = dir.path().join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        dir
    }

    fn mdapi_paths(r: &Resolved) -> Vec<String> {
        let mut v: Vec<String> = r
            .components
            .iter()
            .flat_map(|c| c.files.iter().map(|(p, _)| p.clone()))
            .collect();
        v.sort();
        v
    }

    fn names(r: &Resolved) -> Vec<String> {
        r.components
            .iter()
            .map(|c| format!("{}:{}", c.kind, c.full_name))
            .collect()
    }

    #[test]
    fn converts_common_strategies() {
        let d = project(&[
            ("fa/main/default/classes/A.cls", "class A {}"),
            ("fa/main/default/classes/A.cls-meta.xml", "<ApexClass/>"),
            (
                "fa/main/default/layouts/Account-Account Layout.layout-meta.xml",
                "<Layout/>",
            ),
            (
                "fa/other/place/Conf.Default.md-meta.xml",
                "<CustomMetadata/>",
            ),
            ("fa/main/default/lwc/myComp/myComp.js", ""),
            ("fa/main/default/lwc/myComp/myComp.js-meta.xml", ""),
            ("fa/main/default/lwc/myComp/sub/util.js", ""),
            (
                "fa/main/default/reports/Sales.reportFolder-meta.xml",
                "<ReportFolder/>",
            ),
            (
                "fa/main/default/reports/Sales/Q1.reportFolder-meta.xml",
                "<ReportFolder/>",
            ),
            (
                "fa/main/default/reports/Sales/Q1/Pipeline.report-meta.xml",
                "<Report/>",
            ),
            ("fa/main/default/email/T/Hello.email", "hi"),
            (
                "fa/main/default/email/T/Hello.email-meta.xml",
                "<EmailTemplate/>",
            ),
            ("fa/main/default/README.md", "notes"),
        ]);
        let r = resolve(&[d.path().join("fa")]).unwrap();
        assert_eq!(
            names(&r),
            [
                "ApexClass:A",
                "CustomMetadata:Conf.Default",
                "EmailTemplate:T/Hello",
                "Layout:Account-Account Layout",
                "LightningComponentBundle:myComp",
                "Report:Sales/",
                "Report:Sales/Q1/",
                "Report:Sales/Q1/Pipeline",
            ]
        );
        assert_eq!(
            mdapi_paths(&r),
            [
                "classes/A.cls",
                "classes/A.cls-meta.xml",
                "customMetadata/Conf.Default.md",
                "email/T/Hello.email",
                "email/T/Hello.email-meta.xml",
                "layouts/Account-Account Layout.layout",
                "lwc/myComp/myComp.js",
                "lwc/myComp/myComp.js-meta.xml",
                "lwc/myComp/sub/util.js",
                "reports/Sales-meta.xml",
                "reports/Sales/Q1-meta.xml",
                "reports/Sales/Q1/Pipeline.report",
            ]
        );
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("README.md"));
    }

    #[test]
    fn writes_an_mdapi_directory() {
        let d = project(&[
            ("fa/classes/A.cls", "class A {}"),
            ("fa/classes/A.cls-meta.xml", "<ApexClass/>"),
            ("fa/layouts/L.layout-meta.xml", "<Layout/>"),
        ]);
        let r = resolve(&[d.path().join("fa")]).unwrap();
        let out = d.path().join("out");
        r.write_mdapi(&out, Some("62.0")).unwrap();
        assert_eq!(
            fs::read_to_string(out.join("layouts/L.layout")).unwrap(),
            "<Layout/>"
        );
        assert_eq!(
            fs::read_to_string(out.join("classes/A.cls")).unwrap(),
            "class A {}"
        );
        let m = Manifest::parse(&fs::read_to_string(out.join("package.xml")).unwrap()).unwrap();
        assert_eq!(m.version.as_deref(), Some("62.0"));
        assert_eq!(m.types.len(), 2);
    }

    #[test]
    fn honours_forceignore() {
        let d = project(&[
            (".forceignore", "**/profiles/**\n**/__tests__/**\n"),
            ("fa/profiles/Admin.profile-meta.xml", "<Profile/>"),
            ("fa/lwc/c/c.js", ""),
            ("fa/lwc/c/c.js-meta.xml", ""),
            ("fa/lwc/c/__tests__/c.test.js", ""),
            ("fa/.eslintrc.json", "{}"),
        ]);
        let r = resolve(&[d.path().join("fa")]).unwrap();
        assert_eq!(names(&r), ["LightningComponentBundle:c"]);
        assert_eq!(mdapi_paths(&r), ["lwc/c/c.js", "lwc/c/c.js-meta.xml"]);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn errors_are_explicit() {
        let missing = project(&[("fa/classes/A.cls-meta.xml", "")]);
        assert!(matches!(
            resolve(&[missing.path().join("fa")]),
            Err(Error::MissingContent { .. })
        ));

        let unknown = project(&[("fa/x/Thing.gadget-meta.xml", "")]);
        assert!(matches!(
            resolve(&[unknown.path().join("fa")]),
            Err(Error::UnknownSuffix(_))
        ));

        let object = project(&[("fa/uiBundles/app/index.html", "")]);
        assert!(matches!(
            resolve(&[object.path().join("fa")]),
            Err(Error::Unsupported { .. })
        ));

        let loose = project(&[("fa/R.report-meta.xml", "")]);
        assert!(matches!(
            resolve(&[loose.path().join("fa")]),
            Err(Error::NotInTypeDir { .. })
        ));
    }

    const NS_OPEN: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";

    fn render(r: &Resolved, mdapi: &str) -> String {
        let (_, o) = r
            .components
            .iter()
            .flat_map(|c| &c.files)
            .find(|(p, _)| p == mdapi)
            .unwrap_or_else(|| panic!("{mdapi} not produced"));
        String::from_utf8(o.render(&r.replacements).unwrap()).unwrap()
    }

    #[test]
    fn recomposes_objects_across_packages() {
        let field = |name: &str| {
            format!(
                "{NS_OPEN}<CustomField xmlns=\"http://soap.sforce.com/2006/04/metadata\">\
                 <fullName>{name}</fullName><type>Text</type></CustomField>"
            )
        };
        let (b, a) = (field("b__c"), field("A__c"));
        let d = project(&[
            (
                "one/objects/Invoice__c/Invoice__c.object-meta.xml",
                "<CustomObject xmlns=\"http://soap.sforce.com/2006/04/metadata\">\
                 <label>Invoice</label></CustomObject>",
            ),
            ("one/objects/Invoice__c/fields/b__c.field-meta.xml", &b),
            (
                "one/objects/Invoice__c/listViews/All.listView-meta.xml",
                "<ListView xmlns=\"http://soap.sforce.com/2006/04/metadata\">\
                 <fullName>All</fullName></ListView>",
            ),
            ("two/objects/Invoice__c/fields/A__c.field-meta.xml", &a),
            (
                "two/objects/Account/Account.object-meta.xml",
                "<CustomObject xmlns=\"http://soap.sforce.com/2006/04/metadata\"/>",
            ),
            (
                "two/objects/Account/fields/Foo__c.field-meta.xml",
                &field("Foo__c"),
            ),
        ]);
        let r = resolve(&[d.path().join("one"), d.path().join("two")]).unwrap();
        assert_eq!(
            names(&r),
            ["CustomObject:Account", "CustomObject:Invoice__c"]
        );
        assert_eq!(
            render(&r, "objects/Invoice__c.object"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <CustomObject xmlns=\"http://soap.sforce.com/2006/04/metadata\">\n\
             \x20   <label>Invoice</label>\n\
             \x20   <fields>\n\
             \x20       <fullName>A__c</fullName>\n\
             \x20       <type>Text</type>\n\
             \x20   </fields>\n\
             \x20   <fields>\n\
             \x20       <fullName>b__c</fullName>\n\
             \x20       <type>Text</type>\n\
             \x20   </fields>\n\
             \x20   <listViews>\n\
             \x20       <fullName>All</fullName>\n\
             \x20   </listViews>\n\
             </CustomObject>\n"
        );
        let m = r.manifest(None);
        assert_eq!(
            m.types["CustomField"].iter().collect::<Vec<_>>(),
            ["Account.Foo__c", "Invoice__c.A__c", "Invoice__c.b__c"]
        );
        assert_eq!(
            m.types["ListView"].iter().collect::<Vec<_>>(),
            ["Invoice__c.All"]
        );
        // Account's own file is empty: only its fields are listed.
        assert_eq!(
            m.types["CustomObject"].iter().collect::<Vec<_>>(),
            ["Invoice__c"]
        );
    }

    #[test]
    fn merges_labels() {
        let labels = |items: &[(&str, &str)]| {
            let body: String = items
                .iter()
                .map(|(n, v)| {
                    format!("<labels><fullName>{n}</fullName><value>{v}</value></labels>")
                })
                .collect();
            format!(
                "<CustomLabels xmlns=\"http://soap.sforce.com/2006/04/metadata\">{body}</CustomLabels>"
            )
        };
        let d = project(&[
            (
                "one/labels/CustomLabels.labels-meta.xml",
                &labels(&[("Zeta", "z"), ("Alpha", "a1")]),
            ),
            (
                "two/labels/CustomLabels.labels-meta.xml",
                &labels(&[("Alpha", "a2"), ("Beta", "b")]),
            ),
        ]);
        let r = resolve(&[d.path().join("one"), d.path().join("two")]).unwrap();
        let out = render(&r, "labels/CustomLabels.labels");
        let a = out.find("Alpha").unwrap();
        assert!(
            a < out.find("Beta").unwrap() && out.find("Beta").unwrap() < out.find("Zeta").unwrap()
        );
        assert!(
            out.contains("<value>a1</value>") && !out.contains("a2"),
            "first wins"
        );
        let m = r.manifest(None);
        assert_eq!(m.types["CustomLabel"].len(), 3);
        assert_eq!(
            m.types["CustomLabels"].iter().collect::<Vec<_>>(),
            ["CustomLabels"]
        );
    }

    #[test]
    fn mixed_content() {
        let d = project(&[
            (
                "fa/staticresources/lib.resource-meta.xml",
                "<StaticResource/>",
            ),
            ("fa/staticresources/lib/js/a.js", "a"),
            ("fa/staticresources/lib/b.css", "b"),
            (
                "fa/staticresources/logo.resource-meta.xml",
                "<StaticResource/>",
            ),
            ("fa/staticresources/logo.png", "png"),
            (
                "fa/documents/Pub.documentFolder-meta.xml",
                "<DocumentFolder/>",
            ),
            ("fa/documents/Pub/my.logo.png", "png"),
            ("fa/documents/Pub/my.logo.document-meta.xml", "<Document/>"),
            ("fa/experiences/site1.site-meta.xml", "<ExperienceBundle/>"),
            ("fa/experiences/site1/config/site1.json", "{}"),
        ]);
        let r = resolve(&[d.path().join("fa")]).unwrap();
        assert_eq!(
            names(&r),
            [
                "Document:Pub",
                "Document:Pub/my.logo",
                "ExperienceBundle:site1",
                "StaticResource:lib",
                "StaticResource:logo",
            ]
        );
        assert_eq!(
            mdapi_paths(&r),
            [
                "documents/Pub-meta.xml",
                "documents/Pub/my.logo.png",
                "documents/Pub/my.logo.png-meta.xml",
                "experiences/site1.site-meta.xml",
                "experiences/site1/config/site1.json",
                "staticresources/lib.resource",
                "staticresources/lib.resource-meta.xml",
                "staticresources/logo.resource",
                "staticresources/logo.resource-meta.xml",
            ]
        );
        let (_, zip) = r.components[3]
            .files
            .iter()
            .find(|(p, _)| p.ends_with(".resource"))
            .unwrap();
        let bytes = zip.render(&Replacements::default()).unwrap();
        let mut a = ::zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut entries: Vec<String> = (0..a.len())
            .map(|i| a.by_index(i).unwrap().name().to_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, ["b.css", "js/a.js"]);
    }

    #[test]
    fn mixed_content_needs_its_meta() {
        let d = project(&[("fa/staticresources/lib/a.js", "a")]);
        assert!(matches!(
            resolve(&[d.path().join("fa")]),
            Err(Error::MissingMeta { .. })
        ));
    }

    #[test]
    fn manifest_members_follow_sdr() {
        let d = project(&[
            (
                "fa/workflows/Opp.workflow-meta.xml",
                "<Workflow xmlns=\"http://soap.sforce.com/2006/04/metadata\">\
                 <rules><fullName>R1</fullName></rules>\
                 <fieldUpdates><fullName>U1</fullName></fieldUpdates></Workflow>",
            ),
            ("fa/sites/S.site-meta.xml", "<CustomSite/>"),
            (
                "fa/objects/Account/fields/F__c.field-meta.xml",
                "<CustomField><fullName>F__c</fullName></CustomField>",
            ),
            (
                "fa/digitalExperiences/site/Web/Web.digitalExperience-meta.xml",
                "<DigitalExperienceBundle/>",
            ),
            (
                "fa/digitalExperiences/site/Web/sfdc_cms__view/home/_meta.json",
                "{}",
            ),
            (
                "fa/digitalExperiences/site/Web/sfdc_cms__view/home/content.json",
                "{}",
            ),
            (
                "fa/digitalExperiences/site/Web/sfdc_cms__view/home/mobile/mobile.json",
                "{}",
            ),
        ]);
        let r = resolve(&[d.path().join("fa")]).unwrap();
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        let m = r.manifest(None);
        let members = |t: &str| {
            m.types
                .get(t)
                .map(|s| s.iter().cloned().collect::<Vec<_>>())
        };
        assert_eq!(members("Workflow").unwrap(), ["Opp"]);
        assert_eq!(members("WorkflowRule").unwrap(), ["Opp.R1"]);
        assert_eq!(members("WorkflowFieldUpdate").unwrap(), ["Opp.U1"]);
        assert_eq!(members("CustomSite").unwrap(), ["S"]);
        assert_eq!(members("CustomObject"), None, "no parent file, only fields");
        assert_eq!(members("DigitalExperienceBundle").unwrap(), ["site/Web"]);
        assert_eq!(
            members("DigitalExperience").unwrap(),
            ["site/Web.sfdc_cms__view/home"]
        );
        assert!(mdapi_paths(&r).contains(
            &"digitalExperiences/site/Web/sfdc_cms__view/home/mobile/mobile.json".into()
        ));

        let no_meta = project(&[(
            "fa/digitalExperiences/site/Web/sfdc_cms__view/home/content.json",
            "{}",
        )]);
        assert!(matches!(
            resolve(&[no_meta.path().join("fa")]),
            Err(Error::MissingFile { .. })
        ));
    }

    #[test]
    fn duplicates_resolve_as_sdr_does() {
        let obj = |l: &str| {
            format!(
                "<CustomObject xmlns=\"http://soap.sforce.com/2006/04/metadata\">\
                 <label>{l}</label></CustomObject>"
            )
        };
        let d = project(&[
            ("a/classes/A.cls", "// a"),
            ("a/classes/A.cls-meta.xml", "<ApexClass/>"),
            ("b/classes/A.cls", "// b"),
            ("b/classes/A.cls-meta.xml", "<ApexClass/>"),
            ("a/classes/Same.cls", ""),
            ("a/classes/Same.cls-meta.xml", "<ApexClass/>"),
            ("b/classes/Same.cls", ""),
            ("b/classes/Same.cls-meta.xml", "<ApexClass/>"),
            ("a/objects/O__c/O__c.object-meta.xml", &obj("first")),
            ("b/objects/O__c/O__c.object-meta.xml", &obj("second")),
            (
                "b/objects/O__c/fields/F__c.field-meta.xml",
                "<CustomField><fullName>F__c</fullName></CustomField>",
            ),
        ]);
        let r = resolve(&[d.path().join("a"), d.path().join("b")]).unwrap();
        assert_eq!(
            render(&r, "classes/A.cls"),
            "// b",
            "last whole component wins"
        );
        let o = render(&r, "objects/O__c.object");
        assert!(o.contains("first") && !o.contains("second") && o.contains("F__c"));
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
        assert!(r.warnings.iter().any(|w| w.starts_with("ApexClass:A ")));
        assert!(
            r.warnings
                .iter()
                .any(|w| w.starts_with("CustomObject:O__c "))
        );
    }

    #[test]
    fn a_single_field_deploys_with_its_parent_only() {
        let field = |n: &str| {
            format!(
                "<CustomField xmlns=\"http://soap.sforce.com/2006/04/metadata\">\
                 <fullName>{n}</fullName></CustomField>"
            )
        };
        let (a, b) = (field("A__c"), field("B__c"));
        let d = project(&[
            (
                "fa/objects/Inv__c/Inv__c.object-meta.xml",
                "<CustomObject xmlns=\"http://soap.sforce.com/2006/04/metadata\">\
                 <label>Inv</label></CustomObject>",
            ),
            ("fa/objects/Inv__c/fields/A__c.field-meta.xml", &a),
            ("fa/objects/Inv__c/fields/B__c.field-meta.xml", &b),
            ("fa/classes/C.cls", ""),
            ("fa/classes/C.cls-meta.xml", ""),
        ]);
        let mut r = resolve(&[d.path().join("fa")]).unwrap();
        r.retain(
            &[d.path()
                .join("fa/objects/Inv__c/fields/A__c.field-meta.xml")],
            &[],
        )
        .unwrap();
        assert_eq!(names(&r), ["CustomObject:Inv__c"]);
        let out = render(&r, "objects/Inv__c.object");
        assert!(
            out.contains("<label>Inv</label>") && out.contains("A__c") && !out.contains("B__c")
        );
        let m = r.manifest(None);
        assert_eq!(m.types.keys().collect::<Vec<_>>(), ["CustomField"]);

        let mut whole = resolve(&[d.path().join("fa")]).unwrap();
        whole
            .retain(&[d.path().join("fa/objects/Inv__c")], &[])
            .unwrap();
        assert!(render(&whole, "objects/Inv__c.object").contains("B__c"));
        assert!(whole.manifest(None).types.contains_key("CustomObject"));

        // The same through --metadata selectors, together with a path.
        let sel = |s: &str| s.parse::<crate::select::Selector>().unwrap();
        let mut r = resolve(&[d.path().join("fa")]).unwrap();
        r.retain(
            &[d.path().join("fa/classes")],
            &[sel("CustomField:Inv__c.A__c")],
        )
        .unwrap();
        assert_eq!(names(&r), ["ApexClass:C", "CustomObject:Inv__c"]);
        let out = render(&r, "objects/Inv__c.object");
        assert!(out.contains("A__c") && !out.contains("B__c"));

        let mut r = resolve(&[d.path().join("fa")]).unwrap();
        r.retain(&[], &[sel("CustomObject:Inv__c")]).unwrap();
        assert!(render(&r, "objects/Inv__c.object").contains("B__c"));
        assert!(r.manifest(None).types.contains_key("CustomObject"));

        let mut r = resolve(&[d.path().join("fa")]).unwrap();
        assert!(matches!(
            r.retain(&[], &[sel("ApexClass:Missing")]),
            Err(Error::NoMatch(_))
        ));
    }
}
