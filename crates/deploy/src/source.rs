//! Deploy from source: the project's package directories converted into a temporary
//! mdapi directory, which then goes through the same packaging as `--metadata-dir`.

use std::path::{Path, PathBuf};

use orgpunk_config::sfdx_project::SfdxProject;
use orgpunk_convert::replace::Replacements;
use orgpunk_convert::select::Selector;
use orgpunk_convert::source;

use crate::{Error, Result};

/// What to take from the project; empty means everything.
#[derive(Default)]
pub struct Selection {
    /// Components with files under these paths.
    pub paths: Vec<PathBuf>,
    /// Components matching `Type`, `Type:Name` or `Type:Prefix*`.
    pub metadata: Vec<Selector>,
}

impl Selection {
    fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.metadata.is_empty()
    }

    fn describe(&self) -> String {
        let paths = self.paths.iter().map(|p| p.display().to_string());
        let metadata = self.metadata.iter().map(Selector::to_string);
        paths.chain(metadata).collect::<Vec<_>>().join(", ")
    }
}

pub struct SourceBuild {
    /// The mdapi directory; removed on drop.
    pub dir: tempfile::TempDir,
    pub components: usize,
    pub warnings: Vec<String>,
}

/// Converts into a temporary directory for a deploy, with the replacements; see
/// [`convert_into`].
pub fn build(
    project: &SfdxProject,
    selection: &Selection,
    fallback_api_version: Option<&str>,
) -> Result<SourceBuild> {
    let dir = tempfile::Builder::new()
        .prefix("orgpunk-mdapi-")
        .tempdir()
        .map_err(|source| Error::Io {
            path: std::env::temp_dir(),
            source,
        })?;
    let done = convert_into(project, selection, fallback_api_version, true, dir.path())?;
    Ok(SourceBuild {
        dir,
        components: done.components,
        warnings: done.warnings,
    })
}

/// What a conversion produced.
pub struct Converted {
    pub components: usize,
    pub warnings: Vec<String>,
}

/// Converts the project, or only the selected components, into the mdapi directory `out`.
/// package.xml gets `sourceApiVersion`, or `fallback_api_version` when the project has none.
/// The project's `replacements` apply with `replacements`: sf applies them on deploy, and on
/// convert only with `SF_APPLY_REPLACEMENTS_ON_CONVERT=true`.
pub fn convert_into(
    project: &SfdxProject,
    selection: &Selection,
    fallback_api_version: Option<&str>,
    replacements: bool,
    out: &Path,
) -> Result<Converted> {
    let unsupported = project.unsupported();
    if !unsupported.is_empty() {
        return Err(Error::UnsupportedProject(unsupported.join(", ")));
    }
    // Canonical roots, so `paths` (canonicalized too) compare by prefix reliably.
    let roots = project
        .package_paths()
        .iter()
        .map(|p| absolute(p))
        .collect::<Result<Vec<_>>>()?;
    let mut resolved = source::resolve(&roots)?;
    if !selection.is_empty() {
        let wanted: Vec<PathBuf> = selection
            .paths
            .iter()
            .map(|p| absolute(p))
            .collect::<Result<_>>()?;
        resolved.retain(&wanted, &selection.metadata)?;
        if resolved.components.is_empty() {
            return Err(Error::NothingToDeploy(selection.describe()));
        }
    }
    let version = project
        .source_api_version
        .as_deref()
        .or(fallback_api_version);
    if replacements {
        resolved.replacements = Replacements::new(&project.root, &project.replacements, |name| {
            std::env::var(name).ok()
        })?;
    }
    resolved.write_mdapi(out, version)?;
    let mut warnings = resolved.warnings;
    warnings.extend(resolved.replacements.warnings());
    Ok(Converted {
        components: resolved.components.len(),
        warnings,
    })
}

fn absolute(p: &Path) -> Result<PathBuf> {
    p.canonicalize().map_err(|source| Error::Io {
        path: p.to_owned(),
        source,
    })
}

/// The project as of a commit, converted for a rollback or a promote.
pub struct CommitBuild {
    /// The mdapi directory; removed on drop.
    pub dir: tempfile::TempDir,
    /// The exported commit; removed on drop.
    _src: tempfile::TempDir,
    pub components: usize,
    /// Wanted members the commit does not have.
    pub missing: Vec<String>,
    pub warnings: Vec<String>,
}

