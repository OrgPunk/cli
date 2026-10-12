//! opk: a CLI for Salesforce DevOps.

mod cmd;
mod complete;
mod context;
mod output;

use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::CompleteEnv;

use crate::output::Output;

#[derive(Parser)]
#[command(name = "opk", version, about = "OrgPunk: CLI for Salesforce DevOps")]
struct Cli {
    /// JSON output on stdout; progress and hints go to stderr.
    #[arg(long, global = true, help_heading = "Global options")]
    json: bool,
    /// Non-interactive CI mode (also with CI=true): no prompts, JSON output, auth only from
    /// ORGPUNK_CLIENT_ID, ORGPUNK_JWT_KEY, ORGPUNK_USERNAME, ORGPUNK_INSTANCE_URL.
    #[arg(long, global = true, help_heading = "Global options")]
    ci: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a project or set opk up in an existing one, with a git repository.
    Init(cmd::init::Args),
    /// Org connections: login, list, the default target.
    #[command(subcommand)]
    Org(cmd::org::Command),
    /// Show what changed in the org and locally since the last pull or push.
    Status(cmd::sync::StatusArgs),
    /// Bring the org's changes since the last sync into the working copy.
    Pull(cmd::sync::PullArgs),
    /// Send local changes since the last sync to the org.
    Push(cmd::sync::PushArgs),
    /// Environments of the pipeline (qa, uat, prod): deploy, history, rollback.
    #[command(subcommand)]
    Env(cmd::env::Command),
    /// Config variables: local (project) and global.
    #[command(subcommand)]
    Config(cmd::config::Command),
    /// Print the shell completion script
    ///
    /// bash: add `source <(opk completion bash)` to ~/.bashrc
    /// zsh: add `source <(opk completion zsh)` to ~/.zshrc
    /// fish: add `opk completion fish | source` to ~/.config/fish/config.fish
    /// PowerShell: add `opk completion powershell | Out-String | Invoke-Expression` to $PROFILE
    #[command(verbatim_doc_comment)]
    Completion {
        /// The shell.
        shell: complete::Shell,
    },
}

fn main() -> ExitCode {
    CompleteEnv::with_factory(Cli::command)
        .var(complete::VAR)
        .complete();
    let cli = Cli::parse();
    if let Command::Completion { shell } = cli.command {
        return match complete::script(shell) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("opk: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let ci = orgpunk_env::ci_mode(cli.ci, std::env::var("CI").ok().as_deref());
    let out = Output::new(cli.json || ci);
    let result = context::Ctx::load(ci).and_then(|ctx| match cli.command {
        Command::Init(a) => cmd::init::run(&ctx, &out, a),
        Command::Org(c) => cmd::org::run(&ctx, &out, c),
        Command::Status(a) => cmd::sync::status(&ctx, &out, a),
        Command::Pull(a) => cmd::sync::pull(&ctx, &out, a),
        Command::Push(a) => cmd::sync::push(&ctx, &out, a),
        Command::Env(c) => cmd::env::run(&ctx, &out, c),
        Command::Config(c) => cmd::config::run(&ctx, &out, c),
        Command::Completion { .. } => unreachable!("handled before the context"),
    });
    let code = match result {
        Ok(done) => {
            out.result(done.code, &done.json);
            done.code
        }
        Err(failure) => {
            out.failure(&failure);
            failure.code
        }
    };
    ExitCode::from(code as u8)
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory as _;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
