//! `opk env …`: the environments of .orgpunk/config.toml — the orgs of the pipeline (qa, uat, prod;
//! the master is one of them) with protect, history and rollback.

use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use clap::{Args, Subcommand};
use orgpunk_auth::OrgType;
use orgpunk_config::project::{
    DEFAULT_MASTER_PROTECT, Environment, Policy, ProjectConfig, same_org, save_envs,
    valid_env_name, valid_org_id,
};
use orgpunk_config::sfdx_project::SfdxProject;
use orgpunk_env::protect::Rules;
use serde_json::{Value, json};

use crate::cmd::deploy::{self, Origin};
use crate::cmd::rollback::{self, short};
use crate::context::Ctx;
use crate::output::{Done, Exit, Failure, Output};

#[derive(Subcommand)]
pub enum Command {
    /// Environments: their orgs, protect, branches, and whether this machine has a connection.
    List,
    /// Add an environment: a connected org by alias (or an Org Id) with its protect.
    Add(AddArgs),
    /// Change an environment's org, protect, branches or next.
    Set(SetArgs),
    /// Remove an environment from .orgpunk/config.toml; its history stays.
    Remove {
        /// The environment.
        #[arg(add = crate::complete::envs())]
        name: String,
    },
    /// Deploy to an environment: the project, a part of it, or with --from what another
    /// environment runs.
    Deploy(deploy::Args),
    /// Take an environment back to an earlier deploy from its history.
    Rollback(rollback::Args),
    /// Record the org as it is now in the history: changes made around opk stop being drift,
    /// components the history did not know get a baseline. Files are not touched.
    Sync {
        /// The environment.
        #[arg(add = crate::complete::envs())]
        name: String,
        /// Only show what differs from the history.
        #[arg(long)]
        dry_run: bool,
        /// Do not commit the history after it.
        #[arg(long)]
        no_git_commit: bool,
    },
    /// The deploys of an environment, newest first: ids for rollback.
    History {
        /// The environment.
        #[arg(add = crate::complete::envs())]
        name: String,
        /// How many deploys to show.
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Args)]
pub struct AddArgs {
    /// Name of the environment: qa, uat, prod…
    name: String,
    /// The org: a connection alias or username (see `opk org list`), or an Org Id.
    #[arg(add = crate::complete::orgs())]
    org: String,
    /// The master: the source of truth, usually production. Its protect defaults to confirm,
    /// validate-first, branch.
    #[arg(long)]
    master: bool,
    /// Protect policies, comma-separated: confirm, validate-first, branch, no-destructive,
    /// locked; `none` for no protect.
    #[arg(long, value_delimiter = ',', value_parser = parse_policy)]
    protect: Option<Vec<Option<Policy>>>,
    /// Branches a `branch` policy allows, comma-separated globs (release/*); default main.
    #[arg(long, value_delimiter = ',')]
    branches: Option<Vec<String>>,
    /// The environment that usually comes after this one (`env deploy <next> --from <name>`).
    #[arg(long, value_name = "ENV", add = crate::complete::envs())]
    next: Option<String>,
    /// Commit the org's current versions as the starting point of the history, so a rollback
    /// can restore what existed before opk. Needs a clean working tree.
    #[arg(long)]
    import: bool,
    /// Confirms moving the master without a prompt: the current master's Org Id.
    #[arg(long, value_name = "ORG_ID", requires = "master")]
    confirm_master: Option<String>,
    /// Minutes to wait for the --import retrieve.
    #[arg(long, default_value_t = 33)]
    wait: u64,
}

#[derive(Args)]
pub struct SetArgs {
    /// The environment.
    #[arg(add = crate::complete::envs())]
    name: String,
    /// Another org: a connection alias or username, or an Org Id.
    #[arg(long, add = crate::complete::orgs())]
    org: Option<String>,
    /// Make it the master.
    #[arg(long)]
    master: bool,
    /// Protect policies, comma-separated; `none` removes them all.
    #[arg(long, value_delimiter = ',', value_parser = parse_policy)]
    protect: Option<Vec<Option<Policy>>>,
    /// Branches a `branch` policy allows, comma-separated globs.
    #[arg(long, value_delimiter = ',')]
    branches: Option<Vec<String>>,
    /// The environment that usually comes after this one.
    #[arg(long, value_name = "ENV", add = crate::complete::envs(), conflicts_with = "no_next")]
    next: Option<String>,
    /// Remove next.
    #[arg(long)]
    no_next: bool,
    /// Confirms moving the master without a prompt: the current master's Org Id.
    #[arg(long, value_name = "ORG_ID", requires = "master")]
    confirm_master: Option<String>,
}

