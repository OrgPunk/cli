//! Deploy history of an org: `.orgpunk/deployments/<OrgId>.json`, committed with the project.
//!
//! Like Terraform state: the content of the metadata lives in git, the history knows which
//! commit went to the org, what each deploy created and changed, and the org's revision of
//! every component afterwards, to catch changes made around opk (drift).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use orgpunk_convert::manifest::Manifest;
use orgpunk_sfapi::{FileProperties, ListQuery};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct History {
    pub org_id: String,
    pub deploys: Vec<Entry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: u64,
    /// The commit the package was built from; `None` when the working tree was dirty or the
    /// package was a ready mdapi directory — such a deploy cannot be rolled back to.
    pub commit: Option<String>,
    pub at: String,
    pub by: String,
    pub deploy_ids: Vec<String>,
    /// `Type:Name` of components the org did not have before this deploy.
    pub created: Vec<String>,
    /// `Type:Name` of components the org had and this deploy overwrote.
    pub changed: Vec<String>,
    /// `Type:Name` → the org's `lastModifiedDate` right after the deploy.
    pub org_revision: BTreeMap<String, Revision>,
    /// Only some parts of a `--split` deploy reached the org.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// A rollback: the id of the deploy it went back to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_to: Option<u64>,
    /// `opk env add --import`: the org's versions committed as a starting point, nothing
    /// deployed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub import: bool,
    /// `opk env sync`: the org's current revisions recorded as known, nothing deployed and no
    /// commit. Changes made around opk stop being drift; it is never a rollback target.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sync: bool,
    /// A sync: members the history knew and the org no longer has; they are forgotten.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Revision {
    pub last_modified_date: String,
    pub last_modified_by_id: String,
}

/// The file of an org's history. An existing file of the same org under its other Org Id
/// form (15 or 18 characters) wins, so a config with short ids finds the history.
pub fn path(root: &Path, org_id: &str) -> PathBuf {
    let dir = root.join(".orgpunk").join("deployments");
    let exact = dir.join(format!("{org_id}.json"));
    if exact.exists() {
        return exact;
    }
    let other = std::fs::read_dir(&dir).ok().and_then(|entries| {
        entries.flatten().map(|e| e.path()).find(|p| {
            p.extension().is_some_and(|x| x == "json")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| orgpunk_config::project::same_org(s, org_id))
        })
    });
    other.unwrap_or(exact)
}

/// A history as read, with the bytes it was read from to notice a concurrent writer.
pub struct Loaded {
    pub history: History,
    path: PathBuf,
    read: Option<Vec<u8>>,
}

pub fn load(root: &Path, org_id: &str) -> Result<Loaded> {
    let p = path(root, org_id);
    let read = match std::fs::read(&p) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => return Err(Error::Io { path: p, source }),
    };
    let history = match &read {
        Some(b) => serde_json::from_slice(b).map_err(|e| Error::Io {
            path: p.clone(),
            source: std::io::Error::other(e),
        })?,
        None => History {
            org_id: org_id.to_owned(),
            deploys: Vec::new(),
        },
    };
    Ok(Loaded {
        history,
        path: p,
        read,
    })
}

impl Loaded {
    /// Appends an entry (its id is assigned) unless someone else wrote the file since it
    /// was read: two deploys into one org at once would lose one of them.
    pub fn append(mut self, mut entry: Entry) -> Result<Entry> {
        let now = match std::fs::read(&self.path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(Error::Io {
                    path: self.path,
                    source,
                });
            }
        };
        if now != self.read {
            return Err(Error::HistoryChanged(self.path));
        }
        entry.id = self.history.deploys.last().map_or(1, |e| e.id + 1);
        self.history.deploys.push(entry.clone());
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.to_owned(),
                source,
            })?;
        }
        let text = serde_json::to_string_pretty(&self.history).expect("serializes") + "\n";
        std::fs::write(&self.path, text).map_err(|source| Error::Io {
            path: self.path,
            source,
        })?;
        Ok(entry)
    }
}

