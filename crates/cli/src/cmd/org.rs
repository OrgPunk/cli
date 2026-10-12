//! `opk org login / list / use / logout`: connections, nothing of the project.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use orgpunk_auth::{
    AuthMethod, CALLBACK_PORT, Connection, DEFAULT_CLIENT_ID, OrgType, PRODUCTION_LOGIN_URL,
    SANDBOX_LOGIN_URL, Tokens, oauth,
};
use orgpunk_config::Scope;
use orgpunk_sfapi::{Client, Credentials};
use serde_json::{Value, json};

use crate::context::Ctx;
use crate::output::{Done, Failure, Output};

#[derive(Subcommand)]
pub enum Command {
    /// Log in to an org via the browser or JWT and save the connection.
    Login(LoginArgs),
    /// List connections, the state of their tokens and their environments.
    List,
    /// Make the org the default target of status, pull and push.
    Use {
        /// Alias, username or Org Id of a connection.
        #[arg(add = crate::complete::orgs())]
        alias: String,
        /// Write to the global config instead of the project.
        #[arg(long)]
        global: bool,
    },
    /// Take over the connections of sf cli (it must be installed): all its orgs, or the named
    /// ones. Web logins move with their refresh token; JWT ones need `opk org login --jwt`.
    Import {
        /// Aliases or usernames in sf cli.
        names: Vec<String>,
    },
    /// Revoke the token and remove the connection.
    Logout {
        /// Alias, username or Org Id of a connection.
        #[arg(add = crate::complete::orgs())]
        alias: String,
    },
}

#[derive(Args)]
pub struct LoginArgs {
    /// Connection alias; defaults to the username.
    #[arg(long, short)]
    alias: Option<String>,
    /// Sandbox: log in via test.salesforce.com.
    #[arg(long, conflicts_with = "instance_url")]
    sandbox: bool,
    /// My Domain or another login URL.
    #[arg(long)]
    instance_url: Option<String>,
    /// Client id; defaults to PlatformCLI for browser login. Required with --jwt.
    #[arg(long)]
    client_id: Option<String>,
    /// JWT bearer (CI): requires --client-id, --key, --username.
    #[arg(long, requires_all = ["client_id", "key", "username"])]
    jwt: bool,
    /// RSA private key (PEM) for JWT.
    #[arg(long, requires = "jwt")]
    key: Option<PathBuf>,
    /// The username to log in as with JWT.
    #[arg(long, requires = "jwt")]
    username: Option<String>,
    /// Make the org the default target.
    #[arg(long)]
    set_default: bool,
    /// Do not open a browser, only print the URL.
    #[arg(long)]
    no_browser: bool,
}

pub fn run(ctx: &Ctx, out: &Output, cmd: Command) -> Result<Done, Failure> {
    match cmd {
        Command::Login(args) => login(ctx, out, args),
        Command::List => list(ctx, out),
        Command::Use { alias, global } => use_org(ctx, out, &alias, global),
        Command::Import { names } => import_sf(ctx, out, &names),
        Command::Logout { alias } => logout(ctx, out, &alias),
    }
}

fn login(ctx: &Ctx, out: &Output, a: LoginArgs) -> Result<Done, Failure> {
    let login_url = match (&a.instance_url, a.sandbox) {
        (Some(url), _) => url.trim_end_matches('/').to_owned(),
        (None, true) => SANDBOX_LOGIN_URL.to_owned(),
        (None, false) => PRODUCTION_LOGIN_URL.to_owned(),
    };
    let http = &ctx.http;
    let (resp, auth_method, client_id) = if a.jwt {
        let key = std::fs::canonicalize(a.key.as_ref().expect("clap: requires"))
            .map_err(|e| Failure::error(format!("JWT key: {e}")))?;
        let pem =
            std::fs::read(&key).map_err(|e| Failure::error(format!("{}: {e}", key.display())))?;
        let client_id = a.client_id.clone().expect("clap: requires");
        let username = a.username.as_deref().expect("clap: requires");
        let resp = oauth::jwt(http, &login_url, &client_id, username, &pem)?;
        (resp, AuthMethod::Jwt { key_path: key }, client_id)
    } else {
        let client_id = a
            .client_id
            .clone()
            .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_owned());
        let resp = oauth::web(http, &login_url, &client_id, CALLBACK_PORT, |url| {
            out.progress(format!("Log in to Salesforce in the browser:\n\n  {url}\n"));
            if !a.no_browser && can_open_browser() {
                let _ = webbrowser::open(url.as_str());
            }
        })?;
        (resp, AuthMethod::Web, client_id)
    };
    let json = connect(
        ctx,
        out,
        a.alias.as_deref(),
        resp,
        login_url,
        client_id,
        auth_method,
    )?;
    if a.set_default {
        set_target(ctx, json["alias"].as_str().unwrap_or_default(), false)?;
    }
    Ok(Done::ok(json))
}