/// A policy, or `None` for `none`.
fn parse_policy(s: &str) -> Result<Option<Policy>, String> {
    match s.trim() {
        "none" | "" => Ok(None),
        p => p.parse().map(Some),
    }
}

pub fn run(ctx: &Ctx, out: &Output, c: Command) -> Result<Done, Failure> {
    match c {
        Command::List => list(ctx, out),
        Command::Add(a) => add(ctx, out, a),
        Command::Set(a) => set(ctx, out, a),
        Command::Remove { name } => remove(ctx, out, &name),
        Command::Deploy(a) => deploy_env(ctx, out, a),
        Command::Rollback(a) => rollback::run(ctx, out, a),
        Command::Sync {
            name,
            dry_run,
            no_git_commit,
        } => sync(ctx, out, &name, dry_run, no_git_commit),
        Command::History { name, limit } => history(ctx, out, &name, limit),
    }
}

fn root(ctx: &Ctx) -> Result<&Path, Failure> {
    ctx.config
        .project_root()
        .filter(|r| r.join(".orgpunk").is_dir())
        .ok_or_else(|| Failure::error("not an opk project; run `opk init` first"))
}

fn env_of<'a>(config: &'a ProjectConfig, name: &str) -> Result<&'a Environment, Failure> {
    config.envs.get(name).ok_or_else(|| {
        Failure::error(format!(
            "no environment `{name}` in .orgpunk/config.toml; see `opk env list`"
        ))
    })
}

/// The Org Id of a connection alias or username, or an Org Id itself. A scratch org cannot be
/// an environment: it is short-lived.
fn org_id_of(ctx: &Ctx, org: &str) -> Result<String, Failure> {
    let auth = ctx.auth()?;
    if let Some(alias) = auth.connections.resolve(org) {
        let conn = auth.connection(alias)?;
        if conn.org_type == Some(OrgType::Scratch) {
            return Err(Failure::error(
                "a scratch org cannot be an environment: it is short-lived",
            ));
        }
        return Ok(conn.org_id.clone());
    }
    if valid_org_id(org) {
        return Ok(org.to_owned());
    }
    Err(Failure::error(format!(
        "connection `{org}` not found; log in first (`opk org login --alias {org}`) or pass \
         the Org Id"
    )))
}

fn add(ctx: &Ctx, out: &Output, a: AddArgs) -> Result<Done, Failure> {
    let root = root(ctx)?.to_owned();
    if !valid_env_name(&a.name) {
        return Err(Failure::error(
            "an environment name has only letters, digits, - and _",
        ));
    }
    let config = ProjectConfig::load(&root)?;
    let org = org_id_of(ctx, &a.org)?;
    let interactive = !ctx.ci && std::io::stdin().is_terminal();

    // Adding an existing environment again only updates it: same org, new settings.
    let mut env = match config.envs.get(&a.name) {
        Some(e) if same_org(&e.org, &org) => e.clone(),
        Some(e) => {
            return Err(Failure::error(format!(
                "env.{} is {}; `opk env set {} --org {}` moves it",
                a.name, e.org, a.name, a.org
            )));
        }
        None => Environment::new(&a.name, &org, false),
    };
    let new_master = a.master && !env.master;
    if new_master {
        env.master = true;
        if a.protect.is_none() {
            env.protect = if interactive {
                choose_protect()?
            } else {
                DEFAULT_MASTER_PROTECT.to_vec()
            };
        }
    }
    if let Some(p) = a.protect {
        env.protect = p.into_iter().flatten().collect();
    }
    if let Some(b) = a.branches {
        env.branches = b;
    }
    if a.next.is_some() {
        env.next = a.next;
    }
    let old_master = moved_master(&config, &env, a.confirm_master.as_deref(), interactive)?;

    // The import commits on its own, from a clean tree: before the config changes.
    let import = if a.import || (interactive && can_import(out, &root)? && ask_import(&a.name)?) {
        Some(crate::cmd::import::run(ctx, out, &a.name, &org, a.wait)?)
    } else {
        None
    };

    let mut save = vec![&env];
    save.extend(old_master.as_ref());
    save_envs(&root, &save, &[])?;
    out.say(format!(
        "env.{}: {org}{}, protect: {}",
        env.name,
        if env.master { " (master)" } else { "" },
        policies(&env.protect)
    ));
    if let Some(m) = &old_master {
        out.say(format!("env.{} is no longer the master.", m.name));
    }
    out.progress("Commit .orgpunk/config.toml so the team and CI share the environment.");
    let mut json = env_json(ctx, &config, &env);
    json["import"] = import.unwrap_or(Value::Null);
    Ok(Done::ok(json))
}

