//! `.orgpunk/state/<OrgId>.json`: what the org and the working copy looked like at the last
//! pull or push. Local to the machine, never committed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, key};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub org_id: String,
    /// Whether the org has source tracking (SourceMember); learned on the first fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking: Option<bool>,
    /// Highest SourceMember RevisionCounter seen.
    #[serde(default)]
    pub max_revision: i64,
    /// By `key(Type:Name)`.
    #[serde(default)]
    pub components: BTreeMap<String, Synced>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Synced {
    /// `Type:Name` as the org names it.
    pub member: String,
    /// RevisionCounter (tracking orgs) or lastModifiedDate.
    pub revision: String,
    /// Project-relative path (with `/`) → sha256 of the local content then.
    pub files: BTreeMap<String, String>,
}

pub fn path(root: &Path, org_id: &str) -> PathBuf {
    root.join(".orgpunk")
        .join("state")
        .join(format!("{org_id}.json"))
}

impl State {
    /// The state of an org; empty before the first sync.
    pub fn load(root: &Path, org_id: &str) -> Result<Self> {
        let p = path(root, org_id);
        match std::fs::read(&p) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| Error::State {
                path: p,
                reason: e.to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                org_id: org_id.to_owned(),
                ..Self::default()
            }),
            Err(source) => Err(Error::Io { path: p, source }),
        }
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let p = path(root, &self.org_id);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.to_owned(),
                source,
            })?;
        }
        let text = serde_json::to_string_pretty(self).expect("serializes") + "\n";
        std::fs::write(&p, text).map_err(|source| Error::Io { path: p, source })
    }

    pub fn get(&self, member: &str) -> Option<&Synced> {
        self.components.get(&key(member))
    }

    pub fn set(&mut self, synced: Synced) {
        self.components.insert(key(&synced.member), synced);
    }

    pub fn remove(&mut self, member: &str) {
        self.components.remove(&key(member));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_starts_empty() {
        let d = tempfile::tempdir().unwrap();
        let mut s = State::load(d.path(), "00D1").unwrap();
        assert!(s.components.is_empty() && s.tracking.is_none());
        s.tracking = Some(true);
        s.set(Synced {
            member: "ApexClass:A".into(),
            revision: "3".into(),
            files: BTreeMap::from([("fa/classes/A.cls".into(), "h".into())]),
        });
        s.save(d.path()).unwrap();
        let back = State::load(d.path(), "00D1").unwrap();
        assert_eq!(back, s);
        assert_eq!(back.get("apexclass:a").unwrap().revision, "3");
    }
}
