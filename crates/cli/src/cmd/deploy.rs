//! `opk env deploy <env>`: the project source (converted to mdapi on the fly), a ready mdapi
//! directory or, with `--from`, the commit last deployed to another environment. A package over
//! the deploy limits goes in several parts with `--split`. The pipeline also serves push and
//! rollback.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Args as ClapArgs, ValueEnum};
use orgpunk_auth::{Auth, Connection};
use orgpunk_config::project::{Policy, ProjectConfig, same_org};
use orgpunk_config::sfdx_project::SfdxProject;
use orgpunk_convert::manifest::Manifest;
use orgpunk_convert::select::Selector;
use orgpunk_deploy::history::{self, Tracker};
use orgpunk_deploy::source::{Selection, SourceBuild};
use orgpunk_deploy::split::{self, Plan};
use orgpunk_deploy::validations::Validation;
use orgpunk_deploy::{Limits, Outcome, Package, Request};
use orgpunk_env::protect::{Decision, Refusal, Rules, check, check_destructive};
use orgpunk_sfapi::{Client, DeployOptions, DeployResult, DeployStatus, TestLevel};
use serde_json::json;

use crate::context::Ctx;
use crate::output::{Done, Exit, Failure, Output};

#[derive(ClapArgs)]
pub struct Args {
    /// The environment to deploy to (see `opk env list`): its org, with its protect.
    #[arg(value_name = "ENV", required = true, add = crate::complete::envs())]
    pub(crate) env: Option<String>,
    /// Set by push: an org by alias instead of an environment.
    #[arg(skip)]
    to: Option<String>,
    /// Deploy the commit last deployed to this environment, e.g. `opk env deploy uat --from
    /// qa`: what was tested there goes on, whatever the working copy holds.
    #[arg(long, value_name = "ENV", add = crate::complete::envs(), conflicts_with_all = ["paths", "metadata", "metadata_dir", "quick", "validate"])]
    pub(crate) from: Option<String>,
    /// Deploy over components changed in the org since opk last deployed them (drift).
    #[arg(long)]
    force: bool,
    /// Confirms a protected org without a prompt; must be the target's Org Id.
    #[arg(long, value_name = "ORG_ID")]
    confirm_org: Option<String>,
    /// Only the components with files under these paths; without paths, --metadata and
    /// --metadata-dir, all packageDirectories of sfdx-project.json.
    #[arg(conflicts_with_all = ["metadata_dir", "quick"])]
    paths: Vec<PathBuf>,
    /// Only these components: Type, Type:Name or Type:Prefix* (repeat the flag). A child such
    /// as CustomField:Account.Foo__c deploys with its parent and no other children. Combines
    /// with paths.
    #[arg(long, value_name = "TYPE:NAME", add = crate::complete::metadata_types(), conflicts_with_all = ["metadata_dir", "quick"])]
    metadata: Vec<Selector>,
    /// A ready mdapi directory with package.xml instead of the project source.
    #[arg(long, conflicts_with = "quick", value_hint = clap::ValueHint::DirPath)]
    pub(crate) metadata_dir: Option<PathBuf>,
    /// Which tests run; by default as Salesforce decides: none in a sandbox, local tests in
    /// production.
    #[arg(long, value_enum)]
    test_level: Option<Level>,
    /// Test classes to run (repeat the flag or separate with commas); alone they mean
    /// --test-level RunSpecifiedTests.
    #[arg(long, value_delimiter = ',')]
    tests: Vec<String>,
    /// Validate only (checkOnly); prints a validation id for --quick.
    #[arg(long)]
    pub(crate) validate: bool,
    /// Quick deploy of a previously validated deploy.
    #[arg(long, value_name = "VALIDATION_ID")]
    pub(crate) quick: Option<String>,
    /// Allow destructiveChanges*.xml.
    #[arg(long)]
    allow_destructive: bool,
    /// Over the limits: deploy in several parts ordered by dependencies. Each part is its own
    /// deploy: a failure stops the rest, and the parts before it stay in the org. Tests run
    /// only in the parts with Apex.
    #[arg(long, conflicts_with_all = ["validate", "quick"])]
    pub(crate) split: bool,
    /// Build and check the package (or the parts with --split) without sending anything.
    #[arg(long, conflicts_with = "quick")]
    dry_run: bool,
    /// With --dry-run: save the built mdapi directory here, as it would go to the org; must not
    /// exist or be empty.
    #[arg(long, requires = "dry_run", conflicts_with_all = ["metadata_dir", "from"], value_hint = clap::ValueHint::DirPath)]
    output_dir: Option<PathBuf>,
    /// Do not commit the deploy history (.orgpunk/deployments) after a deploy to the master or an
    /// environment; the working tree must still be clean before it.
    #[arg(long)]
    no_git_commit: bool,
    /// Minutes to wait; on timeout exits with 4 and the deploy id.
    #[arg(long, default_value_t = 33)]
    wait: u64,
    /// Set by rollback and --from: where the package comes from.
    #[arg(skip)]
    pub(crate) origin: Option<Origin>,
}

