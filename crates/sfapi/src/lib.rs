//! Salesforce REST API client: Metadata deploy, SOQL, limits.
//!
//! The client does not know where tokens come from: on 401 it calls
//! `refresher` once and retries, so `sfapi` does not depend on `auth`.

mod deploy;
mod metadata;

use std::cell::RefCell;

use reqwest::blocking::{Client as Http, RequestBuilder, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;

pub use deploy::{
    CodeCoverageWarning, ComponentMessage, DeployDetails, DeployOptions, DeployResult,
    DeployStatus, RunTestResult, TestFailure, TestLevel,
};
pub use metadata::{FileProperties, ListQuery, MetadataObject, RetrieveStatus, Retrieved};

/// Default API version when `api-version` is not set.
pub const DEFAULT_API_VERSION: &str = "66.0";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Salesforce API {status}: {code}: {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },
    #[error("session is invalid and the token could not be refreshed: {0}")]
    Unauthorized(String),
    #[error("unexpected Salesforce response: {0}")]
    Decode(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Org address and access token.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub instance_url: String,
    pub access_token: String,
}

type Refresher = Box<dyn Fn() -> std::result::Result<Credentials, String> + Send>;

pub struct Client {
    http: Http,
    version: String,
    creds: RefCell<Credentials>,
    refresher: Option<Refresher>,
}

impl Client {
    pub fn new(http: Http, creds: Credentials, version: impl Into<String>) -> Self {
        Self {
            http,
            version: version.into(),
            creds: RefCell::new(creds),
            refresher: None,
        }
    }

    /// A client with the same session for another thread; it cannot refresh the token, so it
    /// is meant for a short burst of parallel calls.
    pub fn fork(&self) -> Self {
        Self::new(
            self.http.clone(),
            self.creds.borrow().clone(),
            self.version.clone(),
        )
    }

    /// How to get a new token after a 401.
    pub fn with_refresher(
        mut self,
        f: impl Fn() -> std::result::Result<Credentials, String> + Send + 'static,
    ) -> Self {
        self.refresher = Some(Box::new(f));
        self
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn instance_url(&self) -> String {
        self.creds.borrow().instance_url.clone()
    }

    /// REST resource URL: `path` is relative to `/services/data/vXX.0/`.
    fn url(&self, path: &str) -> String {
        format!(
            "{}/services/data/v{}/{}",
            self.creds.borrow().instance_url.trim_end_matches('/'),
            self.version,
            path.trim_start_matches('/')
        )
    }

    /// Sends a request; on 401 refreshes the token and retries once.
    fn send(&self, build: impl Fn(&Http, &str) -> RequestBuilder) -> Result<Response> {
        let token = self.creds.borrow().access_token.clone();
        let resp = build(&self.http, &token).bearer_auth(&token).send()?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return check(resp);
        }
        let Some(refresh) = &self.refresher else {
            return check(resp);
        };
        let fresh = refresh().map_err(Error::Unauthorized)?;
        let token = fresh.access_token.clone();
        *self.creds.borrow_mut() = fresh;
        check(build(&self.http, &token).bearer_auth(&token).send()?)
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.url(path);
        decode(self.send(|h, _| h.get(&url))?)
    }

    /// SOQL query, all pages.
    pub fn query<T: DeserializeOwned>(&self, soql: &str) -> Result<Vec<T>> {
        self.query_at("query", soql)
    }

    fn query_at<T: DeserializeOwned>(&self, resource: &str, soql: &str) -> Result<Vec<T>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page<T> {
            records: Vec<T>,
            #[serde(default)]
            next_records_url: Option<String>,
        }
        let first = url::Url::parse_with_params(&self.url(resource), &[("q", soql)])
            .map_err(|e| Error::Decode(e.to_string()))?;
        let mut page: Page<T> = decode(self.send(|h, _| h.get(first.clone()))?)?;
        let mut out = std::mem::take(&mut page.records);
        while let Some(next) = page.next_records_url.take() {
            let url = format!(
                "{}{next}",
                self.creds.borrow().instance_url.trim_end_matches('/')
            );
            page = decode(self.send(|h, _| h.get(&url))?)?;
            out.append(&mut page.records);
        }
        Ok(out)
    }

    /// The newest API version the org supports, e.g. `66.0`.
    pub fn latest_api_version(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct Version {
            version: String,
        }
        let url = format!(
            "{}/services/data/",
            self.creds.borrow().instance_url.trim_end_matches('/')
        );
        let versions: Vec<Version> = decode(self.send(|h, _| h.get(&url))?)?;
        versions
            .into_iter()
            .map(|v| v.version)
            .max_by(|a, b| {
                let n = |s: &str| s.parse::<f64>().unwrap_or(0.0);
                n(a).total_cmp(&n(b))
            })
            .ok_or_else(|| Error::Decode("no API versions".into()))
    }

    /// Tooling API SOQL query, all pages.
    pub fn tooling_query<T: DeserializeOwned>(&self, soql: &str) -> Result<Vec<T>> {
        self.query_at("tooling/query", soql)
    }

    /// Whether the org has an sObject (e.g. ScratchOrgInfo marks a Dev Hub).
    pub fn has_sobject(&self, name: &str) -> Result<bool> {
        let url = self.url(&format!("sobjects/{name}/describe"));
        match self.send(|h, _| h.get(&url)) {
            Ok(_) => Ok(true),
            Err(Error::Api { status: 404, .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Org limits: name → (Max, Remaining).
    pub fn limits(&self) -> Result<std::collections::BTreeMap<String, Limit>> {
        self.get_json("limits")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Limit {
    pub max: i64,
    pub remaining: i64,
}

fn check(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().unwrap_or_default();
    Err(api_error(status.as_u16(), &body))
}

/// REST errors come as an array `[{"errorCode": ..., "message": ...}]`.
fn api_error(status: u16, body: &str) -> Error {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Item {
        error_code: String,
        message: String,
    }
    match serde_json::from_str::<Vec<Item>>(body) {
        Ok(items) if !items.is_empty() => Error::Api {
            status,
            code: items[0].error_code.clone(),
            message: items
                .iter()
                .map(|i| i.message.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        },
        _ => Error::Api {
            status,
            code: format!("HTTP_{status}"),
            message: body.chars().take(300).collect(),
        },
    }
}

fn decode<T: DeserializeOwned>(resp: Response) -> Result<T> {
    let text = resp.text()?;
    serde_json::from_str(&text).map_err(|e| Error::Decode(format!("{e}: {}", truncate(&text))))
}

fn truncate(s: &str) -> String {
    s.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_from_salesforce_array() {
        let e = api_error(
            400,
            r#"[{"message":"bad field","errorCode":"INVALID_FIELD"}]"#,
        );
        assert!(matches!(e, Error::Api { status: 400, ref code, .. } if code == "INVALID_FIELD"));
    }

    #[test]
    fn api_error_from_html() {
        let e = api_error(502, "<html>bad gateway</html>");
        assert!(matches!(e, Error::Api { status: 502, ref code, .. } if code == "HTTP_502"));
    }
}
