//! status / pull / push: the working copy and an org, like git with a remote.
//!
//! The state of the last sync with each org (`.orgpunk/state/<OrgId>.json`, not committed) holds,
//! per component, the org's revision and the hashes of its local files then. A component
//! changed since on one side is a local or a remote change; on both sides, a conflict.

pub mod apply;
pub mod local;
pub mod remote;
pub mod retrieve;
pub mod settle;
pub mod state;
pub mod status;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: invalid sync state: {reason}")]
    State { path: PathBuf, reason: String },
    #[error(transparent)]
    Convert(#[from] orgpunk_convert::Error),
    #[error(transparent)]
    Api(#[from] orgpunk_sfapi::Error),
    #[error(transparent)]
    Config(#[from] orgpunk_config::Error),
    #[error("retrieve {0}")]
    Retrieve(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Case-insensitive identity of a `Type:Name` member, as the org treats names.
pub fn key(member: &str) -> String {
    // A folder is `Report:Sales/` in package.xml and `Report:Sales` in listings.
    member.trim_end_matches('/').to_lowercase()
}

#[cfg(test)]
mod tests {
    #[test]
    fn folder_keys_ignore_the_slash() {
        assert_eq!(super::key("Report:Sales/"), super::key("report:sales"));
    }
}
