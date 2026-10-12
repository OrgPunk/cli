//! Successful validations (checkOnly deploys) of a project, so a quick deploy to an org with
//! `validate-first` can be tied to the commit that was validated.
//!
//! `.orgpunk/state/validations.json`, local and not committed: a validation id is only good in
//! its org for a few days.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The newest validations kept.
const KEEP: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Validation {
    pub id: String,
    pub org_id: String,
    /// HEAD of the working copy; `None` outside git or with uncommitted changes.
    pub commit: Option<String>,
    /// RFC 3339.
    pub at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    validations: Vec<Validation>,
}

fn path(root: &Path) -> PathBuf {
    root.join(".orgpunk").join("state").join("validations.json")
}

fn load(root: &Path) -> Result<File> {
    let p = path(root);
    match std::fs::read_to_string(&p) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| Error::Io {
            path: p,
            source: std::io::Error::other(e),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
        Err(source) => Err(Error::Io { path: p, source }),
    }
}

/// Remembers a successful validation.
pub fn record(root: &Path, v: Validation) -> Result<()> {
    let mut f = load(root)?;
    f.validations.retain(|old| old.id != v.id);
    f.validations.push(v);
    let excess = f.validations.len().saturating_sub(KEEP);
    f.validations.drain(..excess);
    let p = path(root);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;
    }
    let text = serde_json::to_string_pretty(&f).expect("serializes");
    std::fs::write(&p, text).map_err(|source| Error::Io { path: p, source })
}

/// The current UTC time as RFC 3339, to the second.
pub fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    rfc3339(secs)
}

fn rfc3339(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// A recorded validation by id.
pub fn find(root: &Path, id: &str) -> Result<Option<Validation>> {
    Ok(load(root)?.validations.into_iter().find(|v| v.id == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_time() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_791_720_000), "2026-10-11T12:00:00Z");
    }

    #[test]
    fn records_and_finds() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(find(d.path(), "0Af1").unwrap(), None);
        let v = |id: &str| Validation {
            id: id.into(),
            org_id: "00D1".into(),
            commit: Some("abc".into()),
            at: "2026-10-11T12:00:00Z".into(),
        };
        for i in 0..KEEP + 3 {
            record(d.path(), v(&format!("0Af{i}"))).unwrap();
        }
        assert_eq!(find(d.path(), "0Af0").unwrap(), None, "oldest dropped");
        assert_eq!(
            find(d.path(), &format!("0Af{}", KEEP + 2)).unwrap(),
            Some(v(&format!("0Af{}", KEEP + 2)))
        );
    }
}