fn set(ctx: &Ctx, out: &Output, a: SetArgs) -> Result<Done, Failure> {
    let root = root(ctx)?.to_owned();
    let config = ProjectConfig::load(&root)?;
    let mut env = env_of(&config, &a.name)?.clone();
    let interactive = !ctx.ci && std::io::stdin().is_terminal();
    if let Some(org) = &a.org {
        env.org = org_id_of(ctx, org)?;
    }
    if a.master && !env.master {
        env.master = true;
        if a.protect.is_none() && env.protect.is_empty() {
            env.protect = DEFAULT_MASTER_PROTECT.to_vec();
        }
    }
    if let Some(p) = a.protect {
        env.protect = p.into_iter().flatten().collect();
    }
    if let Some(b) = a.branches {
        env.branches = b;
    }
    if let Some(n) = a.next {
        env.next = Some(n);
    }
    if a.no_next {
        env.next = None;
    }
    // A master moved to another org, or another environment made master, is confirmed.
    let current = config.master();
    if let Some(m) = current.filter(|m| m.name == env.name && !same_org(&m.org, &env.org)) {
        confirm_change(
            &m.org,
            &env.name,
            &env.org,
            a.confirm_master.as_deref(),
            interactive,
        )?;
    }
    let old_master = moved_master(&config, &env, a.confirm_master.as_deref(), interactive)?;
    let mut save = vec![&env];
    save.extend(old_master.as_ref());
    save_envs(&root, &save, &[])?;
    out.say(format!(
        "env.{}: {}{}, protect: {}",
        env.name,
        env.org,
        if env.master { " (master)" } else { "" },
        policies(&env.protect)
    ));
    Ok(Done::ok(env_json(ctx, &config, &env)))
}

/// When `env` becomes the master and another environment is it now: confirm and return that
/// environment without the flag.
fn moved_master(
    config: &ProjectConfig,
    env: &Environment,
    confirm: Option<&str>,
    interactive: bool,
) -> Result<Option<Environment>, Failure> {
    let Some(old) = config.master().filter(|m| env.master && m.name != env.name) else {
        return Ok(None);
    };
    if !same_org(&old.org, &env.org) {
        confirm_change(&old.org, &env.name, &env.org, confirm, interactive)?;
    }
    let mut old = old.clone();
    old.master = false;
    Ok(Some(old))
}

fn remove(ctx: &Ctx, out: &Output, name: &str) -> Result<Done, Failure> {
    let root = root(ctx)?;
    let config = ProjectConfig::load(root)?;
    let env = env_of(&config, name)?;
    // Environments whose next was this one lose it.
    let unlinked: Vec<Environment> = config
        .envs
        .values()
        .filter(|e| e.next.as_deref() == Some(name))
        .map(|e| Environment {
            next: None,
            ..e.clone()
        })
        .collect();
    let save: Vec<&Environment> = unlinked.iter().collect();
    save_envs(root, &save, &[name])?;
    out.say(format!(
        "env.{name} removed{}; its history stays in .orgpunk/deployments.",
        if env.master {
            " (it was the master: deploy and push stay off until a new one)"
        } else {
            ""
        }
    ));
    for e in &unlinked {
        out.progress(format!("env.{} has no next now.", e.name));
    }
    Ok(Done::ok(json!({ "removed": name })))
}