/// Where a package built by another command comes from.
pub(crate) struct Origin {
    pub commit: String,
    /// A rollback: the id of the deploy it goes back to.
    pub rollback_to: Option<u64>,
}

/// Deploy flags of commands that build their own package (push, rollback). Each command has
/// its own `--force`: what it overrides differs.
#[derive(ClapArgs)]
pub struct PackageFlags {
    /// Confirms a protected org without a prompt; must be the target's Org Id.
    #[arg(long, value_name = "ORG_ID")]
    confirm_org: Option<String>,
    /// Allow deleting components.
    #[arg(long)]
    allow_destructive: bool,
    /// Which tests run; by default as Salesforce decides: none in a sandbox, local tests in
    /// production.
    #[arg(long, value_enum)]
    test_level: Option<Level>,
    /// Test classes to run (repeat the flag or separate with commas); alone they mean
    /// --test-level RunSpecifiedTests.
    #[arg(long, value_delimiter = ',')]
    tests: Vec<String>,
    /// Build and check the package without sending anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Do not commit the deploy history (.orgpunk/deployments) after a deploy to the master or an
    /// environment; the working tree must still be clean before it.
    #[arg(long)]
    no_git_commit: bool,
    /// Minutes to wait; on timeout exits with 4 and the deploy id.
    #[arg(long, default_value_t = 33)]
    wait: u64,
}

impl Args {
    /// A deploy of a ready package to an environment, as rollback and promote do it.
    pub(crate) fn package(
        dir: PathBuf,
        env: String,
        origin: Origin,
        force: bool,
        f: PackageFlags,
    ) -> Self {
        Self::ready(dir, None, Some(env), Some(origin), force, f)
    }

