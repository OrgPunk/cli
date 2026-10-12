//! source <-> mdapi conversion.
//!
//! The mdapi format (metadata types, package.xml, the components of a directory), the
//! dependencies between components, and source → mdapi for the supported strategies.

use std::path::PathBuf;

pub mod deps;
pub mod destructive;
pub mod forceignore;
pub mod manifest;
pub mod mdapi;
mod mime_ext;
pub mod registry;
pub mod replace;
pub mod select;
pub mod source;
pub mod to_source;
pub mod xml;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}: no package.xml at the root, not an mdapi directory")]
    NoPackageXml(PathBuf),
    #[error("{path}: invalid package.xml: {reason}")]
    Manifest { path: PathBuf, reason: String },
    #[error(
        "{dir}/: unknown metadata directory (not in opk's type list, and package.xml \
         lists none of its files)"
    )]
    UnknownDir { dir: String },
    #[error("{path}: invalid .forceignore: {reason}")]
    ForceIgnore { path: PathBuf, reason: String },
    #[error("{0}: a file at the root of an mdapi directory that is not package.xml")]
    StrayFile(String),
    #[error("{0}: unknown metadata suffix (not in the SDR registry)")]
    UnknownSuffix(PathBuf),
    #[error("{content}: {kind} without its -meta.xml file")]
    MissingMeta { content: PathBuf, kind: String },
    #[error("{path}: invalid XML: {reason}")]
    Xml { path: PathBuf, reason: String },
    #[error("{meta}: content file {content} is missing")]
    MissingContent { meta: PathBuf, content: PathBuf },
    #[error("sfdx-project.json: replacements[{index}]: {reason}")]
    Replacement { index: usize, reason: String },
    #[error("{path}: {reason}")]
    ReplacementValue { path: PathBuf, reason: String },
    #[error("{path}: {file} is missing")]
    MissingFile { path: PathBuf, file: String },
    #[error("{path}: must be inside a `{dir}` directory")]
    NotInTypeDir { path: PathBuf, dir: String },
    #[error("{path}: {kind} ({strategy} format) is not supported by opk yet")]
    Unsupported {
        kind: String,
        strategy: String,
        path: PathBuf,
    },
    #[error("{component} is defined twice: {first} and {second}")]
    Duplicate {
        component: String,
        first: PathBuf,
        second: PathBuf,
    },
    #[error("{0}: expected Type or Type:Name")]
    Selector(String),
    #[error("{0}: unknown metadata type")]
    UnknownType(String),
    #[error("{0}: no such component in the project")]
    NoMatch(String),
    #[error("{member}: has no file of its own, select the whole {parent}")]
    ChildWithoutFile { member: String, parent: String },
    #[error("{0}: deletions name exact components, not wildcards")]
    DeleteWildcard(String),
    #[error("{0}: deleted both before and after the deploy")]
    DeletedTwice(String),
    #[error("{0}: both deployed and deleted")]
    DeployedAndDeleted(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