/// `env deploy`, and with --from the commit another environment runs.
fn deploy_env(ctx: &Ctx, out: &Output, mut a: deploy::Args) -> Result<Done, Failure> {
    // Even a dry run names an environment that exists.
    let to = a.env.clone().expect("clap: required");
    env_of(&ProjectConfig::load(root(ctx)?)?, &to)?;
    let Some(from) = a.from.clone() else {
        return deploy::run(ctx, out, a);
    };
    let root = root(ctx)?;
    let config = ProjectConfig::load(root)?;
    let source = env_of(&config, &from)?;
    if from == to {
        return Err(Failure::error("--from names the environment deployed to"));
    }
    let history = orgpunk_deploy::history::load(root, &source.org)?.history;
    let entry = orgpunk_deploy::history::promotable(&history).ok_or_else(|| {
        Failure::error(format!(
            "env.{from} has no complete deploy built from a commit to take"
        ))
    })?;
    let commit = entry.commit.clone().expect("promotable has a commit");
    let now = config
        .envs
        .get(&to)
        .and_then(|t| orgpunk_deploy::history::load(root, &t.org).ok())
        .and_then(|l| l.history.deploys.last().cloned());
    out.progress(format!(
        "env.{from}: #{} {} ({}, {}) → env.{to}{}",
        entry.id,
        short(&commit),
        entry.at,
        entry.by,
        match &now {
            Some(e) => format!(
                "; env.{to} now runs #{} {}",
                e.id,
                e.commit.as_deref().map_or("(no commit)", short)
            ),
            None => String::new(),
        }
    ));
    if source.next.as_deref().is_some_and(|n| n != to) {
        out.progress(format!(
            "Note: env.{from} names {} as its next, not {to}",
            source.next.as_deref().unwrap_or_default()
        ));
    }
    let fallback = ctx.config.get("api-version")?.map(|r| r.value);
    let build = orgpunk_deploy::source::build_at_commit(
        root,
        &commit,
        None,
        &Default::default(),
        fallback.as_deref(),
    )?;
    for w in &build.warnings {
        out.progress(format!("Warning: {w}"));
    }
    a.metadata_dir = Some(build.dir.path().to_owned());
    a.origin = Some(Origin {
        commit: commit.clone(),
        rollback_to: None,
    });
    let mut done = deploy::run(ctx, out, a)?;
    done.json["from"] = json!({ "env": from, "commit": commit, "deploy": entry.id });
    Ok(done)
}

fn history(ctx: &Ctx, out: &Output, name: &str, limit: usize) -> Result<Done, Failure> {
    let root = root(ctx)?;
    let config = ProjectConfig::load(root)?;
    let env = env_of(&config, name)?;
    let deploys = orgpunk_deploy::history::load(root, &env.org)?
        .history
        .deploys;
    if deploys.is_empty() {
        out.say(format!(
            "No deploys to env.{name} yet. `opk env add {name} <alias> --import` sets a \
             starting point for rollbacks."
        ));
    }
    let mut rows = Vec::new();
    for e in deploys.iter().rev().take(limit) {
        let what = if e.import {
            "import".to_owned()
        } else if e.sync {
            format!(
                "sync: {} recorded, {} gone",
                e.org_revision.len(),
                e.removed.len()
            )
        } else {
            format!("{} changed, {} created", e.changed.len(), e.created.len())
        };
        let mut notes = Vec::new();
        if let Some(to) = e.rollback_to {
            notes.push(format!("rollback to #{to}"));
        }
        if e.partial {
            notes.push("partial".into());
        }
        if e.commit.is_none() {
            notes.push("no commit".into());
        }
        out.say(format!(
            "#{:<4} {:<20} {:<12} {:<28} {}{}",
            e.id,
            e.at.get(..19).unwrap_or(&e.at).replace('T', " "),
            e.commit.as_deref().map_or("-", short),
            e.by,
            what,
            if notes.is_empty() {
                String::new()
            } else {
                format!("  ({})", notes.join(", "))
            }
        ));
        rows.push(json!({
            "id": e.id,
            "at": e.at,
            "commit": e.commit,
            "by": e.by,
            "changed": e.changed.len(),
            "created": e.created.len(),
            "import": e.import,
            "sync": e.sync,
            "rollbackTo": e.rollback_to,
            "partial": e.partial,
        }));
    }
    Ok(Done::ok(json!({ "env": name, "deploys": rows })))
}

fn list(ctx: &Ctx, out: &Output) -> Result<Done, Failure> {
    let root = ctx.config.project_root().ok_or_else(|| {
        Failure::error("not inside an opk project (no .orgpunk/ or sfdx-project.json above)")
    })?;
    let config = ProjectConfig::load(root)?;
    if config.envs.is_empty() {
        out.say("No environments; add one with `opk env add <name> <alias>` (`--master` for the source of truth).");
    } else if config.master().is_none() {
        out.say("No master: deploy and push stay off until `opk env add <name> <alias> --master`.");
    }
    let mut rows = Vec::new();
    for e in config.envs.values() {
        let json = env_json(ctx, &config, e);
        out.say(format!(
            "{:<8} {:<18} {:<20} {:<40}{}{}{}",
            e.name,
            e.org,
            json["alias"]
                .as_str()
                .unwrap_or("- (no connection)")
                .to_owned(),
            policies(&e.protect),
            // Branches only matter with a branch policy.
            if e.protect.contains(&Policy::Branch) {
                format!(" branches {}", e.branches.join(", "))
            } else {
                String::new()
            },
            e.next
                .as_deref()
                .map(|n| format!(" → {n}"))
                .unwrap_or_default(),
            if e.master { "  master" } else { "" },
        ));
        rows.push(json);
    }
    Ok(Done::ok(json!({ "environments": rows })))
}