/// Saves a connection from fresh tokens: who the user is, what kind of org. `alias` defaults
/// to the username. Returns the JSON of the connection.
fn connect(
    ctx: &Ctx,
    out: &Output,
    alias: Option<&str>,
    resp: oauth::TokenResponse,
    login_url: String,
    client_id: String,
    auth_method: AuthMethod,
) -> Result<Value, Failure> {
    let http = &ctx.http;
    let me = oauth::userinfo(http, &resp.id, &resp.access_token)?;
    let api = Client::new(
        http.clone(),
        Credentials {
            instance_url: resp.instance_url.clone(),
            access_token: resp.access_token.clone(),
        },
        ctx.api_version()?,
    );
    let org_type = detect_org_type(&api)?;
    // Only prod / Developer Edition orgs can be a Dev Hub; skip the describe on sandboxes.
    let dev_hub = org_type == OrgType::Production && api.has_sobject("ScratchOrgInfo")?;

    let alias = alias.map_or_else(|| me.username.clone(), str::to_owned);
    let conn = Connection {
        org_id: me.organization_id.clone(),
        user_id: me.user_id.clone(),
        username: me.username.clone(),
        instance_url: resp.instance_url.clone(),
        login_url,
        client_id,
        auth: auth_method,
        org_type: Some(org_type),
        dev_hub,
    };
    let mut auth = ctx.auth()?;
    auth.save(
        &alias,
        conn,
        &Tokens {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token,
        },
    )?;
    out.say(format!(
        "Connected: {alias} → {} ({}, {org_type}{})",
        me.username,
        me.organization_id,
        if dev_hub { ", Dev Hub" } else { "" }
    ));
    if auth.secrets.kind() == orgpunk_auth::StoreKind::File {
        out.progress(
            "Keychain is not available: tokens are stored in an encrypted file in the opk config directory.",
        );
    }
    Ok(json!({
        "alias": alias,
        "username": me.username,
        "orgId": me.organization_id,
        "instanceUrl": resp.instance_url,
        "orgType": org_type.to_string(),
        "devHub": dev_hub,
    }))
}

fn detect_org_type(api: &Client) -> Result<OrgType, Failure> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Org {
        is_sandbox: bool,
        trial_expiration_date: Option<String>,
    }
    let orgs: Vec<Org> = api.query("SELECT IsSandbox, TrialExpirationDate FROM Organization")?;
    let org = orgs
        .first()
        .ok_or_else(|| Failure::error("the Organization query returned no rows"))?;
    Ok(match (org.is_sandbox, &org.trial_expiration_date) {
        (true, Some(_)) => OrgType::Scratch,
        (true, None) => OrgType::Sandbox,
        (false, _) => OrgType::Production,
    })
}

/// On Linux without a graphical session `webbrowser` may launch a text browser right in the terminal.
fn can_open_browser() -> bool {
    if cfg!(target_os = "linux") {
        ["DISPLAY", "WAYLAND_DISPLAY", "BROWSER"]
            .iter()
            .any(|v| std::env::var_os(v).is_some())
    } else {
        true
    }
}

