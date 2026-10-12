//! Salesforce OAuth flows: web server + PKCE, JWT bearer, refresh, revoke, userinfo.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("network: {0}")]
    Network(#[from] reqwest::Error),
    #[error("{}", rejected_message(error, description))]
    Rejected {
        status: u16,
        error: String,
        description: String,
    },
    #[error("browser login: {0}")]
    Callback(String),
    #[error("JWT key: {0}")]
    JwtKey(#[from] jsonwebtoken::errors::Error),
}

impl OAuthError {
    /// The refresh token was revoked or expired; a new login is needed.
    pub fn is_invalid_grant(&self) -> bool {
        matches!(self, Self::Rejected { error, .. } if error == "invalid_grant")
    }
}

fn rejected_message(error: &str, description: &str) -> String {
    let hint = if description.contains("refresh_token scope is required") {
        "\nAn External Client App used for JWT needs: the `refresh_token, offline_access` scope, \
         Permitted Users = Admin approved users are pre-authorized with the user's profile or permission set, \
         and an uploaded certificate (Flow Enablement → JWT Bearer Flow)."
    } else if error == "invalid_grant" && description.contains("expired") {
        "\nThe token expired or was revoked; run `opk org login` again."
    } else if error == "invalid_client_id" {
        "\nWrong client id, or Salesforce has not applied the app yet (up to 10 minutes after creation)."
    } else {
        ""
    };
    format!("Salesforce rejected the request: {error}: {description}{hint}")
}

#[derive(Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub instance_url: String,
    /// URL identity service: `.../id/<OrgId>/<UserId>`.
    pub id: String,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("instance_url", &self.instance_url)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    pub organization_id: String,
    pub user_id: String,
    pub username: String,
}

/// Redirect URI for a callback port.
pub fn redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}/OauthRedirect")
}

/// Web server flow + PKCE. `on_url` receives the login page URL (to print it or open a browser).
pub fn web(
    http: &Client,
    login_url: &str,
    client_id: &str,
    port: u16,
    on_url: impl FnOnce(&Url),
) -> Result<TokenResponse, OAuthError> {
    // Bind the port before showing the URL so a busy port fails right away.
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| OAuthError::Callback(format!("cannot bind localhost:{port}: {e}")))?;
    let redirect = redirect_uri(port);
    let verifier = random_b64(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64(16);
    let authorize = Url::parse_with_params(
        &format!(
            "{}/services/oauth2/authorize",
            login_url.trim_end_matches('/')
        ),
        &[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", &redirect),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("scope", "api refresh_token web"),
        ],
    )
    .map_err(|e| OAuthError::Callback(format!("login URL {login_url}: {e}")))?;
    on_url(&authorize);

    let params = wait_for_callback(&listener)?;
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if let Some(error) = get("error") {
        return Err(OAuthError::Rejected {
            status: 0,
            error: error.to_owned(),
            description: get("error_description").unwrap_or_default().to_owned(),
        });
    }
    if get("state") != Some(state.as_str()) {
        return Err(OAuthError::Callback("state mismatch, login aborted".into()));
    }
    let code = get("code").ok_or_else(|| OAuthError::Callback("no code in the callback".into()))?;
    token(
        http,
        login_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
        ],
    )
}

/// JWT bearer. `aud` follows the login URL: sandbox → test.salesforce.com.
pub fn jwt(
    http: &Client,
    login_url: &str,
    client_id: &str,
    username: &str,
    key_pem: &[u8],
) -> Result<TokenResponse, OAuthError> {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        + 180;
    let claims = serde_json::json!({
        "iss": client_id,
        "sub": username,
        "aud": jwt_audience(login_url),
        "exp": exp,
    });
    let assertion = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_rsa_pem(key_pem)?,
    )?;
    token(
        http,
        login_url,
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &assertion),
        ],
    )
}

pub fn refresh(
    http: &Client,
    login_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse, OAuthError> {
    token(
        http,
        login_url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ],
    )
}

/// Exchanges a ScratchOrgInfo AuthCode for tokens (no PKCE, no secret).
pub fn exchange_code(
    http: &Client,
    login_url: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, OAuthError> {
    token(
        http,
        login_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
        ],
    )
}

pub fn revoke(http: &Client, login_url: &str, token: &str) -> Result<(), OAuthError> {
    let resp = http
        .post(format!(
            "{}/services/oauth2/revoke",
            login_url.trim_end_matches('/')
        ))
        .form(&[("token", token)])
        .send()?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    Err(rejected(status.as_u16(), &resp.text().unwrap_or_default()))
}