impl History {
    /// The newest recorded revision of each component.
    pub fn latest_revisions(&self) -> BTreeMap<String, &Revision> {
        let mut out = BTreeMap::new();
        for e in &self.deploys {
            for (k, r) in &e.org_revision {
                out.insert(key(k), r);
            }
            for m in &e.removed {
                out.remove(&key(m));
            }
        }
        out
    }
}

/// Case-insensitive identity of a `Type:Name`, as the org treats names.
fn key(member: &str) -> String {
    member.to_lowercase()
}

/// `Type:Name` members of a package.xml, without wildcards.
pub fn members(manifest: &Manifest) -> Vec<String> {
    manifest
        .types
        .iter()
        .flat_map(|(t, ms)| {
            ms.iter()
                .filter(|m| *m != "*")
                .map(move |m| format!("{t}:{m}"))
        })
        .collect()
}

/// In-folder types and the type of their folders.
pub const FOLDER_TYPES: &[(&str, &str)] = &[
    ("Report", "ReportFolder"),
    ("Dashboard", "DashboardFolder"),
    ("Document", "DocumentFolder"),
    ("EmailTemplate", "EmailFolder"),
];

/// listMetadata queries that cover the members: one per type, one per folder for in-folder
/// types (folders themselves under their folder type).
pub fn queries(members: &[String]) -> Vec<ListQuery> {
    let mut out = BTreeSet::new();
    for m in members {
        let Some((kind, name)) = m.split_once(':') else {
            continue;
        };
        match FOLDER_TYPES.iter().find(|(t, _)| *t == kind) {
            Some((_, folder_type)) => match name.trim_end_matches('/').rsplit_once('/') {
                Some((folder, _)) => {
                    out.insert(ListQuery {
                        kind: kind.to_owned(),
                        folder: Some(folder.to_owned()),
                    });
                }
                // A folder: `Report:Sales/` or `Document:Sales`.
                None => {
                    out.insert(ListQuery {
                        kind: (*folder_type).to_owned(),
                        folder: None,
                    });
                }
            },
            None => {
                out.insert(ListQuery {
                    kind: kind.to_owned(),
                    folder: None,
                });
            }
        }
    }
    out.into_iter().collect()
}

/// The org's components by `key(Type:Name)`; folders are listed under the content type, as
/// package.xml names them.
pub fn index(props: &[FileProperties]) -> BTreeMap<String, &FileProperties> {
    props
        .iter()
        .map(|p| {
            let kind = FOLDER_TYPES
                .iter()
                .find(|(_, f)| *f == p.kind)
                .map_or(p.kind.as_str(), |(t, _)| t);
            (key(&format!("{kind}:{}", p.full_name)), p)
        })
        .collect()
}

/// Components the org changed after opk last deployed them: the newest recorded revision
/// differs from the org's now. Components opk never recorded are not drift.
pub fn drift(history: &History, members: &[String], now: &[FileProperties]) -> Vec<String> {
    let latest = history.latest_revisions();
    let now = index(now);
    members
        .iter()
        .filter(|m| {
            let k = key(m);
            match (latest.get(&k), now.get(&k)) {
                (Some(r), Some(p)) => {
                    r.last_modified_date != p.last_modified_date
                        || r.last_modified_by_id != p.last_modified_by_id
                }
                // Deleted in the org since.
                (Some(_), None) => true,
                (None, _) => false,
            }
        })
        .cloned()
        .collect()
}

