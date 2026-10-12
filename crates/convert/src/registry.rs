//! Metadata types: the SDR registry built into the binary, plus opk's own hints for ordering.
//!
//! `sdr/metadataRegistry.json` is copied from @salesforce/source-deploy-retrieve (Apache-2.0,
//! see `sdr/NOTICE`). It is parsed on first use, so commands that never touch metadata do not
//! pay for it.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

const SDR_REGISTRY: &str = include_str!("../sdr/metadataRegistry.json");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataType {
    pub id: String,
    pub name: String,
    pub directory_name: String,
    #[serde(default)]
    pub suffix: Option<String>,
    #[serde(default)]
    pub legacy_suffix: Option<String>,
    #[serde(default)]
    pub meta_file_suffix: Option<String>,
    #[serde(default)]
    pub in_folder: bool,
    /// The folder type of an in-folder type: `Report` → `reportfolder`.
    #[serde(default)]
    pub folder_type: Option<String>,
    /// Set on folder types: `ReportFolder` → `report`.
    #[serde(default)]
    pub folder_content_type: Option<String>,
    #[serde(default)]
    pub strict_directory_name: bool,
    #[serde(default)]
    pub strategies: Option<Strategies>,
    #[serde(default)]
    pub children: Option<Children>,
    #[serde(default)]
    pub xml_element_name: Option<String>,
    #[serde(default)]
    pub unique_id_element: Option<String>,
    /// `false` for child types that never appear in package.xml on their own.
    #[serde(default)]
    pub is_addressable: Option<bool>,
    #[serde(default)]
    pub unaddressable_without_parent: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct Strategies {
    /// `default`, `matchingContentFile`, `bundle`, `mixedContent`, `decomposed`, …
    pub adapter: String,
    #[serde(default)]
    pub transformer: Option<String>,
    #[serde(default)]
    pub decomposition: Option<String>,
    #[serde(default)]
    pub recomposition: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Children {
    /// Child type id → type.
    pub types: HashMap<String, MetadataType>,
    /// Child suffix → child type id.
    pub suffixes: HashMap<String, String>,
    /// Source subdirectory → child type id (decomposed types).
    #[serde(default)]
    pub directories: HashMap<String, String>,
}

impl MetadataType {
    pub fn adapter(&self) -> &str {
        self.strategies
            .as_ref()
            .map_or("default", |s| s.adapter.as_str())
    }

    /// Whether `ext` (a file extension without the dot) is this type's suffix.
    pub fn has_suffix(&self, ext: &str) -> bool {
        self.suffix.as_deref() == Some(ext) || self.legacy_suffix.as_deref() == Some(ext)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    types: HashMap<String, MetadataType>,
    /// Suffix → top-level type id.
    suffixes: HashMap<String, String>,
    /// Source directory whose files all belong to one type → type id.
    strict_directory_names: HashMap<String, String>,
}

pub struct Registry {
    /// Top-level types by id.
    types: HashMap<String, MetadataType>,
    /// Type name → (top-level id, child id within it).
    by_name: HashMap<String, (String, Option<String>)>,
    /// Directory name → top-level type ids.
    by_dir: HashMap<String, Vec<String>>,
    suffixes: HashMap<String, String>,
    strict_dirs: HashMap<String, String>,
}

/// The built-in registry.
pub fn get() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Registry::parse(SDR_REGISTRY).expect("built-in SDR registry parses"))
}

impl Registry {
    fn parse(json: &str) -> serde_json::Result<Self> {
        let raw: Raw = serde_json::from_str(json)?;
        let mut by_name = HashMap::new();
        let mut by_dir: HashMap<String, Vec<String>> = HashMap::new();
        for (id, t) in &raw.types {
            by_name.insert(t.name.clone(), (id.clone(), None));
            by_dir
                .entry(t.directory_name.clone())
                .or_default()
                .push(id.clone());
            for (child_id, child) in t.children.iter().flat_map(|c| &c.types) {
                by_name
                    .entry(child.name.clone())
                    .or_insert_with(|| (id.clone(), Some(child_id.clone())));
            }
        }
        for ids in by_dir.values_mut() {
            ids.sort();
        }
        Ok(Self {
            types: raw.types,
            by_name,
            by_dir,
            suffixes: raw.suffixes,
            strict_dirs: raw.strict_directory_names,
        })
    }

    /// API names of all types, top-level and child, in no particular order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }

    /// A type (top-level or child) by its API name, e.g. `CustomField`.
    pub fn by_name(&self, name: &str) -> Option<&MetadataType> {
        let (id, child) = self.by_name.get(name)?;
        let top = &self.types[id];
        match child {
            None => Some(top),
            Some(c) => top.children.as_ref().map(|ch| &ch.types[c]),
        }
    }

    /// The parent of a child type: `CustomField` → `CustomObject`.
    pub fn parent(&self, name: &str) -> Option<&MetadataType> {
        match self.by_name.get(name)? {
            (id, Some(_)) => Some(&self.types[id]),
            (_, None) => None,
        }
    }

    /// Top-level types stored in this mdapi directory, folder types included.
    pub fn in_dir(&self, dir: &str) -> impl Iterator<Item = &MetadataType> {
        self.by_dir
            .get(dir)
            .into_iter()
            .flatten()
            .map(|id| &self.types[id])
    }

    pub fn by_id(&self, id: &str) -> Option<&MetadataType> {
        self.types.get(id)
    }

    /// The top-level type of a source file suffix: `cls` → `ApexClass`.
    pub fn by_suffix(&self, suffix: &str) -> Option<&MetadataType> {
        self.types.get(self.suffixes.get(suffix)?)
    }

    /// The type that owns everything below a source directory with this name (`lwc`, `objects`).
    pub fn strict_dir(&self, dir: &str) -> Option<&MetadataType> {
        self.types.get(self.strict_dirs.get(dir)?)
    }
}

/// The component that holds a package.xml member: `CustomField Account.Foo__c` → `CustomObject Account`.
pub fn owner<'a>(kind: &'a str, member: &'a str) -> (&'a str, &'a str) {
    match get().parent(kind) {
        Some(p) if p.name == "CustomLabels" => ("CustomLabels", "CustomLabels"),
        Some(p) => (p.name.as_str(), member.split('.').next().unwrap_or(member)),
        None => (kind, member),
    }
}

