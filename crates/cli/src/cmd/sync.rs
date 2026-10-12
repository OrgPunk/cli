//! `opk status / pull / push`: the working copy and an org, like git with a remote.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args as ClapArgs;
use orgpunk_config::sfdx_project::SfdxProject;
use orgpunk_sync::state::Synced;
use orgpunk_sync::status::{Change, Kind, Status};
use orgpunk_sync::{apply, key, local, remote, retrieve, state::State, status};
use serde_json::{Value, json};

use orgpunk_convert::destructive::{Deletions, Phase};
use orgpunk_convert::manifest::Manifest;
use orgpunk_convert::select::Selector;
use orgpunk_deploy::source::Selection;

use crate::cmd::deploy::{self, PackageFlags};
use crate::context::Ctx;
use crate::output::{Done, Exit, Failure, Output};

#[derive(ClapArgs)]
pub struct PullArgs {
    /// Org: alias, username or Org Id; defaults to target-org.
    #[arg(long, short = 'o', add = crate::complete::orgs())]
    org: Option<String>,
    /// On conflicts, take the org's version (overwrites local changes).
    #[arg(long)]
    force: bool,
    /// Minutes to wait for the retrieve.
    #[arg(long, default_value_t = 33)]
    wait: u64,
}

#[derive(ClapArgs)]
pub struct StatusArgs {
    /// Org: alias, username or Org Id; defaults to target-org.
    #[arg(long, short = 'o', add = crate::complete::orgs())]
    org: Option<String>,
}

/// Minutes to wait for the retrieve that compares components with no earlier sync.
const SETTLE_WAIT_MINUTES: u64 = 33;

/// What both sides look like now, against the last sync.
struct Snapshot {
    project: SfdxProject,
    state: State,
    local: std::collections::BTreeMap<String, local::LocalMember>,
    remote: remote::Remote,
    status: Status,
    alias: String,
    client: orgpunk_sfapi::Client,
}

fn snapshot(ctx: &Ctx, out: &Output, org: Option<&str>) -> Result<Snapshot, Failure> {
    let root = ctx
        .config
        .project_root()
        .ok_or_else(|| Failure::error("not inside an sfdx project (no sfdx-project.json above)"))?;
    let project = SfdxProject::load(root)?;
    let auth = ctx.auth()?;
    let alias = ctx.resolve_alias(&auth, org)?;
    let (client, conn) = ctx.client(&auth, &alias)?;
    let state = State::load(root, &conn.org_id)?;
    let local = local::scan(&project)?;
    if remote::whole_org(&state, &local) {
        out.progress(format!(
            "Nothing local yet: listing everything in {alias} ({})…",
            conn.org_id
        ));
    } else {
        out.progress(format!("Checking {alias} ({}) for changes…", conn.org_id));
    }
    let remote = remote::changes(&client, &state, &local)?;
    let mut status = status::compare(&state, &local, &remote);
    let mut state = state;
    // On both sides with nothing synced to compare against: identical files are not conflicts.
    let unknown = status
        .conflicts
        .iter()
        .filter(|c| state.get(&c.member).is_none())
        .count();
    if unknown > 0 {
        out.progress(format!(
            "Comparing {unknown} component(s) present locally and in {alias} with no earlier sync…"
        ));
        let settled = orgpunk_sync::settle::settle(
            &client,
            &mut state,
            &local,
            &remote,
            &mut status,
            Duration::from_secs(SETTLE_WAIT_MINUTES * 60),
        )?;
        if !settled.is_empty() {
            state.save(root)?;
            out.progress(format!(
                "{} identical on both sides, recorded as synced.",
                settled.len()
            ));
        }
    }
    Ok(Snapshot {
        project,
        state,
        local,
        remote,
        status,
        alias,
        client,
    })
}

pub fn status(ctx: &Ctx, out: &Output, a: StatusArgs) -> Result<Done, Failure> {
    let s = snapshot(ctx, out, a.org.as_deref())?;
    show(out, &s);
    Ok(Done::ok(json!({
        "org": s.alias,
        "tracking": s.remote.tracking,
        "local": changes(&s.status.local),
        "remote": changes(&s.status.remote),
        "conflicts": changes(&s.status.conflicts),
    })))
}