    /// A deploy of a ready package to an org or an environment.
    pub(crate) fn ready(
        dir: PathBuf,
        to: Option<String>,
        env: Option<String>,
        origin: Option<Origin>,
        force: bool,
        f: PackageFlags,
    ) -> Self {
        Self {
            to,
            env,
            from: None,
            force,
            confirm_org: f.confirm_org,
            paths: Vec::new(),
            metadata: Vec::new(),
            metadata_dir: Some(dir),
            test_level: f.test_level,
            tests: f.tests,
            validate: false,
            quick: None,
            allow_destructive: f.allow_destructive,
            split: false,
            dry_run: f.dry_run,
            output_dir: None,
            no_git_commit: f.no_git_commit,
            wait: f.wait,
            origin,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Level {
    #[value(name = "NoTestRun")]
    NoTestRun,
    #[value(name = "RunSpecifiedTests")]
    RunSpecifiedTests,
    #[value(name = "RunLocalTests")]
    RunLocalTests,
    #[value(name = "RunAllTestsInOrg")]
    RunAllTestsInOrg,
}

impl From<Level> for TestLevel {
    fn from(l: Level) -> Self {
        match l {
            Level::NoTestRun => Self::NoTestRun,
            Level::RunSpecifiedTests => Self::RunSpecifiedTests,
            Level::RunLocalTests => Self::RunLocalTests,
            Level::RunAllTestsInOrg => Self::RunAllTestsInOrg,
        }
    }
}

/// What to deploy, known before talking to the org.
enum Work {
    Quick(String),
    Zip(String, Package),
    Parts(Plan),
}

pub fn run(ctx: &Ctx, out: &Output, a: Args) -> Result<Done, Failure> {
    let test_level = match (a.test_level, a.tests.is_empty()) {
        (None, false) => Some(TestLevel::RunSpecifiedTests),
        (Some(Level::RunSpecifiedTests), true) => {
            return Err(Failure::error("RunSpecifiedTests requires --tests"));
        }
        (Some(l), false) if !matches!(l, Level::RunSpecifiedTests) => {
            return Err(Failure::error("--tests only works with RunSpecifiedTests"));
        }
        (l, _) => l.map(Into::into),
    };
    let ci = ctx.ci;
    let root = ctx.config.project_root().map(Path::to_owned);
    let rollback_to = a.origin.as_ref().and_then(|o| o.rollback_to);

    // Target and protect first: a refusal costs milliseconds and builds nothing. A dry run
    // only builds the package and needs no org.
    let target = if a.dry_run {
        None
    } else {
        // An opk project deploys only once its master (and so its protect) is chosen.
        if let Some(root) = root
            .as_deref()
            .filter(|r| r.join(".orgpunk/config.toml").is_file())
        {
            if ProjectConfig::load(root)?.master().is_none() {
                return Err(Failure::new(
                    Exit::Refused,
                    "no master environment in .orgpunk/config.toml: deploy and push stay off \
                     until `opk env add <name> <alias> --master`",
                ));
            }
        }
        let t = resolve_target(ctx, &a)?;
        let guard = protect(&a, &t, ci, root.as_deref())?;
        // A tracked deploy is a commit: the history names it and opk commits the history.
        if let Some(root) = root.as_deref().filter(|_| tracked(&guard, &a)) {
            require_clean(root)?;
        }
        Some((t, guard))
    };

    // Packaging needs no org: conversion, limits, deletions and the split fail before any login.
    // `_source` keeps the converted directory alive until the deploy ends.
    let mut _source = None;
    let mut pkg_dir: Option<PathBuf> = None;
    let work = if let Some(id) = a.quick.clone() {
        Work::Quick(id)
    } else {
        let (dir, label) = match a.metadata_dir.clone() {
            Some(dir) => {
                let label = match &a.origin {
                    Some(o) => format!("commit {}", o.commit.get(..12).unwrap_or(&o.commit)),
                    None => dir.display().to_string(),
                };
                (dir, label)
            }
            None => {
                let selection = Selection {
                    paths: a.paths.clone(),
                    metadata: a.metadata.clone(),
                };
                if let Some(o) = &a.output_dir {
                    check_empty(o)?;
                }
                let build = build_source(ctx, out, &selection)?;
                let label = format!("source ({} components)", build.components);
                let dir = build.dir.path().to_owned();
                // Saved before the limits, so a package over them can be looked at too.
                if let Some(o) = &a.output_dir {
                    copy_dir(&dir, o)?;
                    out.progress(format!("Package saved to {}", o.display()));
                }
                _source = Some(build);
                (dir, label)
            }
        };
        pkg_dir = Some(dir.clone());
        if a.split {
            let plan = split::plan(&dir, Limits::default(), a.allow_destructive)?;
            show_plan(out, &label, &plan);
            Work::Parts(plan)
        } else {
            let pkg =
                orgpunk_deploy::package_mdapi_dir(&dir, Limits::default(), a.allow_destructive)
                    .map_err(split_hint)?;
            Work::Zip(label, pkg)
        }
    };
    let Some((
        Target {
            access,
            alias,
            conn,
        },
        guard,
    )) = target
    else {
        return Ok(Done::ok(dry_run(out, &work)));
    };

    let destructive: Vec<String> = match &work {
        Work::Quick(_) => Vec::new(),
        Work::Zip(_, pkg) => pkg.destructive.clone(),
        Work::Parts(plan) => plan.destructive_files(),
    };
    check_destructive(&guard.rules, &destructive).map_err(refused)?;
    if guard.rules.is_protected() {
        out.progress(format!("Protected: {}", guard.rules.describe()));
    }
    if guard.decision.ask_confirmation {
        confirm(&alias, &conn.org_id)?;
    }

    let client = match access {
        Access::Stored(auth) => ctx.client(&auth, &alias)?.0,
        Access::Ci(client) => client,
    };
    let target = json!({ "alias": alias, "orgId": conn.org_id });
    let tracker = match &root {
        Some(root) if tracked(&guard, &a) => Some(Tracked {
            tracker: start_tracking(out, &client, root, &conn, &a, pkg_dir.as_deref())?,
            git: (!a.no_git_commit).then(|| HistoryCommit {
                root: root.clone(),
                org_id: conn.org_id.clone(),
                label: a.env.clone().unwrap_or_else(|| alias.clone()),
            }),
        }),
        _ => None,
    };
    let deadline = Instant::now() + Duration::from_secs(a.wait * 60);

    let (zip, options, label) = match work {
        Work::Quick(validated_id) => {
            out.progress(format!(
                "Quick deploy {validated_id} → {alias} ({})",
                conn.org_id
            ));
            let (id, outcome) =
                start_and_wait(out, &client, Request::Quick { validated_id }, deadline)?;
            let mut done = finish(out, id.clone(), outcome, false, target);
            record(
                out,
                &client,
                tracker,
                &mut done,
                vec![id],
                false,
                rollback_to,
            )?;
            return Ok(done);
        }
        Work::Parts(plan) => {
            orgpunk_deploy::check_permissions(&client, &conn.user_id, &conn.username)?;
            out.progress(format!("Deploying → {alias} ({})", conn.org_id));
            let (mut done, deployed) =
                deploy_parts(out, &client, &plan, test_level, &a.tests, deadline, target)?;
            let partial = done.code != Exit::Ok;
            if !deployed.is_empty() {
                record(
                    out,
                    &client,
                    tracker,
                    &mut done,
                    deployed,
                    partial,
                    rollback_to,
                )?;
            }
            return Ok(done);
        }
        Work::Zip(label, pkg) => {
            let options = DeployOptions {
                check_only: a.validate || guard.decision.validate_first,
                test_level,
                run_tests: a.tests.clone(),
                ..Default::default()
            };
            (pkg.zip, options, (label, pkg.files))
        }
    };
    orgpunk_deploy::check_permissions(&client, &conn.user_id, &conn.username)?;
    out.progress(format!(
        "{} {} → {alias} ({}): {} files, zip {}",
        if options.check_only {
            "Validating"
        } else {
            "Deploying"
        },
        label.0,
        conn.org_id,
        label.1,
        kb(zip.len())
    ));
    // validate-first may have to send the validated package again.
    let kept = guard.decision.validate_first.then(|| zip.clone());
    let request = Request::Zip { zip, options };
    let (id, outcome) = start_and_wait(out, &client, request, deadline)?;
    let validated = matches!(&outcome, Outcome::Done(r) if r.status.is_success());
    if (a.validate || guard.decision.validate_first) && validated {
        if let Some(root) = &root {
            let v = Validation {
                id: id.clone(),
                org_id: conn.org_id.clone(),
                commit: clean_head(root),
                at: orgpunk_deploy::validations::now(),
            };
            orgpunk_deploy::validations::record(root, v)?;
        }
    }
    if !guard.decision.validate_first || !validated {
        let check_only = a.validate || guard.decision.validate_first;
        let mut done = finish(out, id.clone(), outcome, check_only, target);
        if guard.decision.validate_first && done.code == Exit::DeployFailed {
            out.say("Validation failed: nothing was deployed (protect `validate-first`).");
        }
        if !check_only && done.code == Exit::Ok {
            record(
                out,
                &client,
                tracker,
                &mut done,
                vec![id],
                false,
                rollback_to,
            )?;
        }
        return Ok(done);
    }

    // validate-first: deploy exactly what was validated.
    if let Outcome::Done(r) = &outcome {
        report(out, r, true);
    }
    out.progress(format!("Validated {id}; quick deploy → {alias}"));
    let quick = start_and_wait(
        out,
        &client,
        Request::Quick {
            validated_id: id.clone(),
        },
        deadline,
    );
    let (deploy_id, outcome) = match quick {
        Ok(r) => r,
        // Salesforce takes a quick deploy only for validations that ran tests; the package
        // is the one just validated, so deploy it as is.
        Err(e) => {
            out.progress(format!(
                "Quick deploy not accepted ({}); deploying the validated package",
                e.message
            ));
            let options = DeployOptions {
                test_level,
                run_tests: a.tests,
                ..Default::default()
            };
            let zip = kept.expect("kept for validate-first");
            start_and_wait(out, &client, Request::Zip { zip, options }, deadline)?
        }
    };
    let mut done = finish(out, deploy_id.clone(), outcome, false, target);
    done.json["validationId"] = json!(id);
    if done.code == Exit::Ok {
        record(
            out,
            &client,
            tracker,
            &mut done,
            vec![deploy_id],
            false,
            rollback_to,
        )?;
    }
    Ok(done)
}

/// Lists what the package touches in the org and refuses on drift unless `--force`.
fn start_tracking(
    out: &Output,
    client: &Client,
    root: &Path,
    conn: &Org,
    a: &Args,
    pkg_dir: Option<&Path>,
) -> Result<Tracker, Failure> {
    let members = match pkg_dir {
        Some(dir) => {
            let path = dir.join("package.xml");
            let text = std::fs::read_to_string(&path)
                .map_err(|e| Failure::error(format!("{}: {e}", path.display())))?;
            let manifest = Manifest::parse(&text)
                .map_err(|e| Failure::error(format!("{}: {e}", path.display())))?;
            history::members(&manifest)
        }
        None => Vec::new(),
    };
    // The commit the package comes from: a ready mdapi directory has none, a quick deploy
    // has its validation's.
    let commit = if let Some(o) = &a.origin {
        Some(o.commit.clone())
    } else if a.metadata_dir.is_some() {
        None
    } else if let Some(id) = &a.quick {
        orgpunk_deploy::validations::find(root, id)?.and_then(|v| v.commit)
    } else {
        clean_head(root)
    };
    if !members.is_empty() {
        out.progress(format!("Checking {} components in the org…", members.len()));
    }
    let (tracker, drifted) =
        Tracker::start(client, root, &conn.org_id, &conn.username, members, commit)?;
    if !drifted.is_empty() {
        let shown: Vec<&str> = drifted.iter().take(10).map(String::as_str).collect();
        let more = drifted.len().saturating_sub(shown.len());
        let list = format!(
            "{}{}",
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        );
        if !a.force {
            return Err(Failure::new(
                Exit::Refused,
                format!(
                    "changed in the org since opk last deployed them: {list}. To keep the org's \
                     versions: `opk pull -o <alias>`, commit, then `opk env sync {env}`; to \
                     overwrite them: --force",
                    env = a.env.as_deref().unwrap_or("<env>")
                ),
            ));
        }
        out.progress(format!(
            "Warning: deploying over changes made in the org: {list}"
        ));
    }
    Ok(tracker)
}

/// A deploy to an org with a history: the master or an environment. A validation changes
/// nothing in the org, so it is not tracked.
fn tracked(guard: &Guard, a: &Args) -> bool {
    !guard.rules.sources.is_empty() && !a.validate
}

/// A tracked deploy starts from a clean working tree, so the history names the commit that
/// went to the org and the history file can be committed on its own.
pub(crate) fn require_clean(root: &Path) -> Result<(), Failure> {
    let repo = orgpunk_repo::state(root)?;
    if repo.is_clean() {
        return Ok(());
    }
    let shown: Vec<&str> = repo.dirty.iter().take(5).map(String::as_str).collect();
    let more = repo.dirty.len().saturating_sub(shown.len());
    Err(Failure::new(
        Exit::Refused,
        format!(
            "the working tree has uncommitted changes ({}{}); an environment's history \
             changes only from a commit: commit or stash them first",
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        ),
    ))
}

/// The history of a tracked deploy and where to commit it.
struct Tracked {
    tracker: Tracker,
    /// `None` with --no-git-commit.
    git: Option<HistoryCommit>,
}

struct HistoryCommit {
    root: PathBuf,
    org_id: String,
    /// The environment or the alias, for the commit message.
    label: String,
}

impl HistoryCommit {
    /// Commits the history file; a failure only warns: the deploy itself is done.
    fn commit(&self, out: &Output, id: u64, rollback_to: Option<u64>) -> Option<String> {
        let message = match rollback_to {
            Some(to) => format!("chore: opk rollback {} #{id} to #{to}", self.label),
            None => format!("chore: opk deploy {} #{id}", self.label),
        };
        commit_history(out, &self.root, &self.org_id, &message)
    }
}

/// Commits an org's history file alone; a failure only warns: what it records is done.
pub(crate) fn commit_history(
    out: &Output,
    root: &Path,
    org_id: &str,
    message: &str,
) -> Option<String> {
    let file = history::path(root, org_id);
    let rel = file
        .strip_prefix(root)
        .unwrap_or(&file)
        .to_string_lossy()
        .replace('\\', "/");
    match orgpunk_repo::commit(root, std::slice::from_ref(&rel), message) {
        Ok(Some(c)) => {
            out.progress(format!("Committed {rel}: {}", &c[..12.min(c.len())]));
            Some(c)
        }
        Ok(None) => None,
        Err(e) => {
            out.progress(format!(
                "Warning: the history was not committed ({e}); commit {rel} yourself"
            ));
            None
        }
    }
}

/// Appends the deploy to the org's history and commits it.
fn record(
    out: &Output,
    client: &Client,
    tracker: Option<Tracked>,
    done: &mut Done,
    deploy_ids: Vec<String>,
    partial: bool,
    rollback_to: Option<u64>,
) -> Result<(), Failure> {
    let Some(Tracked { tracker, git }) = tracker else {
        return Ok(());
    };
    let entry = tracker.finish(client, deploy_ids, partial, rollback_to)?;
    out.progress(format!(
        "History: deploy #{} ({} created, {} changed{})",
        entry.id,
        entry.created.len(),
        entry.changed.len(),
        if entry.commit.is_none() {
            "; no commit, cannot be rolled back to"
        } else {
            ""
        }
    ));
    done.json["history"] =
        json!({ "id": entry.id, "created": entry.created, "changed": entry.changed });
    if let Some(git) = &git {
        done.json["history"]["commit"] = json!(git.commit(out, entry.id, rollback_to));
    }
    Ok(())
}

/// The org a deploy goes to: from a stored connection (no network yet) or a CI login.
struct Target {
    access: Access,
    alias: String,
    conn: Org,
}

enum Access {
    Stored(Auth),
    Ci(Client),
}

struct Org {
    org_id: String,
    user_id: String,
    username: String,
}

/// What protect decided for the target.
struct Guard {
    rules: Rules,
    decision: Decision,
}

/// `--env` (its org from .orgpunk/config.toml), else `--to` or target-org. In CI mode the org is
/// the one the environment logs in to, and it must be the one asked for.
fn resolve_target(ctx: &Ctx, a: &Args) -> Result<Target, Failure> {
    let env_org = match &a.env {
        Some(name) => {
            let config = project_config(ctx)?;
            let env = config.envs.get(name).ok_or_else(|| {
                Failure::error(format!(
                    "no environment `{name}` in .orgpunk/config.toml; see `opk env list`"
                ))
            })?;
            Some((name.as_str(), env.org.clone()))
        }
        None => None,
    };
    if ctx.ci {
        let s = ctx.ci_session()?;
        let wrong_org = |wanted: &str| {
            Failure::new(
                Exit::Refused,
                format!(
                    "the CI credentials log in to {} ({}), not {wanted}; check the secrets",
                    s.org_id, s.username
                ),
            )
        };
        if let Some((name, org)) = &env_org {
            if !same_org(org, &s.org_id) {
                return Err(wrong_org(&format!("{org} (env.{name})")));
            }
        }
        if let Some(to) = &a.to {
            if !same_org(to, &s.org_id) && *to != s.username {
                return Err(wrong_org(to));
            }
        }
        return Ok(Target {
            alias: s.username.clone(),
            conn: Org {
                org_id: s.org_id,
                user_id: s.user_id,
                username: s.username,
            },
            access: Access::Ci(s.client),
        });
    }
    let auth = ctx.auth()?;
    let alias = match &env_org {
        Some((name, org)) => auth
            .connections
            .resolve(org)
            .map(str::to_owned)
            .ok_or_else(|| {
                Failure::error(format!(
                    "no connection to {org} (env.{name}); run `opk org login` for it"
                ))
            })?,
        None => ctx.resolve_alias(&auth, a.to.as_deref())?,
    };
    let c: &Connection = auth.connection(&alias)?;
    let conn = Org {
        org_id: c.org_id.clone(),
        user_id: c.user_id.clone(),
        username: c.username.clone(),
    };
    Ok(Target {
        access: Access::Stored(auth),
        alias,
        conn,
    })
}

fn project_config(ctx: &Ctx) -> Result<ProjectConfig, Failure> {
    match ctx.config.project_root() {
        Some(root) => Ok(ProjectConfig::load(root)?),
        None => Ok(ProjectConfig::default()),
    }
}

/// Protect of the target org, before anything is built.
fn protect(a: &Args, t: &Target, ci: bool, root: Option<&Path>) -> Result<Guard, Failure> {
    let config = match root {
        Some(r) => ProjectConfig::load(r)?,
        None => ProjectConfig::default(),
    };
    let rules = Rules::for_org(&config, &t.conn.org_id);
    let repo_dir = root.map_or_else(|| PathBuf::from("."), Path::to_owned);
    let req = orgpunk_env::protect::Request {
        org_id: &t.conn.org_id,
        alias: &t.alias,
        interactive: !ci && std::io::stdin().is_terminal(),
        confirm_org: a.confirm_org.as_deref(),
        quick: a.quick.is_some(),
    };
    let decision = check(&rules, &req, || {
        orgpunk_repo::state(&repo_dir).map_err(|e| e.to_string())
    })
    .map_err(refused)?;
    if decision.validate_first && a.split {
        return Err(refused(Refusal {
            policy: Policy::ValidateFirst,
            reason: "a --split deploy cannot be validated first; deploy without --split".into(),
        }));
    }
    if let (true, Some(id)) = (rules.has(Policy::ValidateFirst), &a.quick) {
        check_validation(id, &t.conn.org_id, root)?;
    }
    Ok(Guard { rules, decision })
}

/// validate-first and `--quick`: the validation must be ours, of this org and of HEAD.
fn check_validation(id: &str, org_id: &str, root: Option<&Path>) -> Result<(), Failure> {
    let refuse = |reason: String| {
        refused(Refusal {
            policy: Policy::ValidateFirst,
            reason,
        })
    };
    let Some(root) = root else {
        return Err(refuse(
            "--quick needs the project its validation came from".into(),
        ));
    };
    let Some(v) = orgpunk_deploy::validations::find(root, id)? else {
        return Err(refuse(format!(
            "{id} is not a validation of this project; run `opk env deploy <env> --validate` first"
        )));
    };
    if !same_org(&v.org_id, org_id) {
        return Err(refuse(format!("{id} validated {}, not {org_id}", v.org_id)));
    }
    match (&v.commit, clean_head(root)) {
        (Some(validated), Some(head)) if *validated == head => Ok(()),
        (None, _) => Err(refuse(format!(
            "{id} was validated from uncommitted changes; validate a commit"
        ))),
        _ => Err(refuse(format!(
            "{id} validated another commit than the clean HEAD; validate again"
        ))),
    }
}

/// HEAD when the working tree is clean: the commit a package built now comes from.
fn clean_head(root: &Path) -> Option<String> {
    orgpunk_repo::state(root)
        .ok()
        .filter(orgpunk_repo::State::is_clean)
        .and_then(|s| s.head)
}

fn refused(r: Refusal) -> Failure {
    Failure::new(Exit::Refused, r.to_string())
}

/// `confirm`: the org alias (or its Org Id) typed on the terminal.
fn confirm(alias: &str, org_id: &str) -> Result<(), Failure> {
    eprint!("Protected org {alias} ({org_id}). Type its alias to deploy: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| Failure::error(format!("reading the confirmation: {e}")))?;
    let typed = line.trim();
    if typed == alias || same_org(typed, org_id) {
        Ok(())
    } else {
        Err(refused(Refusal {
            policy: Policy::Confirm,
            reason: format!("typed `{typed}`, expected `{alias}`; nothing was sent"),
        }))
    }
}

/// The result of a single deploy as the command's output.
fn finish(
    out: &Output,
    id: String,
    outcome: Outcome,
    validate: bool,
    target: serde_json::Value,
) -> Done {
    let (code, result) = match outcome {
        Outcome::TimedOut(r) => {
            out.say(format!(
                "Timed out waiting; deploy {id} is still running ({:?}). See Setup → Deployment Status.",
                r.status
            ));
            (Exit::InProgress, r)
        }
        Outcome::Done(r) => {
            report(out, &r, validate);
            let code = if r.status.is_success() {
                Exit::Ok
            } else {
                Exit::DeployFailed
            };
            (code, r)
        }
    };
    let mut json = serde_json::to_value(&result).expect("DeployResult serializes");
    json["targetOrg"] = target;
    if validate && code == Exit::Ok {
        json["validationId"] = json!(id);
    }
    Done { code, json }
}

/// Converts the project source (or the selected components) into an mdapi directory.
fn build_source(ctx: &Ctx, out: &Output, selection: &Selection) -> Result<SourceBuild, Failure> {
    let root = ctx.config.project_root().ok_or_else(|| {
        Failure::error(
            "not inside an sfdx project (no sfdx-project.json above); pass --metadata-dir",
        )
    })?;
    let project = SfdxProject::load(root)?;
    let fallback = ctx.config.get("api-version")?.map(|r| r.value);
    let build = orgpunk_deploy::source::build(&project, selection, fallback.as_deref())?;
    for w in &build.warnings {
        out.progress(format!("Warning: {w}"));
    }
    Ok(build)
}

fn check_empty(dir: &Path) -> Result<(), Failure> {
    if std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_some()) {
        return Err(Failure::error(format!(
            "{} is not empty; pick a new or empty directory",
            dir.display()
        )));
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), Failure> {
    let io = |p: &Path, e: std::io::Error| Failure::error(format!("{}: {e}", p.display()));
    std::fs::create_dir_all(to).map_err(|e| io(to, e))?;
    for entry in std::fs::read_dir(from).map_err(|e| io(from, e))? {
        let entry = entry.map_err(|e| io(from, e))?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().map_err(|e| io(&src, e))?.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst).map_err(|e| io(&src, e))?;
        }
    }
    Ok(())
}

/// Over a limit without `--split`: say how to get past it.
fn split_hint(e: orgpunk_deploy::Error) -> Failure {
    let hint = matches!(e, orgpunk_deploy::Error::Limit { .. });
    let mut f = Failure::from(e);
    if hint {
        f.message += "; --split deploys it in several parts ordered by dependencies";
    }
    f
}

fn show_plan(out: &Output, label: &str, plan: &Plan) {
    let components: usize = plan.parts.iter().map(|p| p.components.len()).sum();
    out.progress(format!(
        "{}: {} files, {components} components → {} part{}",
        label,
        plan.files(),
        plan.parts.len(),
        if plan.parts.len() == 1 { "" } else { "s" }
    ));
    for (i, p) in plan.parts.iter().enumerate() {
        out.progress(format!(
            "  Part {}: {} files, {}, zip ~{} — {}",
            i + 1,
            p.files,
            mb(p.unzipped_bytes),
            mb(p.zip_bytes),
            plan.describe(i)
        ));
    }
}

fn dry_run(out: &Output, work: &Work) -> serde_json::Value {
    out.say("Dry run: nothing was sent.");
    match work {
        Work::Quick(_) => unreachable!("clap: --dry-run conflicts with --quick"),
        Work::Zip(label, pkg) => {
            out.say(format!(
                "{}: {} files, zip {}",
                label,
                pkg.files,
                kb(pkg.zip.len())
            ));
            json!({
                "dryRun": true,
                "files": pkg.files,
                "zipBytes": pkg.zip.len(),
                "unzippedBytes": pkg.unzipped_bytes,
            })
        }
        Work::Parts(plan) => {
            let parts: Vec<_> = plan
                .parts
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    json!({
                        "part": i + 1,
                        "files": p.files,
                        "unzippedBytes": p.unzipped_bytes,
                        "zipBytesEstimate": p.zip_bytes,
                        "hasApex": p.has_apex,
                        "components": plan.members(i),
                    })
                })
                .collect();
            json!({ "dryRun": true, "parts": parts })
        }
    }
}

