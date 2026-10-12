//! Components present on both sides with no sync to compare against (the first sync with an
//! org, or the same component added on both sides) are conflicts by default. Those whose files
//! are byte for byte what the org has are settled instead: recorded as synced, not conflicts.

use std::collections::BTreeMap;
use std::time::Duration;

use orgpunk_sfapi::Client;

use crate::local::{LocalMember, hash};
use crate::remote::Remote;
use crate::state::{State, Synced};
use crate::status::{Kind, Status};
use crate::{Result, key, retrieve};

/// Retrieves the conflicts nothing was synced for and settles the identical ones: they leave
/// `status.conflicts` and enter `state`. Returns the settled members.
pub fn settle(
    client: &Client,
    state: &mut State,
    local: &BTreeMap<String, LocalMember>,
    remote: &Remote,
    status: &mut Status,
    timeout: Duration,
) -> Result<Vec<String>> {
    let candidates: Vec<String> = status
        .conflicts
        .iter()
        .filter(|c| c.kind != Kind::Deleted && !state.components.contains_key(&key(&c.member)))
        // A label's local file holds all labels: it has no bytes of its own to compare.
        .filter(|c| {
            local
                .get(&key(&c.member))
                .is_some_and(|l| l.files.keys().all(|f| !f.contains('#')))
        })
        .map(|c| c.member.clone())
        .collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let got = retrieve::retrieve(client, &candidates, timeout)?;
    let tree = orgpunk_convert::to_source::convert(&got.files)?;
    // Per member: the files the org has, below `main/default`.
    let mut org_files: BTreeMap<String, Vec<(&str, String)>> = BTreeMap::new();
    for (rel, bytes) in &tree.files {
        if let Some(m) = tree.members.get(rel) {
            org_files
                .entry(key(m))
                .or_default()
                .push((rel.as_str(), hash(bytes)));
        }
    }
    let revisions: BTreeMap<String, &str> = remote
        .changes
        .iter()
        .map(|r| (key(&r.member), r.revision.as_str()))
        .collect();

    let mut settled = Vec::new();
    for member in candidates {
        let k = key(&member);
        let (Some(l), Some(theirs)) = (local.get(&k), org_files.get(&k)) else {
            continue;
        };
        if same_files(&l.files, theirs) {
            state.set(Synced {
                member: member.clone(),
                revision: revisions
                    .get(&k)
                    .map(|r| (*r).to_owned())
                    .unwrap_or_default(),
                files: l.files.clone(),
            });
            settled.push(member);
        }
    }
    status
        .conflicts
        .retain(|c| !settled.iter().any(|s| key(s) == key(&c.member)));
    Ok(settled)
}

/// The local files (project-relative path → hash) are exactly the org's (path below
/// `main/default` → hash): as many, and each org file has a local file ending in its path
/// with the same hash.
fn same_files(local: &BTreeMap<String, String>, org: &[(&str, String)]) -> bool {
    local.len() == org.len()
        && org.iter().all(|(rel, h)| {
            local
                .iter()
                .any(|(path, lh)| lh == h && (path == rel || path.ends_with(&format!("/{rel}"))))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_files_by_path_suffix_and_hash() {
        let local = BTreeMap::from([
            (
                "force-app/main/default/classes/A.cls".to_owned(),
                "h1".to_owned(),
            ),
            (
                "force-app/main/default/classes/A.cls-meta.xml".to_owned(),
                "h2".to_owned(),
            ),
        ]);
        let org = [
            ("classes/A.cls", "h1".to_owned()),
            ("classes/A.cls-meta.xml", "h2".to_owned()),
        ];
        assert!(same_files(&local, &org));
        let changed = [
            ("classes/A.cls", "hx".to_owned()),
            ("classes/A.cls-meta.xml", "h2".to_owned()),
        ];
        assert!(!same_files(&local, &changed));
        assert!(!same_files(&local, &org[..1]), "a missing file differs");
        let elsewhere = [
            ("classes/B/A.cls", "h1".to_owned()),
            ("classes/A.cls-meta.xml", "h2".to_owned()),
        ];
        assert!(!same_files(&local, &elsewhere));
    }
}
