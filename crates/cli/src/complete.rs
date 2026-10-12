//! Shell completion. `opk completion <shell>` prints a small script; on Tab the shell calls opk
//! back (`COMPLETE=<shell> opk -- <words>`), so environment names, aliases and config keys are
//! always the current ones. Completers read local files only and never fail: no candidates
//! instead of an error.

use clap::ValueEnum;
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};
use clap_complete::env::Shells;
use orgpunk_auth::Connections;
use orgpunk_config::Config;
use orgpunk_config::project::{ProjectConfig, same_org};

/// The variable the shell script sets when it asks opk for candidates.
pub const VAR: &str = "COMPLETE";

#[derive(Clone, Copy, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
}

impl Shell {
    fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::Powershell => "powershell",
            Self::Elvish => "elvish",
        }
    }
}

/// The registration script for `shell` on stdout.
pub fn script(shell: Shell) -> std::io::Result<()> {
    let shells = Shells::builtins();
    let completer = shells
        .completer(shell.name())
        .expect("every Shell is a builtin of clap_complete");
    completer.write_registration(VAR, "opk", "opk", "opk", &mut std::io::stdout().lock())
}

/// Environments of the project in the current directory, with their org alias.
pub fn envs() -> ArgValueCandidates {
    ArgValueCandidates::new(|| {
        let Ok(config) = Config::load() else {
            return Vec::new();
        };
        let Some(project) = config
            .project_root()
            .and_then(|root| ProjectConfig::load(root).ok())
        else {
            return Vec::new();
        };
        let connections = Connections::load(config.global_dir()).ok();
        project
            .envs
            .values()
            .map(|env| {
                let alias = connections.as_ref().and_then(|c| {
                    c.iter()
                        .find(|(_, conn)| same_org(&conn.org_id, &env.org))
                        .map(|(alias, _)| alias.clone())
                });
                let mut help = alias.unwrap_or_else(|| env.org.clone());
                if env.master {
                    help.push_str(", master");
                }
                CompletionCandidate::new(&env.name).help(Some(help.into()))
            })
            .collect()
    })
}

/// Aliases of the stored connections, with their username.
pub fn orgs() -> ArgValueCandidates {
    ArgValueCandidates::new(|| {
        let Ok(config) = Config::load() else {
            return Vec::new();
        };
        let Ok(connections) = Connections::load(config.global_dir()) else {
            return Vec::new();
        };
        connections
            .iter()
            .map(|(alias, conn)| {
                CompletionCandidate::new(alias).help(Some(conn.username.clone().into()))
            })
            .collect()
    })
}

/// Config variables.
pub fn config_keys() -> ArgValueCandidates {
    ArgValueCandidates::new(|| {
        orgpunk_config::VARS
            .iter()
            .map(|v| CompletionCandidate::new(v.key))
            .collect()
    })
}

/// Metadata type names for `--metadata`: `ApexClass`, then `:Name` by hand.
pub fn metadata_types() -> ArgValueCandidates {
    ArgValueCandidates::new(|| {
        let mut names: Vec<_> = orgpunk_convert::registry::get().names().collect();
        names.sort_unstable();
        names.into_iter().map(CompletionCandidate::new).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shell_has_a_registration_script() {
        let shells = Shells::builtins();
        for shell in Shell::value_variants() {
            let completer = shells.completer(shell.name()).unwrap();
            let mut buf = Vec::new();
            completer
                .write_registration(VAR, "opk", "opk", "opk", &mut buf)
                .unwrap();
            assert!(String::from_utf8(buf).unwrap().contains("opk"));
        }
    }

    #[test]
    fn metadata_types_are_sorted_and_include_children() {
        let names: Vec<String> = metadata_types()
            .candidates()
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect();
        assert!(names.windows(2).all(|w| w[0] <= w[1]));
        assert!(names.iter().any(|n| n == "ApexClass"));
        assert!(names.iter().any(|n| n == "CustomField"));
    }
}
