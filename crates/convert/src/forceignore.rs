//! `.forceignore`: gitignore syntax, paths relative to the file's directory.
//!
//! Follows SDR: the nearest `.forceignore` up from the start directory applies, SDR's default
//! patterns are prepended to it, and without the file nothing is ignored (not even dot files).

use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::{Error, Result};

pub const FILE: &str = ".forceignore";

/// SDR's `ForceIgnore.DEFAULT_IGNORE`, applied only together with a `.forceignore`.
const DEFAULTS: &[&str] = &[
    "**/*.dup",
    "**/.*",
    "!.sf",
    "**/.sf/**",
    "!**/.sf/orgs",
    "!**/.sf/orgs/**",
    "**/.sf/orgs/*/**",
    "!**/.sf/orgs/*/remoteMetadata",
    "!**/.sf/orgs/*/remoteMetadata/**",
    "**/package2-descriptor.json",
    "**/package2-manifest.json",
];

pub struct ForceIgnore {
    matcher: Option<Gitignore>,
    /// Problems in the file worth telling the user about.
    pub warnings: Vec<String>,
}

impl ForceIgnore {
    /// Ignores nothing.
    pub fn none() -> Self {
        Self {
            matcher: None,
            warnings: Vec::new(),
        }
    }

    /// The nearest `.forceignore` in `start` or above it.
    pub fn find(start: &Path) -> Result<Self> {
        match start
            .ancestors()
            .map(|d| d.join(FILE))
            .find(|p| p.is_file())
        {
            Some(path) => Self::load(&path),
            None => Ok(Self::none()),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        let dir = path.parent().unwrap_or(Path::new("."));
        Self::parse(dir, &text).map_err(|reason| Error::ForceIgnore {
            path: path.to_owned(),
            reason,
        })
    }

    fn parse(dir: &Path, text: &str) -> std::result::Result<Self, String> {
        let mut warnings = Vec::new();
        if text
            .lines()
            .any(|l| l.contains('\\') && !l.trim_start().starts_with('#'))
        {
            warnings.push(
                ".forceignore uses `\\` as a folder separator; use `/`, \
                 those rules do not match"
                    .to_owned(),
            );
        }
        if text.contains("**/unpackaged/**") {
            warnings.push(
                ".forceignore contains `**/unpackaged/**`, which ignores every retrieved file"
                    .to_owned(),
            );
        }
        let mut b = GitignoreBuilder::new(dir);
        for line in DEFAULTS.iter().copied().chain(text.lines()) {
            b.add_line(None, line).map_err(|e| e.to_string())?;
        }
        let matcher = b.build().map_err(|e| e.to_string())?;
        Ok(Self {
            matcher: Some(matcher),
            warnings,
        })
    }

    /// Whether `path` (absolute, or relative to the `.forceignore` directory) is ignored,
    /// itself or through one of its parent directories.
    pub fn denies(&self, path: &Path, is_dir: bool) -> bool {
        let Some(m) = &self.matcher else {
            return false;
        };
        let rel: PathBuf = match path.strip_prefix(m.path()) {
            Ok(r) => r.to_owned(),
            Err(_) if path.is_relative() => path.to_owned(),
            // Outside the .forceignore directory: its rules do not reach.
            Err(_) => return false,
        };
        if rel.as_os_str().is_empty() {
            return false;
        }
        m.matched_path_or_any_parents(&rel, is_dir).is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fi(text: &str) -> ForceIgnore {
        ForceIgnore::parse(Path::new("/repo"), text).unwrap()
    }

    fn denies(f: &ForceIgnore, p: &str) -> bool {
        f.denies(Path::new(p), false)
    }

    #[test]
    fn user_rules_and_parents() {
        let f =
            fi("# comment\n**/jsconfig.json\n**/__tests__/**\nforce-app/main/default/profiles\n");
        assert!(denies(
            &f,
            "/repo/force-app/main/default/lwc/a/jsconfig.json"
        ));
        assert!(denies(
            &f,
            "/repo/force-app/main/default/lwc/a/__tests__/a.test.js"
        ));
        assert!(denies(
            &f,
            "/repo/force-app/main/default/profiles/Admin.profile-meta.xml"
        ));
        assert!(!denies(&f, "/repo/force-app/main/default/classes/A.cls"));
        assert!(denies(
            &f,
            "force-app/main/default/profiles/X.profile-meta.xml"
        ));
    }

    #[test]
    fn sdr_defaults_apply_with_a_file() {
        let f = fi("");
        assert!(denies(&f, "/repo/force-app/.eslintrc.json"));
        assert!(denies(&f, "/repo/force-app/main/.hidden/A.cls"));
        assert!(denies(&f, "/repo/force-app/A.cls.dup"));
        assert!(denies(&f, "/repo/force-app/package2-descriptor.json"));
        assert!(!denies(&f, "/repo/force-app/main/default/classes/A.cls"));
    }

    #[test]
    fn negation() {
        let f = fi("**/profiles/**\n!**/profiles/Admin.profile-meta.xml\n");
        assert!(denies(&f, "/repo/a/profiles/Other.profile-meta.xml"));
        assert!(!denies(&f, "/repo/a/profiles/Admin.profile-meta.xml"));
    }

    #[test]
    fn without_a_file_nothing_is_ignored() {
        let f = ForceIgnore::none();
        assert!(!denies(&f, "/repo/force-app/.eslintrc.json"));
    }

    #[test]
    fn outside_the_directory_is_not_ignored() {
        let f = fi("**/*.cls\n");
        assert!(!denies(&f, "/elsewhere/A.cls"));
    }

    #[test]
    fn warns_about_backslashes() {
        let f = fi("force-app\\main\\default\\profiles\n");
        assert_eq!(f.warnings.len(), 1);
    }
}