fn list(ctx: &Ctx, out: &Output) -> Result<Done, Failure> {
    let auth = ctx.auth()?;
    let target = ctx
        .config
        .get("target-org")?
        .and_then(|r| auth.connections.resolve(&r.value).map(str::to_owned));
    let conns: Vec<(&String, &Connection)> = auth.connections.iter().collect();
    // Master and protect come from the project, by Org Id.
    let project = match ctx.config.project_root() {
        Some(root) => orgpunk_config::project::ProjectConfig::load(root)?,
        None => orgpunk_config::project::ProjectConfig::default(),
    };
    // Check tokens in parallel: each check is a network request.
    let statuses: Vec<&'static str> = std::thread::scope(|s| {
        let handles: Vec<_> = conns
            .iter()
            .map(|(alias, conn)| s.spawn(|| token_status(&auth, alias, conn)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or("unknown"))
            .collect()
    });

    if conns.is_empty() {
        out.say("No connections. Log in with: opk org login --alias <alias>");
    }
    let mut rows = Vec::new();
    for ((alias, c), status) in conns.iter().zip(&statuses) {
        let is_target = target.as_deref() == Some(alias.as_str());
        let kind = c.org_type.map_or("?".to_owned(), |t| t.to_string())
            + if c.dev_hub { "+devhub" } else { "" };
        // The environments of this org: `prod (master)`.
        let envs: Vec<String> = project
            .envs_of(&c.org_id)
            .map(|e| {
                if e.master {
                    format!("{} (master)", e.name)
                } else {
                    e.name.clone()
                }
            })
            .collect();
        let protect: Vec<&str> = orgpunk_env::protect::Rules::for_org(&project, &c.org_id)
            .policies
            .iter()
            .map(|p| p.as_str())
            .collect();
        out.say(format!(
            "{} {:<20} {:<40} {:<18} {:<15} {:<8} {}{}",
            if is_target { "*" } else { " " },
            alias,
            c.username,
            c.org_id,
            kind,
            status,
            if envs.is_empty() {
                String::new()
            } else {
                format!("env {} ", envs.join(", "))
            },
            protect.join(",")
        ));
        rows.push(json!({
            "alias": alias,
            "username": c.username,
            "orgId": c.org_id,
            "instanceUrl": c.instance_url,
            "orgType": c.org_type.map(|t| t.to_string()),
            "devHub": c.dev_hub,
            "auth": match c.auth { AuthMethod::Web => "web", AuthMethod::Jwt { .. } => "jwt" },
            "target": is_target,
            "token": status,
            "master": project.is_master(&c.org_id),
            "envs": project.envs_of(&c.org_id).map(|e| e.name.clone()).collect::<Vec<_>>(),
            "protect": protect,
        }));
    }
    Ok(Done::ok(Value::Array(rows)))
}

/// valid: the token works (possibly after a refresh); expired: the refresh failed;
/// revoked: the refresh token was revoked; missing: no tokens; unknown: network error.
fn token_status(auth: &orgpunk_auth::Auth, alias: &str, conn: &Connection) -> &'static str {
    let Ok(session) = auth.session(alias) else {
        return "missing";
    };
    match oauth::token_valid(auth.http(), &conn.instance_url, &session.access_token) {
        Ok(true) => return "valid",
        Ok(false) => {}
        Err(_) => return "unknown",
    }
    match auth.refresh(alias) {
        Ok(_) => "valid",
        Err(orgpunk_auth::Error::OAuth(e)) if e.is_invalid_grant() => "revoked",
        Err(orgpunk_auth::Error::OAuth(oauth::OAuthError::Network(_))) => "unknown",
        Err(_) => "expired",
    }
}

fn set_target(ctx: &Ctx, alias: &str, global: bool) -> Result<Scope, Failure> {
    let scope = if global || ctx.config.project_root().is_none() {
        Scope::Global
    } else {
        Scope::Local
    };
    ctx.config.set(scope, "target-org", alias)?;
    Ok(scope)
}

fn use_org(ctx: &Ctx, out: &Output, alias: &str, global: bool) -> Result<Done, Failure> {
    let auth = ctx.auth()?;
    let alias = ctx.resolve_alias(&auth, Some(alias))?;
    let scope = set_target(ctx, &alias, global)?;
    let where_ = if scope == Scope::Global {
        "globally"
    } else {
        "in the project"
    };
    out.say(format!("target-org = {alias} ({where_})"));
    Ok(Done::ok(
        json!({ "alias": alias, "global": scope == Scope::Global }),
    ))
}

