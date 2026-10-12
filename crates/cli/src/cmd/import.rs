//! `opk env add <env> <alias> --import`: the org's versions of the project's components
//! committed as the starting point of the environment's history (like `terraform import`), so
//! a rollback has something to go back to for components that existed before opk.

use std::collections::BTreeSet;
use std::time::Duration;

use orgpunk_config::sfdx_project::SfdxProject;
use orgpunk_deploy::history::{self, Entry, Revision};
use orgpunk_sync::{apply, key, local, retrieve};
use serde_json::json;

use crate::context::Ctx;
use crate::output::{Exit, Failure, Output};

/// Imports the org `org` of the environment `env`; the working tree must be clean. Returns the
/// JSON of the result.
pub fn run(
    ctx: &Ctx,
    out: &Output,
    env: &str,
    org: &str,
    wait: u64,
) -> Result<serde_json::Value, Failure> {
    let root = ctx
        .config
        .project_root()
        .ok_or_else(|| Failure::error("not inside an sfdx project (no sfdx-project.json above)"))?
        .to_owned();
    let project = SfdxProject::load(&root)?;
    // The import becomes a commit of its own: nothing else may slip into it.
    let repo = orgpunk_repo::state(&root)?;
    if !repo.is_clean() {
        return Err(Failure::new(
            Exit::Refused,
            format!(
                "the working tree has uncommitted changes ({}); commit or stash them first",
                repo.dirty
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    let (client, org_id, username) = env_client(ctx, env, org)?;

    let before = local::scan(&project)?;
    let members: Vec<String> = before.values().map(|l| l.member.clone()).collect();
    if members.is_empty() {
        return Err(Failure::error("the project has no components to import"));
    }
    out.progress(format!(
        "Retrieving {} component(s) from env.{} ({org_id})…",
        members.len(),
        env
    ));
    let got = retrieve::retrieve(&client, &members, Duration::from_secs(wait * 60))?;
    for m in &got.messages {
        out.progress(format!("Warning: {m}"));
    }
    if !got.skipped.is_empty() {
        out.progress(format!(
            "Skipped (types opk does not know): {}",
            got.skipped.join(", ")
        ));
    }
    let tree = orgpunk_convert::to_source::convert(&got.files)?;
    let pulled: BTreeSet<String> = members
        .iter()
        .filter(|m| !got.skipped.contains(m))
        .map(|m| key(m))
        .collect();
    let applied = apply::apply(&project, &before, &tree, &pulled, &[])?;
    let touched: Vec<String> = applied
        .written
        .iter()
        .chain(&applied.removed)
        .cloned()
        .collect();
    let message = format!("chore: opk import {env}");
    let commit = match orgpunk_repo::commit(&root, &touched, &message)? {
        Some(c) => {
            out.progress(format!("Committed the org's versions: {}", short(&c)));
            c
        }
        // The org already matches HEAD.
        None => orgpunk_repo::state(&root)?
            .head
            .ok_or_else(|| Failure::error("the repository has no commit yet"))?,
    };

    // The starting point: the org's revision of every component, nothing deployed.
    let listed = client.list_metadata(&history::queries(&members))?;
    let index = history::index(&listed);
    let org_revision = members
        .iter()
        .filter_map(|m| {
            index.get(&m.to_lowercase()).map(|p| {
                (
                    m.clone(),
                    Revision {
                        last_modified_date: p.last_modified_date.clone(),
                        last_modified_by_id: p.last_modified_by_id.clone(),
                    },
                )
            })
        })
        .collect();
    let entry = history::load(&root, &org_id)?.append(Entry {
        commit: Some(commit.clone()),
        at: orgpunk_deploy::validations::now(),
        by: username,
        org_revision,
        import: true,
        ..Entry::default()
    })?;
    let file = history::path(&root, &org_id);
    let rel = file
        .strip_prefix(&root)
        .unwrap_or(&file)
        .to_string_lossy()
        .replace('\\', "/");
    orgpunk_repo::commit(&root, &[rel], &format!("{message}: history"))?;

    out.say(format!(
        "Imported env.{}: {} file(s) from the org at {}, history #{}.",
        env,
        touched.len(),
        short(&commit),
        entry.id
    ));
    Ok(json!({
        "env": env,
        "orgId": org_id,
        "commit": commit,
        "history": entry.id,
        "written": applied.written,
        "removed": applied.removed,
        "messages": got.messages,
    }))
}

fn short(c: &str) -> &str {
    c.get(..12).unwrap_or(c)
}

/// A client of the environment `env`'s org `org`: the CI login (which must be that org) or a
/// stored connection. Returns it with the org's Org Id and the username.
pub(crate) fn env_client(
    ctx: &Ctx,
    env: &str,
    org: &str,
) -> Result<(orgpunk_sfapi::Client, String, String), Failure> {
    if ctx.ci {
        let s = ctx.ci_session()?;
        if !orgpunk_config::project::same_org(org, &s.org_id) {
            return Err(Failure::new(
                Exit::Refused,
                format!(
                    "the CI credentials log in to {}, not {org} (env.{env})",
                    s.org_id
                ),
            ));
        }
        return Ok((s.client, s.org_id, s.username));
    }
    let auth = ctx.auth()?;
    let alias = auth
        .connections
        .resolve(org)
        .map(str::to_owned)
        .ok_or_else(|| {
            Failure::error(format!(
                "no connection to {org} (env.{env}); run `opk org login` for it"
            ))
        })?;
    let (client, conn) = ctx.client(&auth, &alias)?;
    Ok((client, conn.org_id, conn.username))
}
