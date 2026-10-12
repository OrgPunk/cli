//! Project and global config, config variables, environment overrides.
//!
//! A config variable is resolved through layers, the first hit wins:
//! `ORGPUNK_*` → `SF_*` → project `.orgpunk/config.toml` → `~/.config/orgpunk/config.toml`.
//! Variables live in the `[config]` table; writes leave other tables (`[project]`,
//! `[env.*]`) and comments untouched.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, value};

pub mod project;
pub mod sfdx_project;

pub(crate) const PROJECT_DIR: &str = ".orgpunk";
pub(crate) const CONFIG_FILE: &str = "config.toml";
const CONFIG_TABLE: &str = "config";

/// Global config directory override, used instead of `~/.config/orgpunk`.
pub const CONFIG_DIR_ENV: &str = "ORGPUNK_CONFIG_DIR";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unknown config variable `{0}`; known: {known}", known = known_keys())]
    UnknownKey(String),
    #[error("invalid value `{value}` for `{key}`: {reason}")]
    InvalidValue {
        key: String,
        value: String,
        reason: String,
    },
    #[error(
        "not inside an opk project (no sfdx-project.json or .orgpunk/ above {0}); use --global"
    )]
    NoProject(PathBuf),
    #[error("home directory not found; set {CONFIG_DIR_ENV}")]
    NoHome,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {reason}")]
    ProjectConfig { path: PathBuf, reason: String },
    #[error("{path}: invalid sfdx-project.json: {reason}")]
    SfdxProject { path: PathBuf, reason: String },
    #[error("{path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    ApiVersion,
    Url,
    PositiveInt,
}

/// A config variable and its environment counterparts.
#[derive(Debug)]
pub struct Var {
    pub key: &'static str,
    pub env: &'static str,
    pub sf_env: &'static str,
    kind: Kind,
}

pub const VARS: &[Var] = &[
    Var {
        key: "target-org",
        env: "ORGPUNK_TARGET_ORG",
        sf_env: "SF_TARGET_ORG",
        kind: Kind::Text,
    },
    Var {
        key: "target-dev-hub",
        env: "ORGPUNK_TARGET_DEV_HUB",
        sf_env: "SF_TARGET_DEV_HUB",
        kind: Kind::Text,
    },
    Var {
        key: "api-version",
        env: "ORGPUNK_API_VERSION",
        sf_env: "SF_ORG_API_VERSION",
        kind: Kind::ApiVersion,
    },
    Var {
        key: "instance-url",
        env: "ORGPUNK_INSTANCE_URL",
        sf_env: "SF_ORG_INSTANCE_URL",
        kind: Kind::Url,
    },
    Var {
        key: "max-query-limit",
        env: "ORGPUNK_MAX_QUERY_LIMIT",
        sf_env: "SF_ORG_MAX_QUERY_LIMIT",
        kind: Kind::PositiveInt,
    },
];

fn known_keys() -> String {
    VARS.iter().map(|v| v.key).collect::<Vec<_>>().join(", ")
}

fn var(key: &str) -> Result<&'static Var> {
    VARS.iter()
        .find(|v| v.key == key)
        .ok_or_else(|| Error::UnknownKey(key.to_owned()))
}

impl Var {
    fn validate(&self, value: &str) -> Result<()> {
        let invalid = |reason: &str| Error::InvalidValue {
            key: self.key.to_owned(),
            value: value.to_owned(),
            reason: reason.to_owned(),
        };
        match self.kind {
            Kind::Text if value.trim().is_empty() => Err(invalid("empty value")),
            Kind::Text => Ok(()),
            Kind::ApiVersion => {
                let ok = value.strip_suffix(".0").is_some_and(|major| {
                    !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit())
                });
                if ok {
                    Ok(())
                } else {
                    Err(invalid("expected a version like 66.0"))
                }
            }
            Kind::Url => match url::Url::parse(value) {
                Ok(u) if u.scheme() == "https" && u.host().is_some() => Ok(()),
                _ => Err(invalid("expected an https URL")),
            },
            Kind::PositiveInt => match value.parse::<u64>() {
                Ok(n) if n > 0 => Ok(()),
                _ => Err(invalid("expected an integer greater than 0")),
            },
        }
    }
}

/// Where a value is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Local,
    Global,
}

