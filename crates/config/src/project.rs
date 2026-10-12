//! `[env.*]` of `.orgpunk/config.toml`: the environments, their protect policies and the master
//! (the environment with `master = true`). Everything is keyed by Org Id, so an alias never
//! decides protection.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Error, Result};

/// Protect policies, combinable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// Type the org alias (or pass `--confirm-org <OrgId>`) before deploying.
    Confirm,
    /// checkOnly the same commit first, then quick deploy the validation.
    ValidateFirst,
    /// Only from allowed branches, with a clean working tree, HEAD at its upstream.
    Branch,
    /// No component deletions.
    NoDestructive,
    /// No deploys at all.
    Locked,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Confirm => "confirm",
            Policy::ValidateFirst => "validate-first",
            Policy::Branch => "branch",
            Policy::NoDestructive => "no-destructive",
            Policy::Locked => "locked",
        }
    }
}

impl std::str::FromStr for Policy {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        [
            Policy::Confirm,
            Policy::ValidateFirst,
            Policy::Branch,
            Policy::NoDestructive,
            Policy::Locked,
        ]
        .into_iter()
        .find(|p| p.as_str() == s.trim())
        .ok_or_else(|| {
            format!(
                "unknown protect policy `{s}`; known: confirm, validate-first, branch, \
                 no-destructive, locked"
            )
        })
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Protect of the master environment when it names no policies.
pub const DEFAULT_MASTER_PROTECT: &[Policy] =
    &[Policy::Confirm, Policy::ValidateFirst, Policy::Branch];
/// Allowed branches when a `branch` policy names none.
pub const DEFAULT_BRANCHES: &[&str] = &["main"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectConfig {
    /// By name.
    pub envs: BTreeMap<String, Environment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub name: String,
    pub org: String,
    /// The source of truth, usually production; at most one environment.
    pub master: bool,
    pub protect: Vec<Policy>,
    /// Glob patterns (`release/*`) for the `branch` policy.
    pub branches: Vec<String>,
    /// The environment `opk env deploy <next> --from <name>` usually goes to.
    pub next: Option<String>,
}

impl Environment {
    /// A new environment: the master gets the default master protect.
    pub fn new(name: &str, org: &str, master: bool) -> Self {
        Self {
            name: name.to_owned(),
            org: org.to_owned(),
            master,
            protect: if master {
                DEFAULT_MASTER_PROTECT.to_vec()
            } else {
                Vec::new()
            },
            branches: default_branches(),
            next: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnv {
    org: String,
    #[serde(default)]
    master: bool,
    protect: Option<Vec<Policy>>,
    branches: Option<Vec<String>>,
    next: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawFile {
    project: Option<toml::Table>,
    #[serde(default)]
    env: BTreeMap<String, RawEnv>,
}

fn default_branches() -> Vec<String> {
    DEFAULT_BRANCHES.iter().map(|s| (*s).to_owned()).collect()
}

/// The first 15 characters of an Org Id: the 18-character form only adds a checksum.
pub fn org_key(org_id: &str) -> &str {
    org_id.get(..15).unwrap_or(org_id)
}

pub fn same_org(a: &str, b: &str) -> bool {
    org_key(a).eq_ignore_ascii_case(org_key(b))
}

pub fn valid_org_id(id: &str) -> bool {
    (id.len() == 15 || id.len() == 18)
        && id.starts_with("00D")
        && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Environment names are TOML keys and command arguments: keep them plain.
pub fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl ProjectConfig {
    /// Reads `[env.*]` of `<root>/.orgpunk/config.toml`; a missing file is an empty config.
    pub fn load(root: &Path) -> Result<Self> {
        let path = config_path(root);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(Error::Io { path, source }),
        };
        Self::parse(&text).map_err(|reason| Error::ProjectConfig { path, reason })
    }

    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let raw: RawFile = toml::from_str(text).map_err(|e| e.message().to_owned())?;
        if raw.project.as_ref().is_some_and(|p| !p.is_empty()) {
            return Err(
                "[project] is no longer used: the master is an environment with \
                        `master = true`; remove [project] and run `opk env add <name> <alias> \
                        --master`"
                    .into(),
            );
        }
        let mut envs = BTreeMap::new();
        for (name, e) in raw.env {
            if !valid_env_name(&name) {
                return Err(format!(
                    "[env.{name}]: an environment name has only letters, digits, - and _"
                ));
            }
            if !valid_org_id(&e.org) {
                return Err(format!(
                    "[env.{name}] org = \"{}\": not an Org Id (00D…, 15 or 18 characters)",
                    e.org
                ));
            }
            let protect = e.protect.unwrap_or_else(|| {
                if e.master {
                    DEFAULT_MASTER_PROTECT.to_vec()
                } else {
                    Vec::new()
                }
            });
            envs.insert(
                name.clone(),
                Environment {
                    name,
                    org: e.org,
                    master: e.master,
                    protect: dedup(protect),
                    branches: e.branches.unwrap_or_else(default_branches),
                    next: e.next,
                },
            );
        }
        let masters: Vec<&str> = envs
            .values()
            .filter(|e| e.master)
            .map(|e| e.name.as_str())
            .collect();
        if masters.len() > 1 {
            return Err(format!(
                "more than one master environment: {}",
                masters.join(", ")
            ));
        }
        for e in envs.values() {
            if let Some(next) = &e.next {
                if !envs.contains_key(next) {
                    return Err(format!(
                        "[env.{}] next = \"{next}\": no such environment; add env.{next} first",
                        e.name
                    ));
                }
            }
        }
        // A promotion chain must end.
        for start in envs.keys() {
            let mut seen = vec![start.as_str()];
            let mut at = start.as_str();
            while let Some(next) = envs[at].next.as_deref() {
                if seen.contains(&next) {
                    return Err(format!(
                        "environments promote in a loop: {} → {next}",
                        seen.join(" → ")
                    ));
                }
                seen.push(next);
                at = next;
            }
        }
        Ok(Self { envs })
    }

    /// The master environment.
    pub fn master(&self) -> Option<&Environment> {
        self.envs.values().find(|e| e.master)
    }

    /// The environments of an org.
    pub fn envs_of<'a>(&'a self, org_id: &'a str) -> impl Iterator<Item = &'a Environment> {
        self.envs.values().filter(move |e| same_org(&e.org, org_id))
    }

    pub fn is_master(&self, org_id: &str) -> bool {
        self.master().is_some_and(|m| same_org(&m.org, org_id))
    }
}

/// Writes environments to `<root>/.orgpunk/config.toml`: `save` replaces or adds their tables,
/// `remove` drops tables. Comments, other tables and variables stay. The result must be a
/// valid config, else nothing is written.
pub fn save_envs(root: &Path, save: &[&Environment], remove: &[&str]) -> Result<()> {
    let path = config_path(root);
    let invalid = |reason: String| Error::ProjectConfig {
        path: path.clone(),
        reason,
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(Error::Io { path, source }),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|source| Error::Parse {
        path: path.clone(),
        source,
    })?;
    let envs = doc
        .entry("env")
        .or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            toml_edit::Item::Table(t)
        })
        .as_table_mut()
        .ok_or_else(|| invalid("[env] is not a table".into()))?;
    for name in remove {
        envs.remove(name);
    }
    for e in save {
        let table = envs
            .entry(&e.name)
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()
            .ok_or_else(|| invalid(format!("[env.{}] is not a table", e.name)))?;
        table["org"] = toml_edit::value(&e.org);
        if e.master {
            table["master"] = toml_edit::value(true);
        } else {
            table.remove("master");
        }
        let mut list = toml_edit::Array::new();
        for p in dedup(e.protect.clone()) {
            list.push(p.as_str());
        }
        table["protect"] = toml_edit::value(list);
        if e.branches == DEFAULT_BRANCHES {
            table.remove("branches");
        } else {
            let mut list = toml_edit::Array::new();
            for b in &e.branches {
                list.push(b.as_str());
            }
            table["branches"] = toml_edit::value(list);
        }
        match &e.next {
            Some(n) => table["next"] = toml_edit::value(n),
            None => {
                table.remove("next");
            }
        }
    }
    let text = doc.to_string();
    ProjectConfig::parse(&text).map_err(invalid)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_owned(),
            source,
        })?;
    }
    std::fs::write(&path, text).map_err(|source| Error::Io { path, source })
}

