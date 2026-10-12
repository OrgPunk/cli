//! Org authentication: web + PKCE, JWT bearer, refresh, connection and token storage.
//!
//! Connections (alias → Org Id, instance URL, username, login method) live in
//! `<global dir>/connections.toml`; tokens live in the OS keychain or, without one,
//! in an encrypted file. Tokens never go into connections.toml.

mod connections;
pub mod oauth;
mod secrets;

pub use connections::{AuthMethod, Connection, Connections, OrgType};
pub use oauth::{OAuthError, TokenResponse, UserInfo};
pub use secrets::{SecretStore, StoreKind, Tokens};

/// The sf cli client id. Present in every org, no secret, login via PKCE.
pub const DEFAULT_CLIENT_ID: &str = "PlatformCLI";
pub const PRODUCTION_LOGIN_URL: &str = "https://login.salesforce.com";
pub const SANDBOX_LOGIN_URL: &str = "https://test.salesforce.com";
/// Callback port. `PlatformCLI` only accepts `http://localhost:1717/OauthRedirect`.
pub const CALLBACK_PORT: u16 = 1717;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    OAuth(#[from] OAuthError),
    #[error("connection `{0}` not found; see `opk org list`")]
    UnknownAlias(String),
    #[error("no stored tokens for `{0}`; run `opk org login` again")]
    NoTokens(String),
    #[error("secret store: {0}")]
    Secrets(String),
    #[error("{path}: {message}")]
    File {
        path: std::path::PathBuf,
        message: String,
    },
    #[error("JWT key {path}: {source}")]
    JwtKey {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

/// An active session: where to call and with which token.
#[derive(Debug, Clone)]
pub struct Session {
    pub alias: String,
    pub org_id: String,
    pub instance_url: String,
    pub access_token: String,
}

/// Connections and tokens together.
pub struct Auth {
    pub connections: Connections,
    pub secrets: SecretStore,
    http: reqwest::blocking::Client,
}

impl Auth {
    /// Connections and secrets from the global opk directory.
    pub fn open(dir: &std::path::Path, http: reqwest::blocking::Client) -> Result<Self> {
        Ok(Self::new(
            Connections::load(dir)?,
            SecretStore::open(dir)?,
            http,
        ))
    }

    pub fn new(
        connections: Connections,
        secrets: SecretStore,
        http: reqwest::blocking::Client,
    ) -> Self {
        Self {
            connections,
            secrets,
            http,
        }
    }

    pub fn http(&self) -> &reqwest::blocking::Client {
        &self.http
    }

    /// Saves a connection after login. An existing alias is overwritten.
    pub fn save(&mut self, alias: &str, conn: Connection, tokens: &Tokens) -> Result<()> {
        self.secrets.put(&conn.secret_key(), tokens)?;
        self.connections.insert(alias, conn);
        self.connections.save()
    }

    pub fn session(&self, alias: &str) -> Result<Session> {
        let conn = self.connection(alias)?;
        let tokens = self
            .secrets
            .get(&conn.secret_key())?
            .ok_or_else(|| Error::NoTokens(alias.to_owned()))?;
        Ok(Session {
            alias: alias.to_owned(),
            org_id: conn.org_id.clone(),
            instance_url: conn.instance_url.clone(),
            access_token: tokens.access_token,
        })
    }

    /// A new access token: refresh grant for web, a new JWT for jwt.
    pub fn refresh(&self, alias: &str) -> Result<Session> {
        let conn = self.connection(alias)?;
        let key = conn.secret_key();
        let old = self
            .secrets
            .get(&key)?
            .ok_or_else(|| Error::NoTokens(alias.to_owned()))?;
        let resp = match &conn.auth {
            AuthMethod::Web => {
                let rt = old
                    .refresh_token
                    .as_deref()
                    .ok_or_else(|| Error::NoTokens(alias.to_owned()))?;
                oauth::refresh(&self.http, &conn.login_url, &conn.client_id, rt)?
            }
            AuthMethod::Jwt { key_path } => {
                let pem = std::fs::read(key_path).map_err(|source| Error::JwtKey {
                    path: key_path.clone(),
                    source,
                })?;
                oauth::jwt(
                    &self.http,
                    &conn.login_url,
                    &conn.client_id,
                    &conn.username,
                    &pem,
                )?
            }
        };
        // Without rotation the refresh grant returns no refresh token, so keep the old one.
        let tokens = Tokens {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token.or(old.refresh_token),
        };
        self.secrets.put(&key, &tokens)?;
        Ok(Session {
            alias: alias.to_owned(),
            org_id: conn.org_id.clone(),
            instance_url: resp.instance_url,
            access_token: tokens.access_token,
        })
    }

    /// Revokes the token in the org (best effort) and removes the connection.
    /// Returns the revoke error if revoking failed; the connection is removed either way.
    pub fn logout(&mut self, alias: &str) -> Result<Option<OAuthError>> {
        let conn = self.connection(alias)?.clone();
        let key = conn.secret_key();
        let mut revoke_error = None;
        if let Some(tokens) = self.secrets.get(&key)? {
            let token = tokens
                .refresh_token
                .as_deref()
                .unwrap_or(&tokens.access_token);
            revoke_error = oauth::revoke(&self.http, &conn.login_url, token).err();
        }
        // Delete the tokens only if no other alias uses this org + user.
        self.connections.remove(alias);
        if !self.connections.iter().any(|(_, c)| c.secret_key() == key) {
            self.secrets.delete(&key)?;
        }
        self.connections.save()?;
        Ok(revoke_error)
    }

    pub fn connection(&self, alias: &str) -> Result<&Connection> {
        self.connections
            .get(alias)
            .ok_or_else(|| Error::UnknownAlias(alias.to_owned()))
    }
}
