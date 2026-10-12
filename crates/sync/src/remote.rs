//! What changed in the org since the last sync: SourceMember in orgs with source tracking
//! (scratch orgs, Developer sandboxes), else listMetadata and lastModifiedDate.

use std::collections::{BTreeMap, BTreeSet};

use orgpunk_convert::registry;
use orgpunk_deploy::history;
use orgpunk_sfapi::{Client, FileProperties, ListQuery};
use serde::Deserialize;

use crate::local::LocalMember;
use crate::state::State;
use crate::{Result, key};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteChange {
    /// `Type:Name`.
    pub member: String,
    /// RevisionCounter or lastModifiedDate now.
    pub revision: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Remote {
    pub tracking: bool,
    pub changes: Vec<RemoteChange>,
    /// Highest RevisionCounter seen (tracking orgs).
    pub max_revision: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SourceMember {
    member_type: String,
    member_name: String,
    revision_counter: i64,
    is_name_obsolete: bool,
}

/// Whether the org has source tracking: SourceMember exists there.
pub fn has_tracking(client: &Client) -> Result<bool> {
    match client.tooling_query::<serde_json::Value>("SELECT Id FROM SourceMember LIMIT 1") {
        Ok(_) => Ok(true),
        Err(orgpunk_sfapi::Error::Api { code, .. }) if code.contains("INVALID_TYPE") => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Changes in the org since `state`.
pub fn changes(
    client: &Client,
    state: &State,
    local: &BTreeMap<String, LocalMember>,
) -> Result<Remote> {
    let tracking = match state.tracking {
        Some(t) => t,
        None => has_tracking(client)?,
    };
    if tracking {
        let rows: Vec<SourceMember> = client.tooling_query(&format!(
            "SELECT MemberType, MemberName, RevisionCounter, IsNameObsolete FROM SourceMember \
             WHERE RevisionCounter > {}",
            state.max_revision
        ))?;
        return Ok(from_source_members(state, rows));
    }
    // Without tracking: list the types the project and the state know, or every type the
    // org has when there is nothing yet (the first pull into an empty project).
    let queries = if whole_org(state, local) {
        whole_org_queries(client)?
    } else {
        let members: Vec<String> = local
            .values()
            .map(|l| l.member.clone())
            .chain(state.components.values().map(|s| s.member.clone()))
            .collect();
        history::queries(&members)
    };
    let listed = list_tolerant(client, &queries)?;
    let types: BTreeSet<String> = queries.iter().map(|q| q.kind.clone()).collect();
    Ok(Remote {
        tracking: false,
        changes: diff_listing(state, local, &listed, &types),
        max_revision: state.max_revision,
    })
}

/// Nothing in the project and no sync with the org yet: the org decides what there is.
pub fn whole_org(state: &State, local: &BTreeMap<String, LocalMember>) -> bool {
    local.is_empty() && state.components.is_empty()
}

/// listMetadata queries for every type of the org opk can convert: top-level types, the
/// children of decomposed types (fields, list views…) and custom labels, and in-folder types
/// per folder.
fn whole_org_queries(client: &Client) -> Result<Vec<ListQuery>> {
    let reg = registry::get();
    let convertible = |name: &str| {
        reg.by_name(name).is_some_and(|t| {
            let top = reg.parent(name).unwrap_or(t);
            top.adapter() != "uiBundles"
        })
    };
    let child_alone = |name: &str| {
        name == "CustomLabel"
            || reg
                .parent(name)
                .is_some_and(|p| p.adapter() == "decomposed")
    };
    let mut plain = BTreeSet::new();
    let mut in_folder = Vec::new();
    for o in client.describe_metadata()? {
        if !convertible(&o.xml_name) {
            continue;
        }
        if o.in_folder {
            in_folder.push(o.xml_name.clone());
        } else {
            plain.insert(o.xml_name.clone());
        }
        for c in o.child_xml_names {
            if convertible(&c) && child_alone(&c) {
                plain.insert(c);
            }
        }
    }
    let mut queries: Vec<ListQuery> = plain
        .into_iter()
        .map(|kind| ListQuery { kind, folder: None })
        .collect();
    // In-folder types: their folders first, then each folder's content.
    let folder_queries: Vec<ListQuery> = in_folder
        .iter()
        .filter_map(|kind| {
            history::FOLDER_TYPES
                .iter()
                .find(|(t, _)| t == kind)
                .map(|(_, f)| ListQuery {
                    kind: (*f).to_owned(),
                    folder: None,
                })
        })
        .collect();
    let folders = list_tolerant(client, &folder_queries)?;
    for kind in &in_folder {
        let Some((_, folder_type)) = history::FOLDER_TYPES.iter().find(|(t, _)| t == kind) else {
            continue;
        };
        let mut names: BTreeSet<String> = folders
            .iter()
            .filter(|f| f.kind == *folder_type)
            .map(|f| f.full_name.clone())
            .collect();
        if matches!(kind.as_str(), "Report" | "EmailTemplate") {
            names.insert("unfiled$public".into());
        }
        queries.extend(names.into_iter().map(|f| ListQuery {
            kind: kind.clone(),
            folder: Some(f),
        }));
    }
    queries.extend(folder_queries);
    Ok(queries)
}

/// listMetadata that survives types the org rejects: a failing call is retried one query at
/// a time and the rejected queries are left out.
fn list_tolerant(client: &Client, queries: &[ListQuery]) -> Result<Vec<FileProperties>> {
    // Calls take three queries each; a handful of threads keeps a whole-org listing short.
    const THREADS: usize = 8;
    let chunks: Vec<&[ListQuery]> = queries.chunks(3).collect();
    if chunks.len() <= 1 {
        return list_chunks(client, &chunks);
    }
    let per_thread = chunks.len().div_ceil(THREADS);
    let results: Vec<Result<Vec<FileProperties>>> = std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .chunks(per_thread)
            .map(|part| {
                let c = client.fork();
                s.spawn(move || list_chunks(&c, part))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("listing thread"))
            .collect()
    });
    let mut out = Vec::new();
    for r in results {
        out.extend(r?);
    }
    Ok(out)
}

fn list_chunks(client: &Client, chunks: &[&[ListQuery]]) -> Result<Vec<FileProperties>> {
    let mut out = Vec::new();
    for chunk in chunks {
        match client.list_metadata(chunk) {
            Ok(mut p) => out.append(&mut p),
            Err(orgpunk_sfapi::Error::Api { .. }) => {
                for q in *chunk {
                    match client.list_metadata(std::slice::from_ref(q)) {
                        Ok(mut p) => out.append(&mut p),
                        Err(orgpunk_sfapi::Error::Api { .. }) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(out)
}

/// The org's revisions of `members` now (after a push), and the highest RevisionCounter.
pub fn revisions(
    client: &Client,
    state: &State,
    members: &[String],
    tracking: bool,
) -> Result<(BTreeMap<String, String>, i64)> {
    if members.is_empty() {
        return Ok((BTreeMap::new(), state.max_revision));
    }
    if tracking {
        let rows: Vec<SourceMember> = client.tooling_query(&format!(
            "SELECT MemberType, MemberName, RevisionCounter, IsNameObsolete FROM SourceMember \
             WHERE RevisionCounter > {}",
            state.max_revision
        ))?;
        let r = from_source_members(state, rows);
        let map = r
            .changes
            .into_iter()
            .map(|c| (key(&c.member), c.revision))
            .collect();
        return Ok((map, r.max_revision));
    }
    let listed = client.list_metadata(&history::queries(members))?;
    let map = history::index(&listed)
        .into_iter()
        .map(|(k, p)| (k, p.last_modified_date.clone()))
        .collect();
    Ok((map, state.max_revision))
}

fn from_source_members(state: &State, rows: Vec<SourceMember>) -> Remote {
    let mut latest: BTreeMap<String, SourceMember> = BTreeMap::new();
    for r in rows {
        let k = key(&format!("{}:{}", r.member_type, r.member_name));
        if latest
            .get(&k)
            .is_none_or(|old| old.revision_counter < r.revision_counter)
        {
            latest.insert(k, r);
        }
    }
    let max = latest
        .values()
        .map(|r| r.revision_counter)
        .max()
        .unwrap_or(0)
        .max(state.max_revision);
    Remote {
        tracking: true,
        changes: latest
            .into_values()
            .map(|r| RemoteChange {
                member: format!("{}:{}", r.member_type, r.member_name),
                revision: r.revision_counter.to_string(),
                deleted: r.is_name_obsolete,
            })
            .collect(),
        max_revision: max,
    }
}

/// Changes from a listing: a different lastModifiedDate than at the last sync, a component
/// the project has but the state does not, a new custom component of a listed type, or a
/// synced component the org no longer lists. Standard and managed-package components are
/// only seen when the project has them, so a first pull does not bring in the whole org.
pub fn diff_listing(
    state: &State,
    local: &BTreeMap<String, LocalMember>,
    listed: &[FileProperties],
    types: &BTreeSet<String>,
) -> Vec<RemoteChange> {
    let now = history::index(listed);
    let mut out = Vec::new();
    for (k, p) in &now {
        let member = match local.get(k) {
            Some(l) => l.member.clone(),
            None => {
                let kind = k.split_once(':').map_or("", |(t, _)| t);
                let t = types
                    .iter()
                    .find(|t| t.eq_ignore_ascii_case(kind))
                    .cloned()
                    .unwrap_or_else(|| p.kind.clone());
                format!("{t}:{}", p.full_name)
            }
        };
        let changed = match state.get(&member) {
            Some(s) => s.revision != p.last_modified_date,
            None => {
                let own =
                    !p.id.is_empty() && matches!(p.manageable_state.as_str(), "" | "unmanaged");
                local.contains_key(k) || own
            }
        };
        if changed {
            out.push(RemoteChange {
                member,
                revision: p.last_modified_date.clone(),
                deleted: false,
            });
        }
    }
    for s in state.components.values() {
        let kind = s.member.split_once(':').map_or("", |(t, _)| t);
        if types.contains(kind) && !now.contains_key(&key(&s.member)) {
            out.push(RemoteChange {
                member: s.member.clone(),
                revision: String::new(),
                deleted: true,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Synced;

    fn prop(kind: &str, name: &str, date: &str, id: &str) -> FileProperties {
        FileProperties {
            kind: kind.into(),
            full_name: name.into(),
            last_modified_date: date.into(),
            id: id.into(),
            manageable_state: "unmanaged".into(),
            ..FileProperties::default()
        }
    }

    fn local(members: &[&str]) -> BTreeMap<String, LocalMember> {
        members
            .iter()
            .map(|m| {
                (
                    key(m),
                    LocalMember {
                        member: (*m).into(),
                        files: BTreeMap::new(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn listing_changes() {
        let mut state = State::default();
        for (m, rev) in [
            ("ApexClass:Same", "d1"),
            ("ApexClass:Edited", "d1"),
            ("ApexClass:Gone", "d1"),
        ] {
            state.set(Synced {
                member: m.into(),
                revision: rev.into(),
                files: BTreeMap::new(),
            });
        }
        let listed = [
            prop("ApexClass", "Same", "d1", "01p1"),
            prop("ApexClass", "Edited", "d2", "01p2"),
            prop("ApexClass", "NewInOrg", "d2", "01p3"),
            prop("ApexClass", "LocalOnlyBefore", "d2", "01p4"),
            prop("CustomObject", "Account", "d0", ""),
        ];
        let types = BTreeSet::from(["ApexClass".to_owned(), "CustomObject".to_owned()]);
        let mut c = diff_listing(
            &state,
            &local(&["ApexClass:LocalOnlyBefore"]),
            &listed,
            &types,
        );
        c.sort_by(|a, b| a.member.cmp(&b.member));
        let shown: Vec<String> = c
            .iter()
            .map(|c| format!("{}{}", c.member, if c.deleted { " (deleted)" } else { "" }))
            .collect();
        assert_eq!(
            shown,
            [
                "ApexClass:Edited",
                "ApexClass:Gone (deleted)",
                "ApexClass:LocalOnlyBefore",
                "ApexClass:NewInOrg"
            ],
            "standard Account is not new to the project"
        );
    }

    #[test]
    fn source_members_keep_the_newest() {
        let state = State {
            max_revision: 5,
            ..State::default()
        };
        let rows = vec![
            SourceMember {
                member_type: "ApexClass".into(),
                member_name: "A".into(),
                revision_counter: 7,
                is_name_obsolete: false,
            },
            SourceMember {
                member_type: "ApexClass".into(),
                member_name: "a".into(),
                revision_counter: 9,
                is_name_obsolete: true,
            },
            SourceMember {
                member_type: "CustomField".into(),
                member_name: "Account.F__c".into(),
                revision_counter: 8,
                is_name_obsolete: false,
            },
        ];
        let r = from_source_members(&state, rows);
        assert_eq!(r.max_revision, 9);
        assert_eq!(r.changes.len(), 2);
        assert!(
            r.changes
                .iter()
                .any(|c| c.member == "ApexClass:a" && c.deleted && c.revision == "9")
        );
    }
}