/// Deploys the parts one by one; the first failure or timeout stops the rest.
fn deploy_parts(
    out: &Output,
    client: &Client,
    plan: &Plan,
    test_level: Option<TestLevel>,
    tests: &[String],
    deadline: Instant,
    target: serde_json::Value,
) -> Result<(Done, Vec<String>), Failure> {
    let n = plan.parts.len();
    let mut results = Vec::new();
    let mut deployed = Vec::new();
    let mut code = Exit::Ok;
    for (i, part) in plan.parts.iter().enumerate() {
        let pkg = plan.package(i)?;
        out.progress(format!(
            "\nPart {}/{n}: {} files, zip {} — {}",
            i + 1,
            pkg.files,
            kb(pkg.zip.len()),
            plan.describe(i)
        ));
        // Tests matter only where there is Apex; elsewhere the org's default applies.
        let options = DeployOptions {
            test_level: test_level.filter(|_| part.has_apex),
            run_tests: if part.has_apex {
                tests.to_vec()
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        let request = Request::Zip {
            zip: pkg.zip,
            options,
        };
        let (id, outcome) = start_and_wait(out, client, request, deadline)?;
        let result = match outcome {
            Outcome::TimedOut(r) => {
                out.say(format!(
                    "Timed out waiting; part {}/{n} (deploy {id}) is still running ({:?}). {}",
                    i + 1,
                    r.status,
                    applied(i, n)
                ));
                code = Exit::InProgress;
                r
            }
            Outcome::Done(r) if r.status.is_success() => {
                deployed.push(id.clone());
                out.say(format!(
                    "Part {}/{n} succeeded: components {}/{} · tests {}/{}",
                    i + 1,
                    r.number_components_deployed,
                    r.number_components_total,
                    r.number_tests_completed,
                    r.number_tests_total
                ));
                r
            }
            Outcome::Done(r) => {
                out.say(format!("\nPart {}/{n} failed.", i + 1));
                report(out, &r, false);
                out.say(format!("\n{}", applied(i, n)));
                code = Exit::DeployFailed;
                r
            }
        };
        results.push(json!({ "part": i + 1, "deployId": id, "result": result }));
        if code != Exit::Ok {
            break;
        }
    }
    if code == Exit::Ok {
        out.say(format!("\nAll {n} parts deployed."));
    }
    Ok((
        Done {
            code,
            json: json!({ "parts": results, "partsTotal": n, "targetOrg": target }),
        },
        deployed,
    ))
}

/// What is in the org after part `i` (0-based) of `n` did not succeed.
fn applied(i: usize, n: usize) -> String {
    let mut s = match i {
        0 => "No parts were deployed.".to_owned(),
        1 => "Part 1 is deployed and stays in the org.".to_owned(),
        _ => format!("Parts 1–{i} are deployed and stay in the org."),
    };
    match n - i - 1 {
        0 => {}
        1 => s += &format!(" Part {n} was not sent."),
        _ => s += &format!(" Parts {}–{n} were not sent.", i + 2),
    }
    s
}

fn start_and_wait(
    out: &Output,
    client: &Client,
    request: Request,
    deadline: Instant,
) -> Result<(String, Outcome), Failure> {
    let id = orgpunk_deploy::start(client, request)?;
    out.progress(format!("Deploy id: {id}"));
    let mut last = None;
    let timeout = deadline.saturating_duration_since(Instant::now());
    let outcome = orgpunk_deploy::wait(client, &id, timeout, |r| {
        let line = progress_line(r);
        if last.as_ref() != Some(&line) {
            out.progress(&line);
            last = Some(line);
        }
    })?;
    Ok((id, outcome))
}

fn kb(bytes: usize) -> String {
    format!("{:.1} KB", bytes as f64 / 1024.0)
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn progress_line(r: &DeployResult) -> String {
    let mut s = format!("  {:?}", r.status);
    if r.number_components_total > 0 {
        s += &format!(
            " · components {}/{}",
            r.number_components_deployed + r.number_component_errors,
            r.number_components_total
        );
    }
    if r.number_tests_total > 0 {
        s += &format!(
            " · tests {}/{}",
            r.number_tests_completed + r.number_test_errors,
            r.number_tests_total
        );
    }
    if let Some(d) = r.state_detail.as_deref().filter(|d| !d.is_empty()) {
        s += &format!(" · {d}");
    }
    s
}

fn report(out: &Output, r: &DeployResult, validate: bool) {
    out.say(format!(
        "{:?}: components {}/{}, errors {} · tests {}/{}, failed {}",
        r.status,
        r.number_components_deployed,
        r.number_components_total,
        r.number_component_errors,
        r.number_tests_completed,
        r.number_tests_total,
        r.number_test_errors
    ));
    if let Some(msg) = &r.error_message {
        out.say(format!("Deploy error: {msg}"));
    }
    let details = r.details.clone().unwrap_or_default();
    let failures: Vec<_> = details
        .component_failures
        .iter()
        .filter(|f| f.problem.is_some())
        .collect();
    if !failures.is_empty() {
        out.say("\nComponent failures:");
        for f in failures {
            let pos = match (f.line_number, f.column_number) {
                (Some(l), Some(c)) => format!(":{l}:{c}"),
                (Some(l), None) => format!(":{l}"),
                _ => String::new(),
            };
            out.say(format!(
                "  {}{pos}  {} {} — {}",
                f.file_name,
                f.component_type,
                f.full_name,
                f.problem.as_deref().unwrap_or_default()
            ));
        }
    }
    if let Some(tests) = &details.run_test_result {
        if !tests.failures.is_empty() {
            out.say("\nTest failures:");
            for t in &tests.failures {
                out.say(format!("  {}.{} — {}", t.name, t.method_name, t.message));
                if let Some(st) = t.stack_trace.as_deref().filter(|s| !s.is_empty()) {
                    out.say(format!("      {st}"));
                }
            }
        }
        if !tests.code_coverage_warnings.is_empty() {
            out.say("\nCode coverage warnings:");
            for w in &tests.code_coverage_warnings {
                out.say(format!(
                    "  {}{}",
                    w.name
                        .as_deref()
                        .map(|n| format!("{n}: "))
                        .unwrap_or_default(),
                    w.message
                ));
            }
        }
    }
    if validate && r.status == DeployStatus::Succeeded {
        out.say(format!(
            "\nValidation id: {} (deploy it: the same command with `--quick {}`)",
            r.id, r.id
        ));
    }
}
