//! `connections.toml`: alias → org, never tokens.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const FILE: &str = "connections.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrgType {
    Production,
    Sandbox,
    Scratch,
}

impl std::fmt::Display for OrgType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Production => "prod",
            Self::Sandbox => "sandbox",
            Self::Scratch => "scratch",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum AuthMethod {
    Web,
    Jwt { key_path: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub org_id: String,
    pub user_id: String,
    pub username: String,
    pub instance_url: String,
    pub login_url: String,
    pub client_id: String,
    pub auth: AuthMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_type: Option<OrgType>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dev_hub: bool,
}

impl Connection {
    /// Key of the tokens in the secret store: one per org + user pair.
    pub fn secret_key(&self) -> String {
        format!("{}/{}", self.org_id, self.user_id)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    orgs: BTreeMap<String, Connection>,
}

#[derive(Debug)]
pub struct Connections {
    path: PathBuf,
    file: File,
}

impl Connections {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(FILE);
        let file = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| Error::File {
                path: path.clone(),
                message: e.to_string(),
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(e) => {
                return Err(Error::File {
                    path,
                    message: e.to_string(),
                });
            }
        };
        Ok(Self { path, file })
    }

    pub fn save(&self) -> Result<()> {
        let err = |e: &dyn std::fmt::Display| Error::File {
            path: self.path.clone(),
            message: e.to_string(),
        };
        let text = toml::to_string_pretty(&self.file).map_err(|e| err(&e))?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|e| err(&e))?;
        }
        fs::write(&self.path, text).map_err(|e| err(&e))
    }

    pub fn get(&self, alias: &str) -> Option<&Connection> {
        self.file.orgs.get(alias)
    }

    pub fn insert(&mut self, alias: &str, conn: Connection) {
        self.file.orgs.insert(alias.to_owned(), conn);
    }

    pub fn remove(&mut self, alias: &str) -> Option<Connection> {
        self.file.orgs.remove(alias)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Connection)> {
        self.file.orgs.iter()
    }

    /// Alias by alias, username or Org Id: as in sf, target-org may be any of them.
    pub fn resolve(&self, name: &str) -> Option<&str> {
        if self.file.orgs.contains_key(name) {
            return Some(self.file.orgs.get_key_value(name)?.0);
        }
        self.file
            .orgs
            .iter()
            .find(|(_, c)| {
                c.username == name || c.org_id == name || c.org_id.get(..15) == Some(name)
            })
            .map(|(a, _)| a.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(org: &str) -> Connection {
        Connection {
            org_id: org.into(),
            user_id: "005000000000001AAA".into(),
            username: format!("user@{org}.test"),
            instance_url: "https://x.my.salesforce.com".into(),
            login_url: "https://login.salesforce.com".into(),
            client_id: "PlatformCLI".into(),
            auth: AuthMethod::Jwt {
                key_path: "/k/server.key".into(),
            },
            org_type: Some(OrgType::Sandbox),
            dev_hub: false,
        }
    }

    #[test]
    fn roundtrip_and_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Connections::load(dir.path()).unwrap();
        c.insert("qa", conn("00D000000000001AAA"));
        c.save().unwrap();

        let c = Connections::load(dir.path()).unwrap();
        assert_eq!(c.get("qa"), Some(&conn("00D000000000001AAA")));
        assert_eq!(c.resolve("qa"), Some("qa"));
        assert_eq!(c.resolve("user@00D000000000001AAA.test"), Some("qa"));
        assert_eq!(c.resolve("00D000000000001"), Some("qa"));
        assert_eq!(c.resolve("nope"), None);

        let text = fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(
            !text.contains("token"),
            "tokens must never end up in connections.toml"
        );
    }
}