pub fn pull(ctx: &Ctx, out: &Output, a: PullArgs) -> Result<Done, Failure> {
    let s = snapshot(ctx, out, a.org.as_deref())?;
    if !s.status.conflicts.is_empty() && !a.force {
        show(out, &s);
        return Err(Failure::new(
            Exit::Refused,
            format!(
                "{} conflict(s) with {}; nothing was pulled. --force takes the org's version",
                s.status.conflicts.len(),
                s.alias
            ),
        ));
    }
    let mut take: Vec<&Change> = s.status.remote.iter().collect();
    if a.force {
        take.extend(&s.status.conflicts);
    }
    let fetch: Vec<String> = take
        .iter()
        .filter(|c| c.kind != Kind::Deleted)
        .map(|c| c.member.clone())
        .collect();
    let delete: Vec<String> = take
        .iter()
        .filter(|c| c.kind == Kind::Deleted)
        .map(|c| c.member.clone())
        .collect();

    let root = s.project.root.clone();
    let mut state = s.state;
    let got = if fetch.is_empty() {
        None
    } else {
        out.progress(format!(
            "Retrieving {} component(s) from {}…",
            fetch.len(),
            s.alias
        ));
        Some(retrieve::retrieve(
            &s.client,
            &fetch,
            Duration::from_secs(a.wait * 60),
        )?)
    };
    let mut written = Vec::new();
    let mut removed = Vec::new();
    let mut skipped = Vec::new();
    if let Some(got) = &got {
        for m in &got.messages {
            out.progress(format!("Warning: {m}"));
        }
        skipped.clone_from(&got.skipped);
        if !skipped.is_empty() {
            out.progress(format!(
                "Skipped (types opk does not know): {}",
                skipped.join(", ")
            ));
        }
    }
    let tree = match &got {
        Some(g) => orgpunk_convert::to_source::convert(&g.files)?,
        None => Default::default(),
    };
    let pulled: BTreeSet<String> = fetch
        .iter()
        .filter(|m| !skipped.contains(m))
        .map(|m| key(m))
        .collect();
    let applied = apply::apply(&s.project, &s.local, &tree, &pulled, &delete)?;
    written.extend(applied.written);
    removed.extend(applied.removed);

    // The new state: what the org and the working copy hold now for what was synced.
    let after = local::scan(&s.project)?;
    let revisions: std::collections::BTreeMap<String, String> = s
        .remote
        .changes
        .iter()
        .map(|c| (key(&c.member), c.revision.clone()))
        .collect();
    for m in &fetch {
        if skipped.contains(m) {
            continue;
        }
        let k = key(m);
        state.set(Synced {
            member: after
                .get(&k)
                .map_or_else(|| m.clone(), |l| l.member.clone()),
            revision: revisions.get(&k).cloned().unwrap_or_default(),
            files: after.get(&k).map(|l| l.files.clone()).unwrap_or_default(),
        });
    }
    for m in &delete {
        state.remove(m);
    }
    state.tracking = Some(s.remote.tracking);
    if skipped.is_empty() {
        state.max_revision = s.remote.max_revision;
    }
    state.save(&root)?;

    if fetch.is_empty() && delete.is_empty() {
        out.say(format!("Already up to date with {}.", s.alias));
    } else {
        out.say(format!(
            "Pulled {} component(s), deleted {} from {}: {} file(s) written, {} removed.",
            fetch.len() - skipped.len(),
            delete.len(),
            s.alias,
            written.len(),
            removed.len()
        ));
    }
    Ok(Done::ok(json!({
        "org": s.alias,
        "pulled": fetch,
        "deleted": delete,
        "skipped": skipped,
        "written": written,
        "removed": removed,
        "localChanges": changes(&s.status.local),
    })))
}

#[derive(ClapArgs)]
pub struct PushArgs {
    /// Org: alias, username or Org Id; defaults to target-org.
    #[arg(long, short = 'o', add = crate::complete::orgs())]
    org: Option<String>,
    /// Only the changes of components with files under these paths.
    paths: Vec<PathBuf>,
    /// Only the changes of these components: Type, Type:Name or Type:Prefix* (repeat the
    /// flag). Combines with paths.
    #[arg(long, value_name = "TYPE:NAME", add = crate::complete::metadata_types())]
    metadata: Vec<Selector>,
    /// On conflicts, send the local version (overwrites the org's changes); also deploys over
    /// drift.
    #[arg(long)]
    force: bool,
    /// Validate only (checkOnly): nothing changes in the org and the sync stays as it was;
    /// prints a validation id for --quick.
    #[arg(long, conflicts_with_all = ["quick", "dry_run"])]
    validate: bool,
    /// Quick deploy of an earlier `push --validate`; push the same changes as then.
    #[arg(long, value_name = "VALIDATION_ID", conflicts_with = "dry_run")]
    quick: Option<String>,
    /// Over the limits: send in several parts ordered by dependencies. Each part is its own
    /// deploy: a failure stops the rest, and the parts before it stay in the org.
    #[arg(long, conflicts_with_all = ["validate", "quick"])]
    split: bool,
    #[command(flatten)]
    flags: PackageFlags,
}

