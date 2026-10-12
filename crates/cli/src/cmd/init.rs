//! `opk init [<dir>]`: a new project or opk added to an existing one, and a git repository
//! with a first commit. Running it again creates only what is missing. The master is an
//! environment: `opk env add <name> <alias> --master`. With `--from-mdapi` an mdapi directory
//! (an old metadata-format repository) becomes the project's source.

use std::path::{Path, PathBuf};

use clap::Args as ClapArgs;
use orgpunk_config::project::ProjectConfig;
use orgpunk_config::sfdx_project::SfdxProject;
use orgpunk_convert::manifest::Manifest;
use orgpunk_init::Mode;
use orgpunk_sfapi::DEFAULT_API_VERSION;
use serde_json::json;

use crate::context::Ctx;
use crate::output::{Done, Failure, Output};

#[derive(ClapArgs)]
pub struct Args {
    /// Project directory; the current one by default.
    dir: Option<PathBuf>,
    /// Scaffold into a non-empty directory without sfdx-project.json; existing files stay.
    #[arg(long)]
    force: bool,
    /// Do not make the first commit.
    #[arg(long)]
    no_commit: bool,
    /// Convert this mdapi directory (package.xml and metadata files) into the project's source,
    /// in the default package directory; a non-empty directory needs no --force then.
    #[arg(long, value_name = "DIR", value_hint = clap::ValueHint::DirPath)]
    from_mdapi: Option<PathBuf>,
}

pub fn run(ctx: &Ctx, out: &Output, a: Args) -> Result<Done, Failure> {
    let dir = match &a.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir().map_err(|e| Failure::error(e.to_string()))?,
    };
    // Converting an mdapi repository in place is what --from-mdapi is for: no --force needed.
    let mode = orgpunk_init::mode(&dir, a.force || a.from_mdapi.is_some())?;
    let mdapi = match &a.from_mdapi {
        Some(d) => Some(read_mdapi(d)?),
        None => None,
    };

    // sourceApiVersion of a new project: the mdapi package's, else target-org's newest, else
    // what opk supports.
    let api_version = if dir.join("sfdx-project.json").exists() {
        DEFAULT_API_VERSION.to_owned()
    } else if let Some(v) = mdapi.as_ref().and_then(|m| m.version.clone()) {
        v
    } else {
        target_api_version(ctx, out)
    };

    let changed = orgpunk_init::scaffold(&dir, mode, &api_version)?;
    let converted = match &mdapi {
        Some(m) => {
            let written = write_source(&dir, m)?;
            out.progress(format!(
                "Converted {} file(s) from {} into source; the mdapi directory stays, delete it \
                 once the source is checked",
                written.len(),
                a.from_mdapi.as_deref().unwrap_or(Path::new(".")).display()
            ));
            written.len()
        }
        None => 0,
    };

    // git: a repository on main; the first commit only for a repository opk created.
    let created_repo = orgpunk_repo::init(&dir)?;
    let mut commit = None;
    if created_repo && !a.no_commit {
        match orgpunk_repo::first_commit(&dir, "chore: opk init") {
            Ok(id) => commit = Some(id),
            Err(orgpunk_repo::Error::NoIdentity) => out.progress(
                "Warning: no first commit: git has no user.name / user.email; set them and commit",
            ),
            Err(e) => return Err(e.into()),
        }
    }

    let label = match mode {
        Mode::New => "Created project",
        Mode::Existing => "opk set up in",
        Mode::Forced => "Scaffolded",
    };
    out.say(format!("{label} {}", dir.display()));
    for c in &changed {
        out.say(format!("  + {c}"));
    }
    match (&commit, created_repo) {
        (Some(id), _) => out.say(format!(
            "Git repository on main, first commit {}",
            &id[..12]
        )),
        (None, true) => out.say("Git repository on main"),
        (None, false) if !changed.is_empty() => out.say("Commit the changes with your other work."),
        _ => {}
    }
    if changed.is_empty() && converted == 0 {
        out.say("Nothing to do: the project is set up.");
    }
    let master = ProjectConfig::load(&dir)?.master().map(|m| m.name.clone());
    if master.is_none() {
        out.progress(
            "No master org yet: deploy and push stay off until `opk env add <name> <alias> --master`.",
        );
    }
    Ok(Done::ok(json!({
        "dir": dir,
        "mode": format!("{mode:?}").to_lowercase(),
        "changed": changed,
        "gitInit": created_repo,
        "commit": commit,
        "master": master,
        "converted": converted,
    })))
}

