//! Shared command state: config, HTTP, connections, API client for an org.

use std::time::Duration;

use orgpunk_auth::{Auth, Connection};
use orgpunk_config::Config;
use orgpunk_sfapi::{Client, Credentials, DEFAULT_API_VERSION};

use crate::output::Failure;

pub struct Ctx {
    pub config: Config,
    pub http: reqwest::blocking::Client,
    /// CI mode: no prompts, auth only from the environment.
    pub ci: bool,
}

/// An org session from the CI environment: JWT bearer, tokens only in memory.
pub struct CiSession {
    pub client: Client,
    pub org_id: String,
    pub user_id: String,
    pub username: String,
}

/// Variables of a CI login.
pub const CI_VARS: [&str; 4] = [
    "ORGPUNK_CLIENT_ID",
    "ORGPUNK_JWT_KEY",
    "ORGPUNK_USERNAME",
    "ORGPUNK_INSTANCE_URL",
];

impl Ctx {
    pub fn load(ci: bool) -> Result<Self, Failure> {
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("opk/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            // Uploading a large zip can take a while.
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self {
            config: Config::load()?,
            http,
            ci,
        })
    }

    pub fn auth(&self) -> Result<Auth, Failure> {
        Ok(Auth::open(self.config.global_dir(), self.http.clone())?)
    }

    pub fn api_version(&self) -> Result<String, Failure> {
        Ok(self
            .config
            .get("api-version")?
            .map_or_else(|| DEFAULT_API_VERSION.to_owned(), |r| r.value))
    }

    /// Org alias: explicit `-o/--org`/argument or `target-org`. Accepts an alias, username or Org Id.
    pub fn resolve_alias(&self, auth: &Auth, explicit: Option<&str>) -> Result<String, Failure> {
        let name = match explicit {
            Some(n) => n.to_owned(),
            None => self.config.get("target-org")?.map(|r| r.value).ok_or_else(|| {
                Failure::error("no org given and target-org is not set; pass -o <alias> or run `opk org use <alias>`")
            })?,
        };
        auth.connections
            .resolve(&name)
            .map(str::to_owned)
            .ok_or_else(|| {
                Failure::error(format!("connection `{name}` not found; see `opk org list`"))
            })
    }

    /// JWT login from the environment (CI mode). `ORGPUNK_JWT_KEY` is the PEM itself or a path to
    /// it; `ORGPUNK_INSTANCE_URL` is the login URL (My Domain, or test.salesforce.com for a
    /// sandbox).
    pub fn ci_session(&self) -> Result<CiSession, Failure> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let missing: Vec<&str> = CI_VARS
            .iter()
            .copied()
            .filter(|k| var(k).is_none())
            .collect();
        if !missing.is_empty() {
            return Err(Failure::error(format!(
                "CI mode logs in only from the environment; set {}",
                missing.join(", ")
            )));
        }
        let [client_id, key, username, login_url] = CI_VARS.map(|k| var(k).expect("checked"));
        let pem = if key.trim_start().starts_with("-----BEGIN") {
            key.into_bytes()
        } else {
            std::fs::read(&key)
                .map_err(|e| Failure::error(format!("ORGPUNK_JWT_KEY {key}: {e}")))?
        };
        let login_url = login_url.trim_end_matches('/').to_owned();
        let resp = orgpunk_auth::oauth::jwt(&self.http, &login_url, &client_id, &username, &pem)?;
        // The identity URL ends in /id/<OrgId>/<UserId>.
        let mut ids = resp.id.rsplit('/');
        let (user_id, org_id) = match (ids.next(), ids.next()) {
            (Some(u), Some(o)) if o.starts_with("00D") => (u.to_owned(), o.to_owned()),
            _ => {
                return Err(Failure::error(format!(
                    "unexpected identity URL {}",
                    resp.id
                )));
            }
        };
        let client = Client::new(
            self.http.clone(),
            Credentials {
                instance_url: resp.instance_url,
                access_token: resp.access_token,
            },
            self.api_version()?,
        );
        Ok(CiSession {
            client,
            org_id,
            user_id,
            username,
        })
    }

    /// API client for a connection; refreshes the token itself on 401.
    pub fn client(&self, auth: &Auth, alias: &str) -> Result<(Client, Connection), Failure> {
        let conn = auth.connection(alias)?.clone();
        let session = auth.session(alias)?;
        let creds = Credentials {
            instance_url: session.instance_url,
            access_token: session.access_token,
        };
        let dir = self.config.global_dir().to_owned();
        let http = self.http.clone();
        let alias = alias.to_owned();
        let client =
            Client::new(self.http.clone(), creds, self.api_version()?).with_refresher(move || {
                let auth = Auth::open(&dir, http.clone()).map_err(|e| e.to_string())?;
                let s = auth.refresh(&alias).map_err(|e| e.to_string())?;
                Ok(Credentials {
                    instance_url: s.instance_url,
                    access_token: s.access_token,
                })
            });
        Ok((client, conn))
    }
}