/// What a deploy did, from listings before and after: created are members the org lacked
/// before and has after, changed are members it had before. A member the org does not list
/// at all (standard fields, some settings) is neither, so a rollback never deletes it. With
/// `touched_only` (a split deploy that stopped half way) a member counts only if its
/// `lastModifiedDate` moved.
pub fn outcome(
    members: &[String],
    before: &[FileProperties],
    after: &[FileProperties],
    touched_only: bool,
) -> (Vec<String>, Vec<String>, BTreeMap<String, Revision>) {
    let (before, after) = (index(before), index(after));
    let (mut created, mut changed, mut revision) = (Vec::new(), Vec::new(), BTreeMap::new());
    for m in members {
        let k = key(m);
        let Some(now) = after.get(&k) else {
            continue;
        };
        match before.get(&k) {
            Some(was) if touched_only && was.last_modified_date == now.last_modified_date => {
                continue;
            }
            Some(_) => changed.push(m.clone()),
            None => created.push(m.clone()),
        }
        revision.insert(
            m.clone(),
            Revision {
                last_modified_date: now.last_modified_date.clone(),
                last_modified_by_id: now.last_modified_by_id.clone(),
            },
        );
    }
    (created, changed, revision)
}

/// What going back to an earlier deploy takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackPlan {
    /// The deploy to go back to.
    pub target: u64,
    pub commit: String,
    /// Changed after the target: deployed again as of its commit.
    pub restore: Vec<String>,
    /// Created after the target: deleted.
    pub delete: Vec<String>,
}

/// The plan to go back to deploy `to`, by default the one before the last.
pub fn rollback_plan(
    history: &History,
    to: Option<u64>,
) -> std::result::Result<RollbackPlan, String> {
    let deploys = &history.deploys;
    // Syncs only record the org: "the one before the last" counts deploys.
    let real: Vec<usize> = (0..deploys.len()).filter(|&i| !deploys[i].sync).collect();
    let at = match to {
        Some(id) => {
            let at = deploys
                .iter()
                .position(|e| e.id == id)
                .ok_or_else(|| format!("no deploy #{id} in the history of {}", history.org_id))?;
            if deploys[at].sync {
                return Err(format!(
                    "#{id} is a sync: it recorded the org, there is nothing to go back to; pick \
                     a deploy"
                ));
            }
            at
        }
        None if real.len() >= 2 => real[real.len() - 2],
        None => {
            return Err(format!(
                "the history of {} has {} deploy(s), a rollback needs an earlier one to go back \
                 to; from a clean working tree, `opk env add <env> <alias> --import` records a \
                 starting point",
                history.org_id,
                real.len()
            ));
        }
    };
    let target = &deploys[at];
    let commit = target.commit.clone().ok_or_else(|| {
        format!(
            "deploy #{} has no commit to go back to: it was made with uncommitted changes (or \
             from --metadata-dir), and only deploys from a clean working tree record theirs; \
             pick another deploy id, or record a starting point with \
             `opk env add <env> <alias> --import`",
            target.id
        )
    })?;
    let mut created = BTreeMap::new();
    let mut touched = BTreeMap::new();
    for e in &deploys[at + 1..] {
        for m in &e.created {
            created.entry(key(m)).or_insert_with(|| m.clone());
        }
        for m in e.created.iter().chain(&e.changed) {
            touched.entry(key(m)).or_insert_with(|| m.clone());
        }
    }
    let restore: Vec<String> = touched
        .iter()
        .filter(|(k, _)| !created.contains_key(*k))
        .map(|(_, m)| m.clone())
        .collect();
    let delete: Vec<String> = created.into_values().collect();
    if restore.is_empty() && delete.is_empty() {
        return Err(format!(
            "nothing was deployed to {} after #{}",
            history.org_id, target.id
        ));
    }
    Ok(RollbackPlan {
        target: target.id,
        commit,
        restore,
        delete,
    })
}

/// The newest deploy another environment can take (`env deploy --from`): complete and built
/// from a commit.
pub fn promotable(history: &History) -> Option<&Entry> {
    history
        .deploys
        .iter()
        .rev()
        .find(|e| e.commit.is_some() && !e.partial && !e.sync)
}

/// The history side of one deploy into a tracked org: what the org had before, then the
/// entry once the deploy is in.
pub struct Tracker {
    loaded: Loaded,
    members: Vec<String>,
    before: Vec<FileProperties>,
    commit: Option<String>,
    by: String,
}