/// Sends local changes since the last sync through the deploy pipeline (protect, drift,
/// history); local deletions need --allow-destructive.
pub fn push(ctx: &Ctx, out: &Output, a: PushArgs) -> Result<Done, Failure> {
    let mut s = snapshot(ctx, out, a.org.as_deref())?;
    if !a.paths.is_empty() || !a.metadata.is_empty() {
        let root = s.project.root.clone();
        let wanted = |c: &Change| selected(&root, &s.local, &s.state, c, &a.paths, &a.metadata);
        let keep = |v: &[Change]| v.iter().filter(|c| wanted(c)).cloned().collect::<Vec<_>>();
        let (local, conflicts) = (keep(&s.status.local), keep(&s.status.conflicts));
        s.status.local = local;
        s.status.conflicts = conflicts;
    }
    if !s.status.conflicts.is_empty() && !a.force {
        show(out, &s);
        return Err(Failure::new(
            Exit::Refused,
            format!(
                "{} conflict(s) with {}; nothing was pushed. --force overwrites the org's version",
                s.status.conflicts.len(),
                s.alias
            ),
        ));
    }
    let mut send = Vec::new();
    let mut delete = Vec::new();
    for c in &s.status.local {
        match c.kind {
            Kind::Deleted => delete.push(c.member.clone()),
            _ => send.push(c.member.clone()),
        }
    }
    if a.force {
        for c in &s.status.conflicts {
            if s.local.contains_key(&key(&c.member)) {
                send.push(c.member.clone());
            } else {
                delete.push(c.member.clone());
            }
        }
    }
    if send.is_empty() && delete.is_empty() {
        out.say(format!("Nothing to push to {}.", s.alias));
        return Ok(Done::ok(
            json!({ "org": s.alias, "pushed": [], "deleted": [] }),
        ));
    }
    out.progress(format!(
        "Pushing {} component(s), deleting {} → {}",
        send.len(),
        delete.len(),
        s.alias
    ));

    // The package: the changed members from the working copy, deletions next to it.
    let fallback = ctx.config.get("api-version")?.map(|r| r.value);
    let version = s.project.source_api_version.clone().or(fallback.clone());
    let mut selectors: Vec<Selector> = send
        .iter()
        .filter_map(|m| {
            let (kind, name) = m.split_once(':')?;
            // A label has no file of its own: its labels file goes.
            Some(if kind == "CustomLabel" {
                Selector {
                    kind: "CustomLabels".into(),
                    name: Some("CustomLabels".into()),
                }
            } else {
                Selector {
                    kind: kind.into(),
                    name: Some(name.into()),
                }
            })
        })
        .collect();
    selectors.sort_by_key(|s| s.to_string());
    selectors.dedup_by_key(|s| s.to_string());
    let (dir, _keep): (std::path::PathBuf, Box<dyn std::any::Any>) = if selectors.is_empty() {
        let tmp = tempfile::Builder::new()
            .prefix("orgpunk-mdapi-")
            .tempdir()
            .map_err(|e| Failure::error(e.to_string()))?;
        let manifest = Manifest {
            version: version.clone(),
            ..Manifest::default()
        };
        std::fs::write(tmp.path().join("package.xml"), manifest.to_xml())
            .map_err(|e| Failure::error(e.to_string()))?;
        (tmp.path().to_owned(), Box::new(tmp))
    } else {
        let selection = Selection {
            paths: Vec::new(),
            metadata: selectors,
        };
        let build = orgpunk_deploy::source::build(&s.project, &selection, fallback.as_deref())?;
        for w in &build.warnings {
            out.progress(format!("Warning: {w}"));
        }
        (build.dir.path().to_owned(), Box::new(build))
    };
    if !delete.is_empty() {
        let mut deletions = Deletions::default();
        for m in &delete {
            if let Some((kind, name)) = m.split_once(':') {
                deletions
                    .add(Phase::Post, kind, name)
                    .map_err(|e| Failure::error(e.to_string()))?;
            }
        }
        let text = std::fs::read_to_string(dir.join("package.xml"))
            .map_err(|e| Failure::error(e.to_string()))?;
        let manifest = Manifest::parse(&text).map_err(Failure::error)?;
        deletions
            .write(&dir, &manifest)
            .map_err(|e| Failure::error(e.to_string()))?;
    }

    // A dry run or a validation changes nothing in the org: the sync stays as it was.
    let unchanged = a.flags.dry_run || a.validate;
    let mut args = deploy::Args::ready(dir, Some(s.alias.clone()), None, None, a.force, a.flags);
    args.validate = a.validate;
    args.quick = a.quick;
    args.split = a.split;
    let mut done = deploy::run(ctx, out, args)?;
    if unchanged || done.code != Exit::Ok {
        return Ok(done);
    }

    // Synced now: the org's new revisions and the working copy's hashes.
    let mut state = s.state;
    let after = local::scan(&s.project)?;
    let (revs, max) = remote::revisions(&s.client, &state, &send, s.remote.tracking)?;
    for m in &send {
        let k = key(m);
        state.set(Synced {
            member: m.clone(),
            revision: revs.get(&k).cloned().unwrap_or_default(),
            files: after.get(&k).map(|l| l.files.clone()).unwrap_or_default(),
        });
    }
    for m in &delete {
        state.remove(m);
    }
    state.tracking = Some(s.remote.tracking);
    state.max_revision = max;
    state.save(&s.project.root)?;
    done.json["pushed"] = json!(send);
    done.json["deleted"] = json!(delete);
    Ok(done)
}

