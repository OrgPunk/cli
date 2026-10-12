//! Local changes, remote changes and conflicts since the last sync.

use std::collections::BTreeMap;

use crate::key;
use crate::local::LocalMember;
use crate::remote::Remote;
use crate::state::State;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Added,
    Modified,
    Deleted,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Added => "added",
            Kind::Modified => "modified",
            Kind::Deleted => "deleted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub member: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Changed in the working copy only.
    pub local: Vec<Change>,
    /// Changed in the org only.
    pub remote: Vec<Change>,
    /// Changed on both sides (or present on both without a sync to compare against).
    pub conflicts: Vec<Change>,
}

impl Status {
    pub fn is_clean(&self) -> bool {
        self.local.is_empty() && self.remote.is_empty() && self.conflicts.is_empty()
    }
}

pub fn compare(state: &State, local: &BTreeMap<String, LocalMember>, remote: &Remote) -> Status {
    let mut locals: BTreeMap<String, Change> = BTreeMap::new();
    for (k, l) in local {
        let kind = match state.components.get(k) {
            None => Some(Kind::Added),
            Some(s) if s.files != l.files => Some(Kind::Modified),
            Some(_) => None,
        };
        if let Some(kind) = kind {
            locals.insert(
                k.clone(),
                Change {
                    member: l.member.clone(),
                    kind,
                },
            );
        }
    }
    for (k, s) in &state.components {
        // Only what was in the working copy at the last sync can be deleted from it.
        if !local.contains_key(k) && !s.files.is_empty() {
            locals.insert(
                k.clone(),
                Change {
                    member: s.member.clone(),
                    kind: Kind::Deleted,
                },
            );
        }
    }

    let mut out = Status::default();
    for r in &remote.changes {
        let k = key(&r.member);
        let kind = if r.deleted {
            Kind::Deleted
        } else if state.components.contains_key(&k) {
            Kind::Modified
        } else {
            Kind::Added
        };
        match locals.remove(&k) {
            // Gone on both sides: nothing to do.
            Some(l) if l.kind == Kind::Deleted && kind == Kind::Deleted => {}
            Some(l) => out.conflicts.push(Change {
                member: l.member,
                kind,
            }),
            None => out.remote.push(Change {
                member: r.member.clone(),
                kind,
            }),
        }
    }
    out.local = locals.into_values().collect();
    for v in [&mut out.local, &mut out.remote, &mut out.conflicts] {
        v.sort_by_key(|c| key(&c.member));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::RemoteChange;
    use crate::state::Synced;

    fn member(m: &str, hash: &str) -> (String, LocalMember) {
        (
            key(m),
            LocalMember {
                member: m.into(),
                files: BTreeMap::from([(format!("{m}.file"), hash.into())]),
            },
        )
    }

    #[test]
    fn sides_and_conflicts() {
        let mut state = State::default();
        for (m, h) in [
            ("ApexClass:Clean", "h"),
            ("ApexClass:Mine", "h"),
            ("ApexClass:Theirs", "h"),
            ("ApexClass:Both", "h"),
            ("ApexClass:Removed", "h"),
        ] {
            state.set(Synced {
                member: m.into(),
                revision: "1".into(),
                files: BTreeMap::from([(format!("{m}.file"), h.into())]),
            });
        }
        let local: BTreeMap<_, _> = [
            member("ApexClass:Clean", "h"),
            member("ApexClass:Mine", "h2"),
            member("ApexClass:Theirs", "h"),
            member("ApexClass:Both", "h2"),
            member("ApexClass:New", "h"),
        ]
        .into_iter()
        .collect();
        let remote = Remote {
            changes: ["ApexClass:Theirs", "ApexClass:Both", "ApexClass:InOrg"]
                .iter()
                .map(|m| RemoteChange {
                    member: (*m).into(),
                    revision: "2".into(),
                    deleted: false,
                })
                .collect(),
            ..Remote::default()
        };
        let s = compare(&state, &local, &remote);
        let show = |v: &[Change]| {
            v.iter()
                .map(|c| format!("{} {}", c.member, c.kind.as_str()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            show(&s.local),
            [
                "ApexClass:Mine modified",
                "ApexClass:New added",
                "ApexClass:Removed deleted"
            ]
        );
        assert_eq!(
            show(&s.remote),
            ["ApexClass:InOrg added", "ApexClass:Theirs modified"]
        );
        assert_eq!(show(&s.conflicts), ["ApexClass:Both modified"]);
    }
}