fn dedup(mut v: Vec<Policy>) -> Vec<Policy> {
    v.sort();
    v.dedup();
    v
}

/// `.orgpunk/config.toml` of a project root, for messages.
pub fn config_path(root: &Path) -> PathBuf {
    root.join(crate::PROJECT_DIR).join(crate::CONFIG_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORG_PROD: &str = "00D000000000001AAA";
    const ORG_QA: &str = "00D000000000002";

    #[test]
    fn parses_the_documented_example() {
        let c = ProjectConfig::parse(&format!(
            r#"
            [config]
            target-org = "dev"

            [env.qa]
            org = "{ORG_QA}"
            protect = ["confirm"]
            branches = ["develop"]
            next = "prod"

            [env.prod]
            org = "{ORG_PROD}"
            master = true
            protect = ["confirm", "validate-first", "branch", "no-destructive"]
            "#
        ))
        .unwrap();
        assert_eq!(c.envs["qa"].next.as_deref(), Some("prod"));
        assert_eq!(c.master().unwrap().name, "prod");
        assert_eq!(c.envs["prod"].branches, ["main"]);
        assert!(
            c.is_master("00D000000000001"),
            "15 and 18 characters are one org"
        );
        assert!(!c.is_master(ORG_QA));
        assert_eq!(
            c.envs_of(ORG_PROD)
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["prod"]
        );
        assert_eq!(c.envs["prod"].protect.len(), 4);
    }

    #[test]
    fn master_gets_the_default_protect() {
        let c = ProjectConfig::parse(&format!(
            "[env.prod]\norg = \"{ORG_PROD}\"\nmaster = true\n[env.qa]\norg = \"{ORG_QA}\""
        ))
        .unwrap();
        assert_eq!(c.envs["prod"].protect, DEFAULT_MASTER_PROTECT);
        assert!(c.envs["qa"].protect.is_empty());
        let c = ProjectConfig::parse(&format!(
            "[env.prod]\norg = \"{ORG_PROD}\"\nmaster = true\nprotect = []"
        ))
        .unwrap();
        assert!(
            c.envs["prod"].protect.is_empty(),
            "an explicit empty list stays"
        );
        assert_eq!(ProjectConfig::parse("").unwrap(), ProjectConfig::default());
    }

    #[test]
    fn saves_and_removes_keeping_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let p = config_path(d.path());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            &p,
            format!("# keep me\n[config]\ntarget-org = \"dev\"\n\n[env.qa]\norg = \"{ORG_QA}\"\n"),
        )
        .unwrap();
        let mut prod = Environment::new("prod", ORG_PROD, true);
        prod.protect.push(Policy::NoDestructive);
        let mut qa = ProjectConfig::load(d.path()).unwrap().envs["qa"].clone();
        qa.next = Some("prod".into());
        qa.branches = vec!["develop".into()];
        save_envs(d.path(), &[&prod, &qa], &[]).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# keep me") && text.contains("target-org = \"dev\""));
        let c = ProjectConfig::load(d.path()).unwrap();
        assert_eq!(c.envs["prod"], prod);
        assert_eq!(c.envs["qa"], qa);

        // Removing prod while qa still points to it is not a valid config.
        assert!(save_envs(d.path(), &[], &["prod"]).is_err());
        qa.next = None;
        save_envs(d.path(), &[&qa], &["prod"]).unwrap();
        let c = ProjectConfig::load(d.path()).unwrap();
        assert!(c.master().is_none() && c.envs.len() == 1);
        assert!(!std::fs::read_to_string(&p).unwrap().contains("next"));

        assert_eq!(
            "validate-first".parse::<Policy>().unwrap(),
            Policy::ValidateFirst
        );
        assert!("yolo".parse::<Policy>().is_err());
    }

    #[test]
    fn rejects_bad_configs() {
        let bad = [
            format!("[project]\nmaster = \"{ORG_PROD}\""),
            "[env.prod]\norg = \"prod\"".to_owned(),
            format!("[env.qa]\norg = \"{ORG_QA}\"\nprotect = [\"yolo\"]"),
            format!("[env.qa]\norg = \"{ORG_QA}\"\nnext = \"nope\""),
            format!(
                "[env.a]\norg = \"{ORG_QA}\"\nnext = \"b\"\n[env.b]\norg = \"{ORG_QA}\"\nnext = \"a\""
            ),
            format!("[env.qa]\norg = \"{ORG_QA}\"\nbranch = [\"main\"]"),
            format!(
                "[env.a]\norg = \"{ORG_QA}\"\nmaster = true\n[env.b]\norg = \"{ORG_PROD}\"\nmaster = true"
            ),
            format!("[env.\"q a\"]\norg = \"{ORG_QA}\""),
        ];
        for text in bad {
            assert!(ProjectConfig::parse(&text).is_err(), "{text}");
        }
    }
}
