//! `sfdx-project.json`: read as is, never written.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Error, Result};

pub const FILE: &str = "sfdx-project.json";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SfdxProject {
    pub package_directories: Vec<PackageDirectory>,
    #[serde(default)]
    pub source_api_version: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
    /// Alternative decompositions (beta in sf), e.g. `decomposePermissionSetBeta`.
    #[serde(default)]
    pub source_behavior_options: Vec<String>,
    /// The older name of `sourceBehaviorOptions`.
    #[serde(default)]
    pub registry_presets: Vec<String>,
    /// String replacements in source files when converting to mdapi.
    #[serde(default)]
    pub replacements: Vec<Replacement>,
    /// Project root: the directory of the file.
    #[serde(skip)]
    pub root: PathBuf,
}

/// One `replacements` entry, as sf defines it: which files (`filename` or `glob`), what to
/// find (`stringToReplace` or `regexToReplace`), what to put there (`replaceWithEnv` or
/// `replaceWithFile`), and optionally when (`replaceWhenEnv`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Replacement {
    /// Relative to the project root, with `/`.
    pub filename: Option<String>,
    pub glob: Option<String>,
    pub string_to_replace: Option<String>,
    pub regex_to_replace: Option<String>,
    pub replace_with_env: Option<String>,
    /// Relative to the project root.
    pub replace_with_file: Option<String>,
    #[serde(default)]
    pub allow_unset_env_variable: bool,
    /// All must hold for the entry to apply.
    #[serde(default)]
    pub replace_when_env: Vec<EnvCondition>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnvCondition {
    pub env: String,
    /// A string, number or boolean in the file; compared as text.
    pub value: serde_json::Value,
}

impl EnvCondition {
    pub fn value_text(&self) -> String {
        match &self.value {
            serde_json::Value::String(s) => s.clone(),
            v => v.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageDirectory {
    /// Relative to the project root, with `/`.
    pub path: String,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub package: Option<String>,
}

impl SfdxProject {
    /// Reads `<root>/sfdx-project.json`.
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(FILE);
        let text = std::fs::read_to_string(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        let mut p = Self::parse(&text).map_err(|reason| Error::SfdxProject {
            path: path.clone(),
            reason,
        })?;
        p.root = root.to_owned();
        Ok(p)
    }

    fn parse(text: &str) -> std::result::Result<Self, String> {
        // sf tolerates a BOM; serde_json does not.
        let text = text.trim_start_matches('\u{feff}');
        let p: Self = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if p.package_directories.is_empty() {
            return Err("packageDirectories is empty".into());
        }
        for (i, r) in p.replacements.iter().enumerate() {
            let one = |a: bool, b: bool, what: &str| {
                if a == b {
                    Err(format!("replacements[{i}]: needs exactly one of {what}"))
                } else {
                    Ok(())
                }
            };
            one(r.filename.is_some(), r.glob.is_some(), "filename, glob")?;
            one(
                r.string_to_replace.is_some(),
                r.regex_to_replace.is_some(),
                "stringToReplace, regexToReplace",
            )?;
            one(
                r.replace_with_env.is_some(),
                r.replace_with_file.is_some(),
                "replaceWithEnv, replaceWithFile",
            )?;
        }
        if let Some(d) = p.package_directories.iter().find(|d| {
            Path::new(&d.path).is_absolute() || d.path.split(['/', '\\']).any(|s| s == "..")
        }) {
            return Err(format!(
                "package directory `{}` must be inside the project",
                d.path
            ));
        }
        Ok(p)
    }

    /// Absolute paths of the package directories, in file order.
    pub fn package_paths(&self) -> Vec<PathBuf> {
        self.package_directories
            .iter()
            .map(|d| self.root.join(d.path.trim_end_matches(['/', '\\'])))
            .collect()
    }

    /// sf features opk does not support yet; converting such a project would silently
    /// produce a different package, so callers refuse instead.
    pub fn unsupported(&self) -> Vec<String> {
        let mut out = Vec::new();
        for o in self
            .source_behavior_options
            .iter()
            .chain(&self.registry_presets)
        {
            out.push(format!("sourceBehaviorOptions: {o}"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_project() {
        let p = SfdxProject::parse(
            "\u{feff}{\"packageDirectories\": [{\"path\": \"force-app\", \"default\": true}, \
             {\"path\": \"unpackaged/\"}], \"sourceApiVersion\": \"62.0\", \"name\": \"x\"}",
        )
        .unwrap();
        assert_eq!(p.package_directories.len(), 2);
        assert!(p.package_directories[0].default);
        assert_eq!(p.source_api_version.as_deref(), Some("62.0"));
        assert!(p.unsupported().is_empty());
    }

    #[test]
    fn rejects_bad_projects() {
        assert!(SfdxProject::parse("{\"packageDirectories\": []}").is_err());
        assert!(SfdxProject::parse("{}").is_err());
        assert!(SfdxProject::parse("{\"packageDirectories\": [{\"path\": \"../x\"}]}").is_err());
    }

    #[test]
    fn reports_unsupported_features() {
        let p = SfdxProject::parse(
            "{\"packageDirectories\": [{\"path\": \"a\"}], \
             \"sourceBehaviorOptions\": [\"decomposePermissionSetBeta\"], \
             \"replacements\": [{\"filename\": \"x\", \"stringToReplace\": \"a\", \
             \"replaceWithEnv\": \"A\", \"replaceWhenEnv\": [{\"env\": \"E\", \"value\": true}]}]}",
        )
        .unwrap();
        assert_eq!(
            p.unsupported(),
            ["sourceBehaviorOptions: decomposePermissionSetBeta"]
        );
        let r = &p.replacements[0];
        assert_eq!(r.filename.as_deref(), Some("x"));
        assert_eq!(r.replace_when_env[0].value_text(), "true");
    }

    #[test]
    fn replacements_need_one_of_each_pair() {
        let parse = |r: &str| {
            SfdxProject::parse(&format!(
                "{{\"packageDirectories\": [{{\"path\": \"a\"}}], \"replacements\": [{r}]}}"
            ))
        };
        assert!(
            parse(
                "{\"glob\": \"**/*.xml\", \"regexToReplace\": \"a+\", \"replaceWithFile\": \"f\"}"
            )
            .is_ok()
        );
        assert!(parse("{\"filename\": \"x\", \"glob\": \"y\", \"stringToReplace\": \"a\", \"replaceWithEnv\": \"A\"}").is_err());
        assert!(parse("{\"filename\": \"x\", \"replaceWithEnv\": \"A\"}").is_err());
        assert!(parse("{\"filename\": \"x\", \"stringToReplace\": \"a\"}").is_err());
    }

    #[test]
    fn package_paths_are_absolute() {
        let mut p =
            SfdxProject::parse("{\"packageDirectories\": [{\"path\": \"force-app/\"}]}").unwrap();
        p.root = PathBuf::from("/repo");
        assert_eq!(p.package_paths(), [PathBuf::from("/repo/force-app")]);
    }
}