/// Where a value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Env(&'static str),
    Local,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub key: &'static str,
    pub value: String,
    pub source: Source,
}

/// Loaded config: paths and an environment snapshot.
#[derive(Debug, Clone)]
pub struct Config {
    project_root: Option<PathBuf>,
    global_dir: PathBuf,
    env: BTreeMap<String, String>,
}

impl Config {
    /// Config for the current directory and process environment.
    pub fn load() -> Result<Self> {
        let cwd = std::env::current_dir().map_err(|source| Error::Io {
            path: PathBuf::from("."),
            source,
        })?;
        let env: BTreeMap<String, String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("ORGPUNK_") || k.starts_with("SF_"))
            .collect();
        let global_dir = match env.get(CONFIG_DIR_ENV) {
            Some(dir) => PathBuf::from(dir),
            None => std::env::home_dir()
                .ok_or(Error::NoHome)?
                .join(".config")
                .join("opk"),
        };
        Ok(Self::new(find_project_root(&cwd), global_dir, env))
    }

    pub fn new(
        project_root: Option<PathBuf>,
        global_dir: PathBuf,
        env: BTreeMap<String, String>,
    ) -> Self {
        Self {
            project_root,
            global_dir,
            env,
        }
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    /// `~/.config/orgpunk` or `ORGPUNK_CONFIG_DIR`.
    pub fn global_dir(&self) -> &Path {
        &self.global_dir
    }

    pub fn get(&self, key: &str) -> Result<Option<Resolved>> {
        let var = var(key)?;
        for name in [var.env, var.sf_env] {
            if let Some(v) = self.env.get(name).filter(|v| !v.is_empty()) {
                return Ok(Some(Resolved {
                    key: var.key,
                    value: v.clone(),
                    source: Source::Env(name),
                }));
            }
        }
        for (scope, source) in [
            (Scope::Local, Source::Local),
            (Scope::Global, Source::Global),
        ] {
            let Some(path) = self.file(scope) else {
                continue;
            };
            if let Some(v) = read_value(&path, var.key)? {
                return Ok(Some(Resolved {
                    key: var.key,
                    value: v,
                    source,
                }));
            }
        }
        Ok(None)
    }

    /// All variables that are set.
    pub fn list(&self) -> Result<Vec<Resolved>> {
        let mut out = Vec::new();
        for var in VARS {
            if let Some(r) = self.get(var.key)? {
                out.push(r);
            }
        }
        Ok(out)
    }

    pub fn set(&self, scope: Scope, key: &str, val: &str) -> Result<()> {
        let var = var(key)?;
        var.validate(val)?;
        let path = self.file_for_write(scope)?;
        edit(&path, |doc| {
            let table = doc
                .entry(CONFIG_TABLE)
                .or_insert_with(|| Item::Table(Table::new()))
                .as_table_mut()
                .expect("[config] is a table");
            table[var.key] = value(val);
        })
    }

    /// Returns true if a value was present.
    pub fn unset(&self, scope: Scope, key: &str) -> Result<bool> {
        let var = var(key)?;
        let path = self.file_for_write(scope)?;
        if !path.exists() {
            return Ok(false);
        }
        let mut removed = false;
        edit(&path, |doc| {
            if let Some(table) = doc.get_mut(CONFIG_TABLE).and_then(Item::as_table_mut) {
                removed = table.remove(var.key).is_some();
            }
        })?;
        Ok(removed)
    }

    fn file(&self, scope: Scope) -> Option<PathBuf> {
        match scope {
            Scope::Local => self
                .project_root
                .as_ref()
                .map(|r| r.join(PROJECT_DIR).join(CONFIG_FILE)),
            Scope::Global => Some(self.global_dir.join(CONFIG_FILE)),
        }
    }

    fn file_for_write(&self, scope: Scope) -> Result<PathBuf> {
        self.file(scope).ok_or_else(|| {
            Error::NoProject(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        })
    }
}

/// The nearest ancestor directory with `.orgpunk/` or `sfdx-project.json`.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(PROJECT_DIR).is_dir() || dir.join("sfdx-project.json").is_file())
        .map(Path::to_path_buf)
}

fn read_doc(path: &Path) -> Result<Option<DocumentMut>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    text.parse::<DocumentMut>()
        .map(Some)
        .map_err(|source| Error::Parse {
            path: path.to_owned(),
            source,
        })
}

