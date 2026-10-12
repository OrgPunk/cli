//! The deploy scenario (`opk env deploy`, push, rollback): packaging, pre-flight checks,
//! deploy and waiting for the result.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use orgpunk_convert::mdapi::{DESTRUCTIVE_FILES, SKIP_FILES};
use orgpunk_sfapi::{Client, DeployOptions, DeployResult};
use zip::write::SimpleFileOptions;

pub mod history;
pub mod source;
pub mod split;
pub mod validations;

/// Limits of the Metadata API file-based deploy.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_files: usize,
    pub max_zip_bytes: u64,
    pub max_unzipped_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 10_000,
            max_zip_bytes: 39 * 1024 * 1024,
            max_unzipped_bytes: 600 * 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}: no package.xml at the root, not an mdapi directory")]
    NoPackageXml(PathBuf),
    #[error("package exceeds a Salesforce limit: {what} {actual}, max {max}")]
    Limit {
        what: &'static str,
        actual: String,
        max: String,
    },
    #[error(
        "{components} components reference each other and must deploy together, \
         but exceed the limits alone ({size}): {members}"
    )]
    TooBigToSplit {
        components: usize,
        size: String,
        members: String,
    },
    #[error("package contains {0}: deleting components requires --allow-destructive")]
    Destructive(String),
    #[error(
        "user {username} cannot deploy metadata: needs Modify All Data \
         or Modify Metadata Through Metadata API Functions"
    )]
    Permissions { username: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("building zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Api(#[from] orgpunk_sfapi::Error),
    #[error(transparent)]
    Mdapi(#[from] orgpunk_convert::Error),
    #[error("sfdx-project.json uses features opk does not support yet: {0}")]
    UnsupportedProject(String),
    #[error("no metadata components under {0}")]
    NothingToDeploy(String),
    #[error(
        "{0}: another deploy wrote the history meanwhile; deploy to one org at a time (in CI: a concurrency group per org)"
    )]
    HistoryChanged(PathBuf),
    #[error(transparent)]
    Repo(#[from] orgpunk_repo::Error),
    #[error(transparent)]
    Config(#[from] orgpunk_config::Error),
}

impl Error {
    /// A refusal by policy (not a failure): exit 2.
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Destructive(_))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A built package.
#[derive(Debug)]
pub struct Package {
    pub zip: Vec<u8>,
    pub files: usize,
    pub unzipped_bytes: u64,
    /// destructiveChanges*.xml files found.
    pub destructive: Vec<String>,
}

/// Zips an mdapi directory in memory. Limits are checked before and after compression,
/// deletions are allowed only with `allow_destructive`.
pub fn package_mdapi_dir(dir: &Path, limits: Limits, allow_destructive: bool) -> Result<Package> {
    if !dir.join("package.xml").is_file() {
        return Err(Error::NoPackageXml(dir.to_owned()));
    }
    let mut entries = Vec::new();
    let mut unzipped = 0u64;
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
        let name = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let len = entry
            .metadata()
            .map_err(|e| Error::Io {
                path: entry.path().to_owned(),
                source: e.into(),
            })?
            .len();
        unzipped += len;
        entries.push((name, entry.into_path()));
    }

    check(
        "file count",
        entries.len() as u64,
        limits.max_files as u64,
        |n| n.to_string(),
    )?;
    check("unzipped size", unzipped, limits.max_unzipped_bytes, mib)?;

    let destructive: Vec<String> = entries
        .iter()
        .filter(|(n, _)| DESTRUCTIVE_FILES.contains(&n.as_str()))
        .map(|(n, _)| n.clone())
        .collect();
    if !destructive.is_empty() && !allow_destructive {
        return Err(Error::Destructive(destructive.join(", ")));
    }

    let entries: Vec<(&str, Content)> = entries
        .iter()
        .map(|(name, path)| (name.as_str(), Content::File(path)))
        .collect();
    let (zip, _) = zip_entries(&entries)?;
    check("zip size", zip.len() as u64, limits.max_zip_bytes, mib)?;

    Ok(Package {
        zip,
        files: entries.len(),
        unzipped_bytes: unzipped,
        destructive,
    })
}

pub(crate) fn check(
    what: &'static str,
    actual: u64,
    max: u64,
    fmt: fn(u64) -> String,
) -> Result<()> {
    if actual > max {
        return Err(Error::Limit {
            what,
            actual: fmt(actual),
            max: fmt(max),
        });
    }
    Ok(())
}

pub(crate) fn mib(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// What goes into a zip entry.
pub(crate) enum Content<'a> {
    File(&'a Path),
    Bytes(&'a [u8]),
}

/// Builds a zip in memory; returns it with the total uncompressed size.
pub(crate) fn zip_entries(entries: &[(&str, Content)]) -> Result<(Vec<u8>, u64)> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut unzipped = 0u64;
    for (name, content) in entries {
        let read;
        let (data, path): (&[u8], &Path) = match content {
            Content::File(path) => {
                read = std::fs::read(path).map_err(|source| Error::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
                (&read, path)
            }
            Content::Bytes(b) => (b, Path::new(name)),
        };
        unzipped += data.len() as u64;
        zip.start_file(*name, opts)?;
        zip.write_all(data).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok((zip.finish()?.into_inner(), unzipped))
}

/// Whether the user may deploy metadata (via profile or permission set).
pub fn check_permissions(client: &Client, user_id: &str, username: &str) -> Result<()> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Perms {
        permissions_modify_all_data: bool,
        permissions_modify_metadata: bool,
    }
    let soql = format!(
        "SELECT PermissionsModifyAllData, PermissionsModifyMetadata FROM PermissionSet \
         WHERE Id IN (SELECT PermissionSetId FROM PermissionSetAssignment WHERE AssigneeId = '{}')",
        soql_escape(user_id)
    );
    let perms: Vec<Perms> = client.query(&soql)?;
    if perms
        .iter()
        .any(|p| p.permissions_modify_all_data || p.permissions_modify_metadata)
    {
        Ok(())
    } else {
        Err(Error::Permissions {
            username: username.to_owned(),
        })
    }
}

fn soql_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// What to send.
pub enum Request {
    Zip {
        zip: Vec<u8>,
        options: DeployOptions,
    },
    Quick {
        validated_id: String,
    },
}

pub fn start(client: &Client, request: Request) -> Result<String> {
    Ok(match request {
        Request::Zip { zip, options } => client.deploy(zip, &options)?,
        Request::Quick { validated_id } => client.quick_deploy(&validated_id)?,
    })
}

pub enum Outcome {
    /// The deploy finished (successfully or not); the result includes details.
    Done(DeployResult),
    /// The wait timed out; the deploy is still running.
    TimedOut(DeployResult),
}

/// Polls the status with a 1 s → 10 s backoff until the deploy finishes or `timeout` passes.
/// The final status is fetched with details (component and test failures).
pub fn wait(
    client: &Client,
    id: &str,
    timeout: Duration,
    mut on_progress: impl FnMut(&DeployResult),
) -> Result<Outcome> {
    let started = Instant::now();
    let mut delay = Duration::from_secs(1);
    loop {
        let status = client.deploy_status(id, false)?;
        on_progress(&status);
        if status.done {
            return Ok(Outcome::Done(client.deploy_status(id, true)?));
        }
        let left = timeout.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Ok(Outcome::TimedOut(status));
        }
        std::thread::sleep(delay.min(left));
        delay = (delay * 3 / 2).min(Duration::from_secs(10));
    }
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

    fn names(zip: &[u8]) -> Vec<String> {
        let mut a = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
        (0..a.len())
            .map(|i| a.by_index(i).unwrap().name().to_owned())
            .collect()
    }

    #[test]
    fn packages_with_forward_slashes_and_skips_os_junk() {
        let d = mdapi(&[
            ("package.xml", "<Package/>"),
            ("classes/A.cls", "class A {}"),
            ("classes/A.cls-meta.xml", "<meta/>"),
            ("classes/.DS_Store", "junk"),
        ]);
        let p = package_mdapi_dir(d.path(), Limits::default(), false).unwrap();
        assert_eq!(p.files, 3);
        assert_eq!(
            names(&p.zip),
            ["classes/A.cls", "classes/A.cls-meta.xml", "package.xml"]
        );
    }

    #[test]
    fn requires_package_xml() {
        let d = mdapi(&[("classes/A.cls", "x")]);
        assert!(matches!(
            package_mdapi_dir(d.path(), Limits::default(), false),
            Err(Error::NoPackageXml(_))
        ));
    }

    #[test]
    fn destructive_needs_flag() {
        let d = mdapi(&[
            ("package.xml", "<Package/>"),
            ("destructiveChanges.xml", "<Package/>"),
        ]);
        let err = package_mdapi_dir(d.path(), Limits::default(), false).unwrap_err();
        assert!(err.is_refusal());
        let p = package_mdapi_dir(d.path(), Limits::default(), true).unwrap();
        assert_eq!(p.destructive, ["destructiveChanges.xml"]);
    }

    #[test]
    fn generated_deletions_need_the_flag_too() {
        use orgpunk_convert::destructive::{Deletions, Phase};
        use orgpunk_convert::manifest::Manifest;

        let d = mdapi(&[("package.xml", "<Package/>")]);
        let mut del = Deletions::default();
        del.add(Phase::Pre, "ApexClass", "Old").unwrap();
        del.add(Phase::Post, "CustomField", "Account.Gone__c")
            .unwrap();
        del.write(d.path(), &Manifest::default()).unwrap();
        assert!(
            package_mdapi_dir(d.path(), Limits::default(), false)
                .unwrap_err()
                .is_refusal()
        );
        let p = package_mdapi_dir(d.path(), Limits::default(), true).unwrap();
        assert_eq!(
            p.destructive,
            ["destructiveChangesPost.xml", "destructiveChangesPre.xml"]
        );
    }

    #[test]
    fn enforces_limits_before_sending() {
        let d = mdapi(&[
            ("package.xml", "<Package/>"),
            ("classes/A.cls", "0123456789"),
        ]);
        let files = Limits {
            max_files: 1,
            ..Limits::default()
        };
        assert!(matches!(
            package_mdapi_dir(d.path(), files, false),
            Err(Error::Limit {
                what: "file count",
                ..
            })
        ));
        let size = Limits {
            max_unzipped_bytes: 5,
            ..Limits::default()
        };
        assert!(matches!(
            package_mdapi_dir(d.path(), size, false),
            Err(Error::Limit {
                what: "unzipped size",
                ..
            })
        ));
        let zip = Limits {
            max_zip_bytes: 10,
            ..Limits::default()
        };
        assert!(matches!(
            package_mdapi_dir(d.path(), zip, false),
            Err(Error::Limit {
                what: "zip size",
                ..
            })
        ));
    }

    #[test]
    fn escapes_soql() {
        assert_eq!(soql_escape("a'b\\c"), "a\\'b\\\\c");
    }
}