fn show(out: &Output, s: &Snapshot) {
    let st = &s.status;
    if st.is_clean() {
        out.say(format!("Up to date with {}.", s.alias));
        return;
    }
    let section = |title: &str, v: &[Change]| {
        if v.is_empty() {
            return;
        }
        out.say(format!("{title} ({}):", v.len()));
        for c in v {
            out.say(format!("  {:<9} {}", c.kind.as_str(), c.member));
        }
    };
    section("In the org (opk pull)", &st.remote);
    section("Local (opk push)", &st.local);
    section(
        "Conflicts (changed on both sides; --force picks a side)",
        &st.conflicts,
    );
    if st
        .conflicts
        .iter()
        .any(|c| s.state.get(&c.member).is_none())
    {
        out.say(format!(
            "Not synced with {} before: components that differ on both sides are conflicts. \
             `opk pull --force` takes the org's versions, `opk push --force` sends yours.",
            s.alias
        ));
    }
}

fn changes(v: &[Change]) -> Value {
    Value::Array(
        v.iter()
            .map(|c| json!({ "member": c.member, "change": c.kind.as_str() }))
            .collect(),
    )
}

/// Whether a change is among the components push was asked for: a selector matches it, or one
/// of its files (now, or at the last sync for a deletion) lies under one of the paths.
fn selected(
    root: &Path,
    local: &std::collections::BTreeMap<String, local::LocalMember>,
    state: &State,
    c: &Change,
    paths: &[PathBuf],
    metadata: &[Selector],
) -> bool {
    if let Some((kind, name)) = c.member.split_once(':') {
        if metadata.iter().any(|s| s.matches(kind, name)) {
            return true;
        }
    }
    if paths.is_empty() {
        return false;
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    let wanted: Vec<PathBuf> = paths
        .iter()
        .filter_map(|p| {
            let abs = p.canonicalize().unwrap_or_else(|_| {
                std::env::current_dir()
                    .map(|d| d.join(p))
                    .unwrap_or_else(|_| p.clone())
            });
            abs.strip_prefix(&root).ok().map(Path::to_owned)
        })
        .collect();
    let k = key(&c.member);
    let files = local
        .get(&k)
        .map(|l| &l.files)
        .or_else(|| state.get(&c.member).map(|s| &s.files));
    files.is_some_and(|files| {
        files
            .keys()
            .any(|f| wanted.iter().any(|w| Path::new(f).starts_with(w)))
    })
}