impl Tracker {
    /// Lists the members in the org. Returns the tracker and the members changed in the org
    /// since opk last deployed them (drift).
    pub fn start(
        client: &orgpunk_sfapi::Client,
        root: &Path,
        org_id: &str,
        by: &str,
        members: Vec<String>,
        commit: Option<String>,
    ) -> Result<(Self, Vec<String>)> {
        let loaded = load(root, org_id)?;
        let before = if members.is_empty() {
            Vec::new()
        } else {
            client.list_metadata(&queries(&members))?
        };
        let drifted = drift(&loaded.history, &members, &before);
        Ok((
            Self {
                loaded,
                members,
                before,
                commit,
                by: by.to_owned(),
            },
            drifted,
        ))
    }

    /// Lists the members again and records the deploy.
    pub fn finish(
        self,
        client: &orgpunk_sfapi::Client,
        deploy_ids: Vec<String>,
        partial: bool,
        rollback_to: Option<u64>,
    ) -> Result<Entry> {
        let after = if self.members.is_empty() {
            Vec::new()
        } else {
            client.list_metadata(&queries(&self.members))?
        };
        let (created, changed, org_revision) =
            outcome(&self.members, &self.before, &after, partial);
        self.loaded.append(Entry {
            id: 0,
            commit: self.commit,
            at: crate::validations::now(),
            by: self.by,
            deploy_ids,
            created,
            changed,
            org_revision,
            partial,
            rollback_to,
            ..Entry::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(kind: &str, name: &str, date: &str) -> FileProperties {
        FileProperties {
            kind: kind.into(),
            full_name: name.into(),
            last_modified_date: date.into(),
            last_modified_by_id: "005A".into(),
            ..FileProperties::default()
        }
    }

    #[test]
    fn queries_cover_folders_and_types() {
        let q = queries(&[
            "ApexClass:A".into(),
            "ApexClass:B".into(),
            "Report:Sales/Q1".into(),
            "Report:Sales/".into(),
            "CustomField:Account.F__c".into(),
        ]);
        let shown: Vec<String> = q
            .iter()
            .map(|q| {
                format!(
                    "{}{}",
                    q.kind,
                    q.folder
                        .as_deref()
                        .map(|f| format!("@{f}"))
                        .unwrap_or_default()
                )
            })
            .collect();
        assert_eq!(
            shown,
            ["ApexClass", "CustomField", "Report@Sales", "ReportFolder"]
        );
    }

    #[test]
    fn outcome_and_drift() {
        let members: Vec<String> = ["ApexClass:Old", "ApexClass:New", "CustomField:Account.Name"]
            .map(String::from)
            .to_vec();
        let before = [prop("ApexClass", "old", "d1")];
        let after = [
            prop("ApexClass", "Old", "d2"),
            prop("ApexClass", "New", "d2"),
        ];
        let (created, changed, rev) = outcome(&members, &before, &after, false);
        let (_, touched, _) = outcome(&members, &[prop("ApexClass", "Old", "d2")], &after, true);
        assert!(touched.is_empty(), "not modified by a partial deploy");
        assert_eq!(created, ["ApexClass:New"]);
        assert_eq!(changed, ["ApexClass:Old"]);
        assert!(
            !rev.contains_key("CustomField:Account.Name"),
            "unlisted is not tracked"
        );

        let h = History {
            org_id: "00D".into(),
            deploys: vec![Entry {
                org_revision: rev,
                ..Entry::default()
            }],
        };
        assert!(drift(&h, &members, &after).is_empty());
        let edited = [
            prop("ApexClass", "Old", "d3"),
            prop("ApexClass", "New", "d2"),
        ];
        assert_eq!(drift(&h, &members, &edited), ["ApexClass:Old"]);
        let deleted = [prop("ApexClass", "Old", "d2")];
        assert_eq!(drift(&h, &members, &deleted), ["ApexClass:New"]);
    }

    #[test]
    fn plans_rollbacks() {
        let entry = |id: u64, commit: Option<&str>, created: &[&str], changed: &[&str]| Entry {
            id,
            commit: commit.map(String::from),
            created: created.iter().map(|s| (*s).to_owned()).collect(),
            changed: changed.iter().map(|s| (*s).to_owned()).collect(),
            ..Entry::default()
        };
        let h = History {
            org_id: "00D".into(),
            deploys: vec![
                entry(1, Some("c1"), &["ApexClass:A"], &[]),
                entry(2, Some("c2"), &["ApexClass:B"], &["ApexClass:A"]),
                entry(3, None, &["ApexClass:C"], &["ApexClass:b"]),
            ],
        };
        let p = rollback_plan(&h, None).unwrap();
        assert_eq!((p.target, p.commit.as_str()), (2, "c2"));
        assert_eq!(
            (p.restore, p.delete),
            (
                vec!["ApexClass:b".to_owned()],
                vec!["ApexClass:C".to_owned()]
            )
        );
        let e = rollback_plan(&h, Some(3)).unwrap_err();
        assert!(e.contains("#3 has no commit"), "{e}");
        let p = rollback_plan(&h, Some(1)).unwrap();
        assert_eq!((p.target, p.commit.as_str()), (1, "c1"));
        assert_eq!(p.restore, ["ApexClass:A"]);
        assert_eq!(
            p.delete,
            ["ApexClass:B", "ApexClass:C"],
            "b is the same component as B"
        );
        assert!(rollback_plan(&h, Some(9)).is_err());
        assert_eq!(promotable(&h).map(|e| e.id), Some(2));
    }

    #[test]
    fn syncs_record_the_org_and_are_not_deploys() {
        let rev = |d: &str| Revision {
            last_modified_date: d.into(),
            last_modified_by_id: "005A".into(),
        };
        let deploy = |id: u64, commit: &str, changed: &[&str]| Entry {
            id,
            commit: Some(commit.into()),
            changed: changed.iter().map(|s| (*s).to_owned()).collect(),
            org_revision: changed
                .iter()
                .map(|s| ((*s).to_owned(), rev("d1")))
                .collect(),
            ..Entry::default()
        };
        let h = History {
            org_id: "00D".into(),
            deploys: vec![
                deploy(1, "c1", &["ApexClass:A", "ApexClass:Gone"]),
                deploy(2, "c2", &["ApexClass:A"]),
                Entry {
                    id: 3,
                    sync: true,
                    org_revision: BTreeMap::from([("ApexClass:A".into(), rev("d9"))]),
                    removed: vec!["ApexClass:Gone".into()],
                    ..Entry::default()
                },
            ],
        };
        let latest = h.latest_revisions();
        assert_eq!(latest["apexclass:a"].last_modified_date, "d9");
        assert!(!latest.contains_key("apexclass:gone"), "a sync forgets");
        let now = [prop("ApexClass", "A", "d9")];
        assert!(drift(&h, &["ApexClass:A".into(), "ApexClass:Gone".into()], &now).is_empty());

        assert_eq!(
            rollback_plan(&h, None).unwrap().target,
            1,
            "a sync is no deploy"
        );
        assert!(
            rollback_plan(&h, Some(3))
                .unwrap_err()
                .contains("is a sync")
        );
        assert_eq!(promotable(&h).map(|e| e.id), Some(2));
    }

    #[test]
    fn appends_and_refuses_concurrent_writes() {
        let d = tempfile::tempdir().unwrap();
        let org = "00D000000000001AAA";
        let e = load(d.path(), org)
            .unwrap()
            .append(Entry::default())
            .unwrap();
        assert_eq!(e.id, 1);
        let second = load(d.path(), org).unwrap();
        let racing = load(d.path(), org).unwrap();
        assert_eq!(second.append(Entry::default()).unwrap().id, 2);
        assert!(matches!(
            racing.append(Entry::default()),
            Err(Error::HistoryChanged(_))
        ));
        let h = load(d.path(), org).unwrap().history;
        assert_eq!(h.org_id, org);
        assert_eq!(h.deploys.len(), 2);
        let short = load(d.path(), "00D000000000001").unwrap().history;
        assert_eq!(
            short.deploys.len(),
            2,
            "the 15-character id finds the same file"
        );
    }
}