/// Converts the project as of `commit` (of the repository at `root`) into a temporary mdapi
/// directory: all of it, or only `members` (`Type:Name`), with `deletions` next to its
/// package.xml. Replacements apply, as for any deploy.
pub fn build_at_commit(
    root: &Path,
    commit: &str,
    members: Option<&[String]>,
    deletions: &orgpunk_convert::destructive::Deletions,
    fallback_api_version: Option<&str>,
) -> Result<CommitBuild> {
    let tmp = |prefix: &str| {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .map_err(|source| Error::Io {
                path: std::env::temp_dir(),
                source,
            })
    };
    let src = tmp("orgpunk-commit-")?;
    orgpunk_repo::export(root, commit, src.path())?;
    let project = SfdxProject::load(src.path())?;
    let unsupported = project.unsupported();
    if !unsupported.is_empty() {
        return Err(Error::UnsupportedProject(unsupported.join(", ")));
    }
    let roots = project
        .package_paths()
        .iter()
        .map(|p| absolute(p))
        .collect::<Result<Vec<_>>>()?;
    let mut resolved = source::resolve(&roots)?;
    let mut missing = Vec::new();
    if let Some(members) = members {
        let mut selectors = Vec::new();
        for m in members {
            match selector_for(&resolved, m) {
                Some(s) => selectors.push(s),
                None => missing.push(m.clone()),
            }
        }
        selectors.sort_by_key(|s| s.to_string());
        selectors.dedup_by_key(|s| s.to_string());
        resolved.retain(&[], &selectors)?;
    }
    resolved.replacements = Replacements::new(&project.root, &project.replacements, |name| {
        std::env::var(name).ok()
    })?;
    let out = tmp("orgpunk-mdapi-")?;
    let version = project
        .source_api_version
        .as_deref()
        .or(fallback_api_version);
    resolved.write_mdapi(out.path(), version)?;
    deletions.write(out.path(), &resolved.manifest(version))?;
    let mut warnings = resolved.warnings;
    warnings.extend(resolved.replacements.warnings());
    Ok(CommitBuild {
        components: resolved.components.len(),
        dir: out,
        _src: src,
        missing,
        warnings,
    })
}

/// How to select a `Type:Name` member: itself, or its parent when it has no file of its own
/// (a CustomLabel goes with its CustomLabels). `None` when the project does not have it.
fn selector_for(resolved: &source::Resolved, member: &str) -> Option<Selector> {
    let (kind, name) = member.split_once(':')?;
    let same = |k: &str, n: &str| k == kind && n.eq_ignore_ascii_case(name);
    for c in &resolved.components {
        if same(&c.kind, &c.full_name) {
            return Some(Selector {
                kind: c.kind.clone(),
                name: Some(c.full_name.clone()),
            });
        }
        if let Some(ch) = c.children.iter().find(|ch| same(&ch.kind, &ch.full_name)) {
            return Some(match ch.path {
                Some(_) => Selector {
                    kind: ch.kind.clone(),
                    name: Some(ch.full_name.clone()),
                },
                None => Selector {
                    kind: c.kind.clone(),
                    name: Some(c.full_name.clone()),
                },
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git is installed");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn builds_members_as_of_a_commit() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let meta = "<ApexClass xmlns=\"http://soap.sforce.com/2006/04/metadata\"><apiVersion>62.0</apiVersion></ApexClass>";
        write(
            r,
            "sfdx-project.json",
            r#"{"packageDirectories":[{"path":"fa"}],"sourceApiVersion":"62.0"}"#,
        );
        write(r, "fa/classes/A.cls", "// v1");
        write(r, "fa/classes/A.cls-meta.xml", meta);
        write(
            r,
            "fa/labels/CustomLabels.labels-meta.xml",
            "<CustomLabels xmlns=\"http://soap.sforce.com/2006/04/metadata\"><labels><fullName>L1</fullName><value>one</value></labels></CustomLabels>",
        );
        git(r, &["init", "-q"]);
        git(r, &["add", "."]);
        git(r, &["commit", "-qm", "v1"]);
        let v1 = git(r, &["rev-parse", "HEAD"]);
        write(r, "fa/classes/A.cls", "// v2");
        write(r, "fa/classes/C.cls", "// new");
        write(r, "fa/classes/C.cls-meta.xml", meta);
        git(r, &["add", "."]);
        git(r, &["commit", "-qm", "v2"]);

        let mut deletions = orgpunk_convert::destructive::Deletions::default();
        deletions
            .add(orgpunk_convert::destructive::Phase::Post, "ApexClass", "C")
            .unwrap();
        let members: Vec<String> = ["ApexClass:A", "ApexClass:C", "CustomLabel:L1"]
            .map(String::from)
            .to_vec();
        let b = build_at_commit(r, &v1, Some(&members), &deletions, None).unwrap();
        let out = b.dir.path();
        assert_eq!(
            std::fs::read_to_string(out.join("classes/A.cls")).unwrap(),
            "// v1"
        );
        assert!(!out.join("classes/C.cls").exists());
        assert!(out.join("labels/CustomLabels.labels").exists());
        assert!(out.join("destructiveChangesPost.xml").exists());
        assert_eq!(b.missing, ["ApexClass:C"]);

        let all = build_at_commit(r, &v1, None, &Default::default(), None).unwrap();
        assert_eq!(all.components, 2, "A and CustomLabels");
    }
}