fn read_value(path: &Path, key: &str) -> Result<Option<String>> {
    Ok(
        read_doc(path)?
            .and_then(|doc| doc.get(CONFIG_TABLE)?.get(key)?.as_str().map(str::to_owned)),
    )
}

fn edit(path: &Path, f: impl FnOnce(&mut DocumentMut)) -> Result<()> {
    let mut doc = read_doc(path)?.unwrap_or_default();
    f(&mut doc);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;
    }
    fs::write(path, doc.to_string()).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        project: PathBuf,
        global: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let global = tmp.path().join("global");
        fs::create_dir_all(project.join("force-app")).unwrap();
        fs::write(project.join("sfdx-project.json"), "{}").unwrap();
        Fixture {
            _tmp: tmp,
            project,
            global,
        }
    }

    fn config(f: &Fixture, env: &[(&str, &str)]) -> Config {
        let env = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::new(Some(f.project.clone()), f.global.clone(), env)
    }

    #[test]
    fn finds_project_root_from_subdir() {
        let f = fixture();
        assert_eq!(
            find_project_root(&f.project.join("force-app")),
            Some(f.project.clone())
        );
    }

    #[test]
    fn layers_resolve_in_order() {
        let f = fixture();
        let c = config(&f, &[]);
        c.set(Scope::Global, "target-org", "global-org").unwrap();
        assert_eq!(c.get("target-org").unwrap().unwrap().source, Source::Global);

        c.set(Scope::Local, "target-org", "local-org").unwrap();
        let r = c.get("target-org").unwrap().unwrap();
        assert_eq!((r.value.as_str(), r.source), ("local-org", Source::Local));

        let c = config(&f, &[("SF_TARGET_ORG", "sf-org")]);
        assert_eq!(
            c.get("target-org").unwrap().unwrap().source,
            Source::Env("SF_TARGET_ORG")
        );

        let c = config(
            &f,
            &[
                ("SF_TARGET_ORG", "sf-org"),
                ("ORGPUNK_TARGET_ORG", "opk-org"),
            ],
        );
        assert_eq!(c.get("target-org").unwrap().unwrap().value, "opk-org");
    }

    #[test]
    fn empty_env_value_is_ignored() {
        let f = fixture();
        let c = config(&f, &[("ORGPUNK_TARGET_ORG", "")]);
        c.set(Scope::Global, "target-org", "global-org").unwrap();
        assert_eq!(c.get("target-org").unwrap().unwrap().value, "global-org");
    }

    #[test]
    fn set_keeps_other_tables_and_comments() {
        let f = fixture();
        let path = f.project.join(".orgpunk/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "# master org\n[project]\nmaster = \"00D\"\n").unwrap();
        let c = config(&f, &[]);
        c.set(Scope::Local, "api-version", "66.0").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# master org"));
        assert!(text.contains("master = \"00D\""));
        assert!(text.contains("api-version = \"66.0\""));
    }

    #[test]
    fn unset_removes_value() {
        let f = fixture();
        let c = config(&f, &[]);
        assert!(!c.unset(Scope::Global, "target-org").unwrap());
        c.set(Scope::Global, "target-org", "x").unwrap();
        assert!(c.unset(Scope::Global, "target-org").unwrap());
        assert_eq!(c.get("target-org").unwrap(), None);
    }

    #[test]
    fn validates_values() {
        let f = fixture();
        let c = config(&f, &[]);
        assert!(matches!(
            c.set(Scope::Global, "nope", "x"),
            Err(Error::UnknownKey(_))
        ));
        for (key, bad) in [
            ("api-version", "66"),
            ("api-version", "v66.0"),
            ("instance-url", "http://example.com"),
            ("max-query-limit", "0"),
            ("target-org", " "),
        ] {
            assert!(
                matches!(
                    c.set(Scope::Global, key, bad),
                    Err(Error::InvalidValue { .. })
                ),
                "{key}={bad}"
            );
        }
        c.set(Scope::Global, "instance-url", "https://x.my.salesforce.com")
            .unwrap();
    }

    #[test]
    fn local_scope_requires_project() {
        let f = fixture();
        let c = Config::new(None, f.global.clone(), BTreeMap::new());
        assert!(matches!(
            c.set(Scope::Local, "target-org", "x"),
            Err(Error::NoProject(_))
        ));
    }
}
