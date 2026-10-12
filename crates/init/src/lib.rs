//! `opk init`: a new sf project, or opk added to an existing one.
//!
//! Every file is created only when missing, so running it again changes nothing the user
//! has; `.gitignore` only gets the lines it lacks.

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "{0} is not empty and has no sfdx-project.json; --force scaffolds into it without touching existing files"
    )]
    NotEmpty(PathBuf),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

/// What `init` is doing in a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The directory is missing or empty.
    New,
    /// It has an sfdx-project.json: opk is added to it.
    Existing,
    /// Not empty, no sfdx-project.json, `--force`: scaffold next to what is there.
    Forced,
}

/// The mode for `dir`; a non-empty directory without sfdx-project.json needs `force`.
pub fn mode(dir: &Path, force: bool) -> Result<Mode> {
    if dir.join("sfdx-project.json").is_file() {
        return Ok(Mode::Existing);
    }
    let empty = match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(source) => {
            return Err(Error::Io {
                path: dir.to_owned(),
                source,
            });
        }
    };
    match (empty, force) {
        (true, _) => Ok(Mode::New),
        (false, true) => Ok(Mode::Forced),
        (false, false) => Err(Error::NotEmpty(dir.to_owned())),
    }
}

/// `.gitignore` lines opk wants in every project.
pub const GITIGNORE: &str = "# opk
.orgpunk/state/

# sf / sfdx
.sf/
.sfdx/
.localdevserver/

# secrets
*.key
*.pem
.env

# environment
node_modules/
.DS_Store
*.log
";

const FORCEIGNORE: &str = "# Files and directories opk skips when it builds packages and syncs, like sf.
# Same syntax as .gitignore. More information:
# https://developer.salesforce.com/docs/atlas.en-us.sfdx_dev.meta/sfdx_dev/sfdx_dev_exclude_source.htm
";

const CONFIG: &str = "# opk project config: the environments (the master is one of them) and their
# protect. Managed with `opk env add / set / remove`. Committed: no secrets here.
";

/// Creates what `mode` needs and is missing; returns the paths created or changed, relative
/// to `dir`. `api_version` goes into a new sfdx-project.json.
pub fn scaffold(dir: &Path, mode: Mode, api_version: &str) -> Result<Vec<String>> {
    let mut done = Vec::new();
    let name = dir.file_name().map_or_else(
        || "project".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    if mode != Mode::Existing {
        // Written by hand to keep the usual key order (packageDirectories first).
        let quoted = |s: &str| serde_json::Value::from(s).to_string();
        let project = format!(
            "{{\n  \"packageDirectories\": [\n    {{\n      \"path\": \"force-app\",\n      \"default\": true\n    }}\n  ],\n  \"name\": {},\n  \"namespace\": \"\",\n  \"sourceApiVersion\": {}\n}}\n",
            quoted(&name),
            quoted(api_version)
        );
        let scratch = format!(
            "{{\n  \"orgName\": {},\n  \"edition\": \"Developer\",\n  \"features\": [],\n  \"settings\": {{}}\n}}\n",
            quoted(&name)
        );
        create(dir, "sfdx-project.json", &project, &mut done)?;
        create(dir, "config/project-scratch-def.json", &scratch, &mut done)?;
        create(dir, ".forceignore", FORCEIGNORE, &mut done)?;
        // git keeps no empty directories; .forceignore skips dot files by default.
        if !dir.join("force-app").exists() {
            create(dir, "force-app/main/default/.gitkeep", "", &mut done)?;
        }
    }
    create(dir, ".orgpunk/config.toml", CONFIG, &mut done)?;
    if merge_gitignore(dir)? {
        done.push(".gitignore".into());
    }
    Ok(done)
}

fn create(dir: &Path, rel: &str, body: &str, done: &mut Vec<String>) -> Result<()> {
    let path = dir.join(rel);
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_owned(),
            source,
        })?;
    }
    std::fs::write(&path, body).map_err(|source| Error::Io { path, source })?;
    done.push(rel.to_owned());
    Ok(())
}

/// Adds the `.gitignore` lines opk wants and the file lacks; returns whether it changed.
fn merge_gitignore(dir: &Path) -> Result<bool> {
    let path = dir.join(".gitignore");
    let current = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => return Err(Error::Io { path, source }),
    };
    let text = match current {
        None => GITIGNORE.to_owned(),
        Some(cur) => {
            let have: Vec<&str> = cur.lines().map(str::trim).collect();
            let missing: Vec<&str> = GITIGNORE
                .lines()
                .filter(|l| !l.is_empty() && !l.starts_with('#') && !have.contains(l))
                .collect();
            if missing.is_empty() {
                return Ok(false);
            }
            let mut t = cur;
            if !t.ends_with('\n') {
                t.push('\n');
            }
            t.push_str("\n# added by opk init\n");
            for l in missing {
                t.push_str(l);
                t.push('\n');
            }
            t
        }
    };
    std::fs::write(&path, text).map_err(|source| Error::Io { path, source })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(mode(&d.path().join("missing"), false).unwrap(), Mode::New);
        assert_eq!(mode(d.path(), false).unwrap(), Mode::New);
        std::fs::write(d.path().join("notes.txt"), "x").unwrap();
        assert!(matches!(mode(d.path(), false), Err(Error::NotEmpty(_))));
        assert_eq!(mode(d.path(), true).unwrap(), Mode::Forced);
        std::fs::write(d.path().join("sfdx-project.json"), "{}").unwrap();
        assert_eq!(mode(d.path(), false).unwrap(), Mode::Existing);
    }

    #[test]
    fn scaffolds_once_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("acme");
        let made = scaffold(&dir, Mode::New, "66.0").unwrap();
        assert_eq!(
            made,
            [
                "sfdx-project.json",
                "config/project-scratch-def.json",
                ".forceignore",
                "force-app/main/default/.gitkeep",
                ".orgpunk/config.toml",
                ".gitignore",
            ]
        );
        let project = std::fs::read_to_string(dir.join("sfdx-project.json")).unwrap();
        assert!(
            project.contains("\"sourceApiVersion\": \"66.0\"")
                && project.contains("\"name\": \"acme\"")
        );
        std::fs::write(dir.join(".forceignore"), "mine\n").unwrap();
        assert!(
            scaffold(&dir, Mode::New, "67.0").unwrap().is_empty(),
            "idempotent"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".forceignore")).unwrap(),
            "mine\n"
        );
    }

    #[test]
    fn existing_projects_get_config_and_missing_ignores() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        std::fs::write(dir.join("sfdx-project.json"), "{}").unwrap();
        std::fs::write(dir.join(".gitignore"), "node_modules/\n.sf/").unwrap();
        let made = scaffold(dir, Mode::Existing, "66.0").unwrap();
        assert_eq!(made, [".orgpunk/config.toml", ".gitignore"]);
        let ignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(
            ignore.starts_with("node_modules/\n.sf/\n"),
            "user lines kept first"
        );
        assert!(ignore.contains(".orgpunk/state/") && ignore.matches(".sf/").count() == 1);
        assert!(
            !dir.join("force-app").exists(),
            "an existing project keeps its layout"
        );
        assert!(scaffold(dir, Mode::Existing, "66.0").unwrap().is_empty());
    }
}