fn logout(ctx: &Ctx, out: &Output, alias: &str) -> Result<Done, Failure> {
    let mut auth = ctx.auth()?;
    let alias = ctx.resolve_alias(&auth, Some(alias))?;
    let revoke_error = auth.logout(&alias)?;
    if let Some(e) = &revoke_error {
        out.progress(format!(
            "Could not revoke the token in the org ({e}); the local connection was removed."
        ));
    }
    if ctx
        .config
        .get("target-org")?
        .is_some_and(|r| r.value == alias)
    {
        out.progress(format!(
            "target-org still points to {alias}; set a new one with: opk org use <alias>"
        ));
    }
    out.say(format!("Connection {alias} removed."));
    Ok(Done::ok(
        json!({ "alias": alias, "revoked": revoke_error.is_none() }),
    ))
}

/// An org of `sf org list`.
#[derive(Debug, PartialEq, Eq)]
struct SfOrg {
    username: String,
    alias: Option<String>,
}

/// The credentials of an sf `sfdxAuthUrl`: `force://<clientId>:<secret>:<refreshToken>@<url>`.
#[derive(Debug, PartialEq, Eq)]
struct SfAuthUrl {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    instance_url: String,
}

fn parse_auth_url(url: &str) -> Option<SfAuthUrl> {
    let rest = url.strip_prefix("force://")?;
    let (creds, host) = rest.rsplit_once('@')?;
    let mut parts = creds.splitn(3, ':');
    let (client_id, client_secret, refresh_token) = (parts.next()?, parts.next()?, parts.next()?);
    if client_id.is_empty() || refresh_token.is_empty() || host.is_empty() {
        return None;
    }
    let instance_url = if host.starts_with("http://") || host.starts_with("https://") {
        host.to_owned()
    } else {
        format!("https://{host}")
    };
    Some(SfAuthUrl {
        client_id: client_id.to_owned(),
        client_secret: client_secret.to_owned(),
        refresh_token: refresh_token.to_owned(),
        instance_url: instance_url.trim_end_matches('/').to_owned(),
    })
}

/// The orgs of `sf org list --json`, from every group it reports, once each.
fn parse_org_list(json: &Value) -> Vec<SfOrg> {
    let mut out: Vec<SfOrg> = Vec::new();
    let Some(groups) = json["result"].as_object() else {
        return out;
    };
    for org in groups.values().filter_map(Value::as_array).flatten() {
        let Some(username) = org["username"].as_str() else {
            continue;
        };
        if out.iter().any(|o| o.username == username) {
            continue;
        }
        out.push(SfOrg {
            username: username.to_owned(),
            alias: org["alias"]
                .as_str()
                .filter(|a| !a.is_empty())
                .map(str::to_owned),
        });
    }
    out
}

