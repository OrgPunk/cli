//! Output: human-readable or `--json`, and exit codes.

use serde_json::{Value, json};

/// `--json` schema version.
const JSON_SCHEMA: u32 = 1;

/// opk exit codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Exit {
    Ok = 0,
    /// CLI or environment error.
    Error = 1,
    /// Refused by policy (protect, deletions without the flag).
    Refused = 2,
    /// The deploy failed: components or tests.
    DeployFailed = 3,
    /// `--wait` timed out; the deploy is still running.
    InProgress = 4,
}

/// A command that completed (from the CLI's point of view).
pub struct Done {
    pub code: Exit,
    pub json: Value,
}

impl Done {
    pub fn ok(json: Value) -> Self {
        Self {
            code: Exit::Ok,
            json,
        }
    }
}

/// An error with an exit code.
#[derive(Debug)]
pub struct Failure {
    pub code: Exit,
    pub message: String,
}

impl Failure {
    pub fn new(code: Exit, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(Exit::Error, message)
    }
}

macro_rules! failure_from {
    ($($t:ty),*) => {$(
        impl From<$t> for Failure {
            fn from(e: $t) -> Self {
                Self::error(e.to_string())
            }
        }
    )*};
}
failure_from!(
    orgpunk_config::Error,
    orgpunk_auth::Error,
    orgpunk_auth::OAuthError,
    orgpunk_sfapi::Error,
    orgpunk_convert::Error,
    orgpunk_sync::Error,
    orgpunk_repo::Error,
    orgpunk_init::Error,
    reqwest::Error
);

impl From<orgpunk_deploy::Error> for Failure {
    fn from(e: orgpunk_deploy::Error) -> Self {
        let code = if e.is_refusal() {
            Exit::Refused
        } else {
            Exit::Error
        };
        Self::new(code, e.to_string())
    }
}

pub struct Output {
    json: bool,
}

impl Output {
    pub fn new(json: bool) -> Self {
        Self { json }
    }

    /// Human-readable text on stdout; suppressed with `--json`.
    pub fn say(&self, text: impl AsRef<str>) {
        if !self.json {
            println!("{}", text.as_ref());
        }
    }

    /// Progress and hints always go to stderr so they never break JSON on stdout.
    pub fn progress(&self, text: impl AsRef<str>) {
        eprintln!("{}", text.as_ref());
    }

    pub fn result(&self, code: Exit, result: &Value) {
        if self.json {
            println!(
                "{}",
                json!({ "version": JSON_SCHEMA, "status": code as u8, "result": result })
            );
        }
    }

    pub fn failure(&self, f: &Failure) {
        if self.json {
            println!(
                "{}",
                json!({ "version": JSON_SCHEMA, "status": f.code as u8, "error": { "message": f.message } })
            );
        } else {
            eprintln!("Error: {}", f.message);
        }
    }
}
