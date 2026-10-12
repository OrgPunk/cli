//! The converter for the SDR parity check (`scripts/parity`); not part of the opk binary.
//!
//! ```text
//! parity_convert source <out>         project in the current directory -> mdapi <out>
//! parity_convert mdapi <mdapi> <out>  mdapi directory -> source <out>/main/default
//! ```
//!
//! Like `sf project convert`, replacements apply to source -> mdapi only with
//! `ORGPUNK_APPLY_REPLACEMENTS_ON_CONVERT=true` or `SF_APPLY_REPLACEMENTS_ON_CONVERT=true`.
//! `ORGPUNK_API_VERSION` is the package version of a project without sourceApiVersion.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use orgpunk_config::sfdx_project::SfdxProject;
use orgpunk_deploy::source::{Selection, convert_into};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["source", out] => to_mdapi(Path::new(out)),
        ["mdapi", mdapi, out] => to_source(Path::new(mdapi), Path::new(out)),
        _ => Err("usage: parity_convert source <out> | parity_convert mdapi <mdapi> <out>".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn to_mdapi(out: &Path) -> Result<()> {
    let project = SfdxProject::load(&std::env::current_dir()?)?;
    std::fs::create_dir_all(out)?;
    let replacements = [
        "ORGPUNK_APPLY_REPLACEMENTS_ON_CONVERT",
        "SF_APPLY_REPLACEMENTS_ON_CONVERT",
    ]
    .iter()
    .find_map(|k| std::env::var(k).ok())
    .is_some_and(|v| v == "true");
    let fallback = std::env::var("ORGPUNK_API_VERSION").ok();
    let done = convert_into(
        &project,
        &Selection::default(),
        fallback.as_deref(),
        replacements,
        out,
    )?;
    for w in &done.warnings {
        eprintln!("Warning: {w}");
    }
    Ok(())
}

fn to_source(mdapi: &Path, out: &Path) -> Result<()> {
    let files = orgpunk_convert::to_source::read_dir(mdapi)?;
    let tree = orgpunk_convert::to_source::convert(&files)?;
    let base: PathBuf = out.join("main").join("default");
    for (rel, bytes) in &tree.files {
        let path = base.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
    }
    Ok(())
}