/// Whether an access token works: `true` on 200, `false` on 401/403.
pub fn token_valid(
    http: &Client,
    instance_url: &str,
    access_token: &str,
) -> Result<bool, OAuthError> {
    let url = format!(
        "{}/services/oauth2/userinfo",
        instance_url.trim_end_matches('/')
    );
    let resp = http.get(url).bearer_auth(access_token).send()?;
    match resp.status().as_u16() {
        200..=299 => Ok(true),
        401 | 403 => Ok(false),
        status => Err(rejected(status, &resp.text().unwrap_or_default())),
    }
}

/// User and org for a token. 401 means the token is invalid.
pub fn userinfo(http: &Client, id_url: &str, access_token: &str) -> Result<UserInfo, OAuthError> {
    let resp = http.get(id_url).bearer_auth(access_token).send()?;
    let status = resp.status();
    if !status.is_success() {
        return Err(rejected(status.as_u16(), &resp.text().unwrap_or_default()));
    }
    Ok(resp.json()?)
}

fn token(
    http: &Client,
    login_url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse, OAuthError> {
    let resp = http
        .post(format!(
            "{}/services/oauth2/token",
            login_url.trim_end_matches('/')
        ))
        .form(form)
        .send()?;
    let status = resp.status();
    if !status.is_success() {
        return Err(rejected(status.as_u16(), &resp.text().unwrap_or_default()));
    }
    Ok(resp.json()?)
}

fn rejected(status: u16, body: &str) -> OAuthError {
    #[derive(Deserialize)]
    struct Body {
        error: String,
        #[serde(default)]
        error_description: String,
    }
    match serde_json::from_str::<Body>(body) {
        Ok(b) => OAuthError::Rejected {
            status,
            error: b.error,
            description: b.error_description,
        },
        Err(_) => OAuthError::Rejected {
            status,
            error: format!("http_{status}"),
            description: body.chars().take(300).collect(),
        },
    }
}

fn jwt_audience(login_url: &str) -> &'static str {
    let host = Url::parse(login_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    if host == "test.salesforce.com" || host.ends_with(".sandbox.my.salesforce.com") {
        crate::SANDBOX_LOGIN_URL
    } else {
        crate::PRODUCTION_LOGIN_URL
    }
}

/// Waits for a request to /OauthRedirect and returns its query parameters.
fn wait_for_callback(listener: &TcpListener) -> Result<Vec<(String, String)>, OAuthError> {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        // Browsers open idle preconnect connections: without a timeout the server
        // would block on them while the request with the code waits in the queue.
        if stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .is_err()
        {
            continue;
        }
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() || line.is_empty() {
            continue;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/");
        if !path.starts_with("/OauthRedirect") {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            continue;
        }
        let url = Url::parse(&format!("http://localhost{path}"))
            .map_err(|e| OAuthError::Callback(e.to_string()))?;
        let params: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let ok = params.iter().all(|(k, _)| k != "error");
        let body = if ok {
            "opk: logged in, you can close this tab."
        } else {
            "opk: login failed, see the terminal for details."
        };
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        return Ok(params);
    }
    Err(OAuthError::Callback("callback server stopped".into()))
}

fn random_b64(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audience_by_login_url() {
        assert_eq!(
            jwt_audience("https://login.salesforce.com"),
            crate::PRODUCTION_LOGIN_URL
        );
        assert_eq!(
            jwt_audience("https://test.salesforce.com"),
            crate::SANDBOX_LOGIN_URL
        );
        assert_eq!(
            jwt_audience("https://acme--uat.sandbox.my.salesforce.com"),
            crate::SANDBOX_LOGIN_URL
        );
        assert_eq!(
            jwt_audience("https://acme.my.salesforce.com"),
            crate::PRODUCTION_LOGIN_URL
        );
    }

    #[test]
    fn rejected_parses_salesforce_error() {
        let e = rejected(
            400,
            r#"{"error":"invalid_grant","error_description":"expired access/refresh token"}"#,
        );
        assert!(e.is_invalid_grant());
        assert!(e.to_string().contains("opk org login"));
    }

    #[test]
    fn rejected_explains_eca_scope() {
        let e = rejected(
            400,
            r#"{"error":"invalid_request","error_description":"refresh_token scope is required and the connected app should be installed and preauthorized."}"#,
        );
        assert!(e.to_string().contains("offline_access"));
    }

    #[test]
    fn rejected_non_json_body() {
        let e = rejected(503, "<html>down</html>");
        assert!(matches!(e, OAuthError::Rejected { status: 503, .. }));
    }

    #[test]
    fn callback_skips_idle_preconnect() {
        use std::io::Read;
        use std::net::TcpStream;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let idle = TcpStream::connect(addr).unwrap();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(b"GET /OauthRedirect?code=abc&state=xyz HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        });
        let params = wait_for_callback(&listener).unwrap();
        drop(idle);
        assert!(params.contains(&("code".into(), "abc".into())));
        assert!(client.join().unwrap().starts_with("HTTP/1.1 200"));
    }
}
