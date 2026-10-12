//! `opk env rollback <env> [<deploy-id>]`: an environment back to an earlier deploy.
//!
//! Components changed after that deploy go back as of its commit, components created after
//! it are deleted; the result goes through the usual deploy (protect, drift, history).

use clap::Args as ClapArgs;
use orgpunk_config::project::ProjectConfig;
use orgpunk_convert::destructive::{Deletions, Phase};
use orgpunk_deploy::history;
use serde_json::json;

use crate::cmd::deploy::{self, Origin, PackageFlags};
use crate::context::Ctx;
use crate::output::{Done, Failure, Output};

#[derive(ClapArgs)]
pub struct Args {
    /// The environment (see `opk env list`).
    #[arg(add = crate::complete::envs())]
    env: String,
    /// The deploy to go back to, as `opk env history` numbers it; by default the one before
    /// the last.
    #[arg(value_name = "DEPLOY_ID")]
    to: Option<u64>,
    /// Roll back over components changed in the org since opk last deployed them (drift).
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    flags: PackageFlags,
}

pub fn run(ctx: &Ctx, out: &Output, a: Args) -> Result<Done, Failure> {
    let root = ctx.config.project_root().ok_or_else(|| {
        Failure::error("not inside an opk project (no .orgpunk/ or sfdx-project.json above)")
    })?;
    let config = ProjectConfig::load(root)?;
    let env = config.envs.get(&a.env).ok_or_else(|| {
        Failure::error(format!(
            "no environment `{}` in .orgpunk/config.toml; see `opk env list`",
            a.env
        ))
    })?;
    let loaded = history::load(root, &env.org)?;
    let plan = history::rollback_plan(&loaded.history, a.to).map_err(Failure::error)?;
    out.progress(format!(
        "Rollback of {} to deploy #{} ({}): {} to restore, {} to delete",
        a.env,
        plan.target,
        short(&plan.commit),
        plan.restore.len(),
        plan.delete.len()
    ));

    let mut deletions = Deletions::default();
    for m in &plan.delete {
        let (kind, name) = m
            .split_once(':')
            .ok_or_else(|| Failure::error(format!("bad history member `{m}`")))?;
        deletions
            .add(Phase::Post, kind, name)
            .map_err(|e| Failure::error(e.to_string()))?;
    }
    let fallback = ctx.config.get("api-version")?.map(|r| r.value);
    let build = orgpunk_deploy::source::build_at_commit(
        root,
        &plan.commit,
        Some(&plan.restore),
        &deletions,
        fallback.as_deref(),
    )?;
    for w in &build.warnings {
        out.progress(format!("Warning: {w}"));
    }
    if !build.missing.is_empty() {
        out.progress(format!(
            "No version as of deploy #{} for {} (not in its commit; `opk env add <env> <alias> --import` sets a starting point), skipped",
            plan.target,
            build.missing.join(", ")
        ));
    }
    if build.components == 0 && plan.delete.is_empty() {
        return Err(Failure::error(
            "nothing to roll back: no component has a version in that commit",
        ));
    }

    let origin = Origin {
        commit: plan.commit.clone(),
        rollback_to: Some(plan.target),
    };
    let args = deploy::Args::package(build.dir.path().to_owned(), a.env, origin, a.force, a.flags);
    let mut done = deploy::run(ctx, out, args)?;
    done.json["rollback"] = json!({
        "to": plan.target,
        "commit": plan.commit,
        "restored": plan.restore,
        "deleted": plan.delete,
        "skipped": build.missing,
    });
    Ok(done)
}

pub(crate) fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}