fn env_json(ctx: &Ctx, config: &ProjectConfig, e: &Environment) -> Value {
    let alias = ctx
        .auth()
        .ok()
        .and_then(|a| a.connections.resolve(&e.org).map(str::to_owned));
    let rules = Rules::for_org(config, &e.org);
    json!({
        "name": e.name,
        "org": e.org,
        "alias": alias,
        "master": e.master,
        "protect": e.protect.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        "branches": e.branches,
        "next": e.next,
        "effectiveProtect": rules.policies.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
    })
}

fn policies(p: &[Policy]) -> String {
    if p.is_empty() {
        "no protect".into()
    } else {
        p.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ")
    }
}

/// The project has components to import and the working tree is clean for its commit.
fn can_import(out: &Output, root: &Path) -> Result<bool, Failure> {
    let project = SfdxProject::load(root)?;
    if orgpunk_sync::local::scan(&project)?.is_empty() {
        return Ok(false);
    }
    if !orgpunk_repo::state(root)?.is_clean() {
        out.progress(
            "No import: the working tree has uncommitted changes. Later: commit them and run \
             `opk env add <name> <alias> --import`.",
        );
        return Ok(false);
    }
    Ok(true)
}

fn ask_import(name: &str) -> Result<bool, Failure> {
    let answer = prompt(&format!(
        "Take env.{name}'s current versions as the starting point of its history (commits them)? [Y/n]: "
    ))?;
    Ok(answer.is_empty() || answer.eq_ignore_ascii_case("y"))
}

/// Reads the master's protect; empty input takes the default.
fn choose_protect() -> Result<Vec<Policy>, Failure> {
    let default: Vec<&str> = DEFAULT_MASTER_PROTECT.iter().map(|p| p.as_str()).collect();
    eprintln!("Protect policies: confirm, validate-first, branch, no-destructive, locked.");
    loop {
        let answer = prompt(&format!("Protect for the master [{}]: ", default.join(",")))?;
        if answer.is_empty() {
            return Ok(DEFAULT_MASTER_PROTECT.to_vec());
        }
        match answer
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::parse)
            .collect::<Result<Vec<Policy>, String>>()
        {
            Ok(p) => return Ok(p),
            Err(e) => eprintln!("{e}"),
        }
    }
}

/// Moving the master takes the current master's Org Id, typed or with --confirm-master.
fn confirm_change(
    old: &str,
    name: &str,
    new: &str,
    confirm: Option<&str>,
    interactive: bool,
) -> Result<(), Failure> {
    let refuse = |why: String| Failure::new(Exit::Refused, why);
    let typed = match (confirm, interactive) {
        (Some(c), _) => c.to_owned(),
        (None, true) => {
            eprintln!("Moving the master from {old} to env.{name} ({new}).");
            prompt("Type the current master's Org Id to confirm: ")?
        }
        (None, false) => {
            return Err(refuse(format!(
                "moving the master from {old} needs --confirm-master {old}"
            )));
        }
    };
    if same_org(&typed, old) {
        Ok(())
    } else {
        Err(refuse(format!(
            "`{typed}` is not the current master {old}; the master stays"
        )))
    }
}

