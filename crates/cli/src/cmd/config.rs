//! `opk config get / set / unset / list`.

use clap::Subcommand;
use orgpunk_config::{Resolved, Scope, Source};
use serde_json::{Value, json};

use crate::context::Ctx;
use crate::output::{Done, Failure, Output};

#[derive(Subcommand)]
pub enum Command {
    /// Show a variable's value and where it comes from.
    Get {
        /// The variable: target-org, api-version…
        #[arg(add = crate::complete::config_keys())]
        key: String,
    },
    /// Set a value (in the project or globally).
    Set {
        /// The variable: target-org, api-version…
        #[arg(add = crate::complete::config_keys())]
        key: String,
        /// Its value.
        value: String,
        /// In the global config (all projects) instead of this project.
        #[arg(long)]
        global: bool,
    },
    /// Remove a value.
    Unset {
        /// The variable.
        #[arg(add = crate::complete::config_keys())]
        key: String,
        /// From the global config instead of this project.
        #[arg(long)]
        global: bool,
    },
    /// List all variables that are set.
    List,
}

fn scope(global: bool) -> Scope {
    if global { Scope::Global } else { Scope::Local }
}

fn source(s: &Source) -> String {
    match s {
        Source::Env(name) => format!("env {name}"),
        Source::Local => "local".into(),
        Source::Global => "global".into(),
    }
}

fn row(r: &Resolved) -> Value {
    json!({ "key": r.key, "value": r.value, "source": source(&r.source) })
}

pub fn run(ctx: &Ctx, out: &Output, cmd: Command) -> Result<Done, Failure> {
    let c = &ctx.config;
    match cmd {
        Command::Get { key } => {
            let r = c.get(&key)?;
            match &r {
                Some(r) => out.say(format!("{} = {}  ({})", r.key, r.value, source(&r.source))),
                None => out.say(format!("{key} is not set")),
            }
            Ok(Done::ok(
                r.as_ref().map_or(json!({ "key": key, "value": null }), row),
            ))
        }
        Command::Set { key, value, global } => {
            c.set(scope(global), &key, &value)?;
            // Warn when the written value is shadowed by env or the local config.
            if let Some(r) = c.get(&key)?.filter(|r| r.value != value) {
                out.progress(format!(
                    "Warning: the effective value {key} = {} comes from {}",
                    r.value,
                    source(&r.source)
                ));
            }
            out.say(format!("{key} = {value}"));
            Ok(Done::ok(
                json!({ "key": key, "value": value, "global": global }),
            ))
        }
        Command::Unset { key, global } => {
            let removed = c.unset(scope(global), &key)?;
            out.say(if removed {
                format!("{key} removed")
            } else {
                format!("{key} was not set")
            });
            Ok(Done::ok(
                json!({ "key": key, "removed": removed, "global": global }),
            ))
        }
        Command::List => {
            let all = c.list()?;
            if all.is_empty() {
                out.say("No config variables are set.");
            }
            for r in &all {
                out.say(format!(
                    "{:<16} {:<40} {}",
                    r.key,
                    r.value,
                    source(&r.source)
                ));
            }
            Ok(Done::ok(Value::Array(all.iter().map(row).collect())))
        }
    }
}