/// Runs sf with `--json` and returns its JSON output.
fn sf_json(args: &[&str]) -> Result<Value, Failure> {
    let program = if cfg!(windows) { "sf.cmd" } else { "sf" };
    let output = std::process::Command::new(program)
        .args(args)
        .arg("--json")
        .output()
        .map_err(|e| Failure::error(format!("cannot run sf cli ({e}); is it installed?")))?;
    serde_json::from_slice(&output.stdout).map_err(|e| {
        Failure::error(format!(
            "sf {}: unexpected output ({e}): {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    })
}

fn import_sf(ctx: &Ctx, out: &Output, names: &[String]) -> Result<Done, Failure> {
    let orgs = parse_org_list(&sf_json(&["org", "list"])?);
    let wanted: Vec<&SfOrg> = if names.is_empty() {
        orgs.iter().collect()
    } else {
        let mut v = Vec::new();
        for n in names {
            let org = orgs
                .iter()
                .find(|o| o.username == *n || o.alias.as_deref() == Some(n))
                .ok_or_else(|| Failure::error(format!("sf cli has no org `{n}`")))?;
            v.push(org);
        }
        v
    };
    if wanted.is_empty() {
        out.say("sf cli has no orgs to import.");
    }
    let mut imported = Vec::new();
    let mut skipped = Vec::new();
    for org in wanted {
        let alias = org.alias.clone().unwrap_or_else(|| org.username.clone());
        let auth = ctx.auth()?;
        if auth.connections.resolve(&alias).is_some()
            || auth.connections.resolve(&org.username).is_some()
        {
            out.say(format!("  {alias}: already connected, skipped"));
            skipped.push(json!({ "alias": alias, "reason": "already connected" }));
            continue;
        }
        match import_one(ctx, out, org, &alias) {
            Ok(json) => imported.push(json),
            Err(why) => {
                out.say(format!("  {alias}: {why}"));
                skipped.push(json!({ "alias": alias, "reason": why }));
            }
        }
    }
    if !skipped.is_empty() && imported.is_empty() && !names.is_empty() {
        return Err(Failure::error("nothing was imported"));
    }
    Ok(Done::ok(
        json!({ "imported": imported, "skipped": skipped }),
    ))
}

/// One org: its refresh token from sf, a fresh access token, a saved connection. The error is
/// why it was skipped.
fn import_one(ctx: &Ctx, out: &Output, org: &SfOrg, alias: &str) -> Result<Value, String> {
    let shown = sf_json(&["org", "display", "--target-org", &org.username, "--verbose"])
        .map_err(|f| f.message)?;
    let relogin = |what: &str| {
        format!("{what}; log in with `opk org login --alias {alias}` (or `--jwt` for a CI user)")
    };
    let url = shown["result"]["sfdxAuthUrl"]
        .as_str()
        .ok_or_else(|| relogin("sf has no refresh token for it (a JWT login or an expired one)"))?;
    let creds = parse_auth_url(url).ok_or_else(|| relogin("sf's auth URL is not readable"))?;
    if !creds.client_secret.is_empty() {
        return Err(relogin("it uses a connected app with a client secret"));
    }
    let resp = oauth::refresh(
        &ctx.http,
        &creds.instance_url,
        &creds.client_id,
        &creds.refresh_token,
    )
    .map_err(|e| relogin(&format!("the refresh token does not work ({e})")))?;
    let resp = oauth::TokenResponse {
        refresh_token: resp.refresh_token.or(Some(creds.refresh_token)),
        ..resp
    };
    connect(
        ctx,
        out,
        Some(alias),
        resp,
        creds.instance_url,
        creds.client_id,
        AuthMethod::Web,
    )
    .map_err(|f| f.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sf_auth_urls() {
        assert_eq!(
            parse_auth_url("force://PlatformCLI::5Aep861.x!y@acme.my.salesforce.com"),
            Some(SfAuthUrl {
                client_id: "PlatformCLI".into(),
                client_secret: String::new(),
                refresh_token: "5Aep861.x!y".into(),
                instance_url: "https://acme.my.salesforce.com".into(),
            })
        );
        let with_scheme =
            parse_auth_url("force://Id:secret:rt@https://acme--dev.sandbox.my.salesforce.com/")
                .unwrap();
        assert_eq!(with_scheme.client_secret, "secret");
        assert_eq!(
            with_scheme.instance_url,
            "https://acme--dev.sandbox.my.salesforce.com"
        );
        assert!(parse_auth_url("https://acme.my.salesforce.com").is_none());
        assert!(parse_auth_url("force://PlatformCLI::@acme.my.salesforce.com").is_none());
    }

    #[test]
    fn reads_sf_org_lists() {
        let json: Value = serde_json::from_str(
            r#"{"status":0,"result":{
                "nonScratchOrgs":[{"username":"me@acme.com","alias":"prod"},
                                  {"username":"me@acme.com.dev","alias":"dev"}],
                "devHubs":[{"username":"me@acme.com","alias":"prod"}],
                "sandboxes":[{"username":"me@acme.com.dev","alias":"dev"}],
                "other":[{"username":"x@acme.com"}],
                "scratchOrgs":[]}}"#,
        )
        .unwrap();
        let orgs = parse_org_list(&json);
        assert_eq!(
            orgs.iter().map(|o| o.username.as_str()).collect::<Vec<_>>(),
            ["me@acme.com", "me@acme.com.dev", "x@acme.com"]
        );
        assert_eq!(orgs[0].alias.as_deref(), Some("prod"));
        assert_eq!(orgs[2].alias, None);
    }
}