/// The newest API version of target-org; opk's default without one or on an error.
fn target_api_version(ctx: &Ctx, out: &Output) -> String {
    let Ok(Some(target)) = ctx.config.get("target-org") else {
        return DEFAULT_API_VERSION.to_owned();
    };
    let version = ctx.auth().and_then(|auth| {
        let alias = ctx.resolve_alias(&auth, Some(&target.value))?;
        let (client, _) = ctx.client(&auth, &alias)?;
        client.latest_api_version().map_err(Into::into)
    });
    version.unwrap_or_else(|e| {
        out.progress(format!(
            "Warning: API version of {} unknown ({}); using {DEFAULT_API_VERSION}",
            target.value, e.message
        ));
        DEFAULT_API_VERSION.to_owned()
    })
}

/// An mdapi directory converted to source, and its package version.
struct Mdapi {
    version: Option<String>,
    tree: orgpunk_convert::to_source::SourceTree,
}

fn read_mdapi(dir: &Path) -> Result<Mdapi, Failure> {
    let manifest = dir.join("package.xml");
    let text = std::fs::read_to_string(&manifest).map_err(|e| {
        Failure::error(format!(
            "{}: {e}; --from-mdapi takes a directory with package.xml",
            manifest.display()
        ))
    })?;
    let version = Manifest::parse(&text)
        .map_err(|e| Failure::error(format!("{}: {e}", manifest.display())))?
        .version;
    let files = orgpunk_convert::to_source::read_dir(dir)?;
    let tree = orgpunk_convert::to_source::convert(&files)?;
    if tree.files.is_empty() {
        return Err(Failure::error(format!(
            "{} has no metadata files to convert",
            dir.display()
        )));
    }
    Ok(Mdapi { version, tree })
}

/// Writes the converted source into `<default package directory>/main/default`. A file that
/// exists with other content stops everything before anything is written; the same content is
/// left as it is. Returns the paths written, relative to the project.
fn write_source(root: &Path, m: &Mdapi) -> Result<Vec<String>, Failure> {
    let project = SfdxProject::load(root)?;
    let package = project
        .package_directories
        .iter()
        .find(|p| p.default)
        .or_else(|| project.package_directories.first())
        .map(|p| p.path.trim_end_matches('/').to_owned())
        .ok_or_else(|| Failure::error("sfdx-project.json has no packageDirectories"))?;
    let base = format!("{package}/main/default");
    let io = |p: &Path, e: std::io::Error| Failure::error(format!("{}: {e}", p.display()));

    let mut clashes = Vec::new();
    let mut write = Vec::new();
    for (rel, bytes) in &m.tree.files {
        let target = format!("{base}/{rel}");
        match std::fs::read(root.join(&target)) {
            Ok(old) if old == *bytes => {}
            Ok(_) => clashes.push(target),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => write.push((target, bytes)),
            Err(e) => return Err(io(&root.join(&target), e)),
        }
    }
    if !clashes.is_empty() {
        let more = clashes.len().saturating_sub(5);
        return Err(Failure::error(format!(
            "these files exist with other content: {}{}; nothing was converted",
            clashes[..clashes.len().min(5)].join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        )));
    }
    let mut written = Vec::new();
    for (target, bytes) in write {
        let path = root.join(&target);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        std::fs::write(&path, bytes).map_err(|e| io(&path, e))?;
        written.push(target);
    }
    Ok(written)
}