fn prompt(text: &str) -> Result<String, Failure> {
    eprint!("{text}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| Failure::error(format!("reading input: {e}")))?;
    Ok(line.trim().to_owned())
}

/// `env sync`: the org's current revisions of the project's components (and of those the
/// history knows) become a history entry; what changed around opk is shown first.
fn sync(
    ctx: &Ctx,
    out: &Output,
    name: &str,
    dry_run: bool,
    no_git_commit: bool,
) -> Result<Done, Failure> {
    use orgpunk_deploy::history::{self, Entry, Revision};

    let root = root(ctx)?.to_owned();
    let config = ProjectConfig::load(&root)?;
    let env = env_of(&config, name)?.clone();
    if !dry_run {
        deploy::require_clean(&root)?;
    }
    let loaded = history::load(&root, &env.org)?;
    let latest: BTreeMap<String, Revision> = loaded
        .history
        .latest_revisions()
        .into_iter()
        .map(|(k, r)| (k, r.clone()))
        .collect();

    // Members by key: the project's now, and those the history still knows.
    let mut members: BTreeMap<String, String> = BTreeMap::new();
    let project = SfdxProject::load(&root)?;
    for l in orgpunk_sync::local::scan(&project)?.values() {
        members.insert(l.member.to_lowercase(), l.member.clone());
    }
    for e in &loaded.history.deploys {
        for m in e.org_revision.keys() {
            if latest.contains_key(&m.to_lowercase()) {
                members.entry(m.to_lowercase()).or_insert_with(|| m.clone());
            }
        }
    }
    if members.is_empty() {
        return Err(Failure::error(
            "nothing to sync: the project has no components and the history knows none",
        ));
    }
    let list: Vec<String> = members.values().cloned().collect();
    let (client, org_id, username) = crate::cmd::import::env_client(ctx, name, &env.org)?;
    out.progress(format!(
        "Listing {} component(s) in env.{name} ({org_id})…",
        list.len()
    ));
    let listed = client.list_metadata(&history::queries(&list))?;
    let now = history::index(&listed);
    let in_org = |m: &str| now.get(&m.trim_end_matches('/').to_lowercase()).copied();

    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    let mut baseline = Vec::new();
    let mut org_revision = BTreeMap::new();
    for (k, m) in &members {
        match (latest.get(k), in_org(m)) {
            (Some(r), Some(p)) => {
                if r.last_modified_date != p.last_modified_date
                    || r.last_modified_by_id != p.last_modified_by_id
                {
                    modified.push((m.clone(), p));
                }
            }
            (Some(_), None) => deleted.push(m.clone()),
            (None, Some(_)) => baseline.push(m.clone()),
            (None, None) => {}
        }
        if let Some(p) = in_org(m) {
            org_revision.insert(
                m.clone(),
                Revision {
                    last_modified_date: p.last_modified_date.clone(),
                    last_modified_by_id: p.last_modified_by_id.clone(),
                },
            );
        }
    }

    let json = json!({
        "env": name,
        "modified": modified.iter().map(|(m, _)| m).collect::<Vec<_>>(),
        "deleted": deleted,
        "baseline": baseline,
    });
    if modified.is_empty() && deleted.is_empty() && baseline.is_empty() {
        out.say(format!("env.{name} matches its history."));
        return Ok(Done::ok(json));
    }
    if !modified.is_empty() {
        out.say(format!(
            "Changed in the org around opk ({}):",
            modified.len()
        ));
        for (m, p) in &modified {
            out.say(format!(
                "  {m:<50} {} by {}",
                p.last_modified_date
                    .get(..19)
                    .unwrap_or(&p.last_modified_date),
                if p.last_modified_by_name.is_empty() {
                    &p.last_modified_by_id
                } else {
                    &p.last_modified_by_name
                }
            ));
        }
    }
    if !deleted.is_empty() {
        out.say(format!("Gone from the org ({}):", deleted.len()));
        for m in &deleted {
            out.say(format!("  {m}"));
        }
    }
    if !baseline.is_empty() {
        out.say(format!(
            "Not in the history yet, get a baseline ({}): {}{}",
            baseline.len(),
            baseline
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            if baseline.len() > 5 { ", …" } else { "" }
        ));
    }
    if dry_run {
        return Ok(Done::ok(json));
    }

    let entry = loaded.append(Entry {
        at: orgpunk_deploy::validations::now(),
        by: username,
        org_revision,
        removed: deleted.clone(),
        sync: true,
        ..Entry::default()
    })?;
    out.say(format!(
        "History #{}: env.{name} recorded as it is now.",
        entry.id
    ));
    if !modified.is_empty() {
        out.progress(format!(
            "The org's versions of the changed components are not in the repo by this: the next \
             deploy overwrites them. To keep them: `opk pull -o <alias of {org_id}>`, commit, \
             and deploy."
        ));
    }
    let commit = if no_git_commit {
        None
    } else {
        deploy::commit_history(
            out,
            &root,
            &org_id,
            &format!("chore: opk env sync {name} #{}", entry.id),
        )
    };
    let mut json = json;
    json["history"] = json!({ "id": entry.id, "commit": commit });
    Ok(Done::ok(json))
}
