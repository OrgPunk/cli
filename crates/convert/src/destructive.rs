//! destructiveChanges manifests: components to delete, before or after the deploy.
//!
//! Callers supply the list (rollback, push); the deploy refuses the files without
//! `--allow-destructive`.

use std::path::Path;

use crate::manifest::Manifest;
use crate::registry;
use crate::{Error, Result};

/// When the org deletes a component relative to the components being deployed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// Before the deploy: `destructiveChangesPre.xml`.
    Pre,
    /// After the deploy: `destructiveChangesPost.xml`, as SDR does by default.
    #[default]
    Post,
}

impl Phase {
    pub fn file_name(self) -> &'static str {
        match self {
            Phase::Pre => "destructiveChangesPre.xml",
            Phase::Post => "destructiveChangesPost.xml",
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Deletions {
    pre: Manifest,
    post: Manifest,
}

impl Deletions {
    /// Adds `kind` `full_name`; the type must be known, and wildcards are not allowed since
    /// the Metadata API ignores them in destructive manifests.
    pub fn add(&mut self, phase: Phase, kind: &str, full_name: &str) -> Result<()> {
        let member = format!("{kind}:{full_name}");
        if registry::get().by_name(kind).is_none() {
            return Err(Error::UnknownType(kind.to_owned()));
        }
        if full_name.is_empty() || full_name.contains('*') {
            return Err(Error::DeleteWildcard(member));
        }
        let (this, other) = match phase {
            Phase::Pre => (&mut self.pre, &self.post),
            Phase::Post => (&mut self.post, &self.pre),
        };
        if other.types.get(kind).is_some_and(|m| m.contains(full_name)) {
            return Err(Error::DeletedTwice(member));
        }
        this.add(kind, full_name);
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.pre.types.is_empty() && self.post.types.is_empty()
    }

    /// Writes the non-empty manifests next to `package` (the package.xml of the same
    /// directory) and returns their file names. A component both deployed and deleted is an
    /// error; the manifests carry the package's API version.
    pub fn write(&self, out: &Path, package: &Manifest) -> Result<Vec<&'static str>> {
        let mut written = Vec::new();
        for (phase, m) in [(Phase::Pre, &self.pre), (Phase::Post, &self.post)] {
            if m.types.is_empty() {
                continue;
            }
            for (kind, members) in &m.types {
                let deployed = package.types.get(kind);
                if let Some(name) = members
                    .iter()
                    .find(|n| deployed.is_some_and(|d| d.contains(*n) || d.contains("*")))
                {
                    return Err(Error::DeployedAndDeleted(format!("{kind}:{name}")));
                }
            }
            let mut m = m.clone();
            m.version.clone_from(&package.version);
            let path = out.join(phase.file_name());
            std::fs::write(&path, m.to_xml()).map_err(|source| Error::Io { path, source })?;
            written.push(phase.file_name());
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_pre_and_post_with_the_package_version() {
        let mut d = Deletions::default();
        d.add(Phase::Pre, "ApexClass", "Old").unwrap();
        d.add(Phase::Post, "CustomField", "Account.Gone__c")
            .unwrap();
        d.add(Phase::default(), "CustomField", "Account.Also__c")
            .unwrap();
        let mut package = Manifest {
            version: Some("62.0".into()),
            ..Manifest::default()
        };
        package.add("ApexClass", "New");

        let dir = tempfile::tempdir().unwrap();
        let files = d.write(dir.path(), &package).unwrap();
        assert_eq!(
            files,
            ["destructiveChangesPre.xml", "destructiveChangesPost.xml"]
        );
        let read = |f: &str| {
            Manifest::parse(&std::fs::read_to_string(dir.path().join(f)).unwrap()).unwrap()
        };
        let pre = read("destructiveChangesPre.xml");
        assert_eq!(pre.version.as_deref(), Some("62.0"));
        assert!(pre.types["ApexClass"].contains("Old"));
        let post = read("destructiveChangesPost.xml");
        assert_eq!(post.types["CustomField"].len(), 2);
    }

    #[test]
    fn only_non_empty_files() {
        let mut d = Deletions::default();
        assert!(d.is_empty());
        d.add(Phase::Post, "ApexClass", "Old").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let files = d.write(dir.path(), &Manifest::default()).unwrap();
        assert_eq!(files, ["destructiveChangesPost.xml"]);
        assert!(!dir.path().join("destructiveChangesPre.xml").exists());
    }

    #[test]
    fn rejects_bad_deletions() {
        let mut d = Deletions::default();
        assert!(matches!(
            d.add(Phase::Post, "Nope", "X"),
            Err(Error::UnknownType(_))
        ));
        assert!(matches!(
            d.add(Phase::Post, "ApexClass", "*"),
            Err(Error::DeleteWildcard(_))
        ));
        d.add(Phase::Pre, "ApexClass", "A").unwrap();
        assert!(matches!(
            d.add(Phase::Post, "ApexClass", "A"),
            Err(Error::DeletedTwice(_))
        ));

        let mut package = Manifest::default();
        package.add("ApexClass", "A");
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            d.write(dir.path(), &package),
            Err(Error::DeployedAndDeleted(_))
        ));
        let mut wildcard = Manifest::default();
        wildcard.add("ApexClass", "*");
        assert!(matches!(
            d.write(dir.path(), &wildcard),
            Err(Error::DeployedAndDeleted(_))
        ));
    }
}