/// Types that reference many components but are referenced only by each other:
/// they go after what they reference, and a mention of their name elsewhere is not a dependency.
const SINKS: &[&str] = &[
    "CustomObjectTranslation",
    "GlobalValueSetTranslation",
    "MutingPermissionSet",
    "PermissionSet",
    "PermissionSetGroup",
    "Profile",
    "ProfilePasswordPolicy",
    "ProfileSessionSetting",
    "StandardValueSetTranslation",
    "Translations",
];

/// Types whose content files are binary (their `-meta.xml` is not).
const BINARY: &[&str] = &["ContentAsset", "Document", "StaticResource"];

pub fn is_sink(kind: &str) -> bool {
    SINKS.contains(&kind)
}

pub fn is_binary(kind: &str) -> bool {
    BINARY.contains(&kind)
}

/// Deploy order preference among components with no dependency between them; lower goes first.
pub fn rank(kind: &str) -> u8 {
    match kind {
        "Settings" | "CustomLabels" => 0,
        "GlobalValueSet" | "StandardValueSet" => 1,
        "CustomObject" => 2,
        "PermissionSetGroup" => 9,
        k if is_sink(k) => 8,
        _ => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdr_registry_loads() {
        let r = get();
        let class = r.by_name("ApexClass").unwrap();
        assert_eq!(class.directory_name, "classes");
        assert_eq!(class.suffix.as_deref(), Some("cls"));
        assert_eq!(class.adapter(), "matchingContentFile");
        assert_eq!(r.by_name("CustomObject").unwrap().adapter(), "decomposed");
        assert!(r.by_name("Report").unwrap().in_folder);
        assert_eq!(r.parent("CustomField").unwrap().name, "CustomObject");
        assert!(r.parent("ApexClass").is_none());
        assert!(r.in_dir("wave").count() > 1);
    }

    #[test]
    fn owners() {
        assert_eq!(
            owner("CustomField", "Account.Foo__c"),
            ("CustomObject", "Account")
        );
        assert_eq!(
            owner("CustomLabel", "Greeting"),
            ("CustomLabels", "CustomLabels")
        );
        assert_eq!(owner("WorkflowRule", "Case.R1"), ("Workflow", "Case"));
        assert_eq!(owner("ApexClass", "A"), ("ApexClass", "A"));
    }
}
