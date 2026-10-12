# Connecting orgs

OrgPunk keeps any number of connections, each under an alias. Wherever a command takes an org you can use the alias (`dev`), the username (`me@acme.com.dev`) or the Org Id (`00D5g000000XXXX`).

## Take them over from sf cli

```sh
opk org import              # every org sf cli knows
opk org import dev prod     # only these
```

sf cli must be installed. OrgPunk reads each org's refresh token from it and saves the connection under the same alias — no browser login. Skipped, with a hint what to do: JWT logins (use `opk org login --jwt`), connected apps with a client secret and expired logins (use `opk org login`), and orgs you already have.

## Log in with the browser

```sh
opk org login --alias dev --sandbox --set-default
```

| Option | |
| --- | --- |
| `-a`, `--alias` | the connection's name; default: the username |
| `--sandbox` | log in at `test.salesforce.com` |
| `--instance-url <url>` | log in at My Domain or another URL |
| `--set-default` | make it the default org for `status`, `pull`, `push` |
| `--no-browser` | print the URL instead of opening a browser |
| `--client-id <id>` | your own app instead of sf's public `PlatformCLI` |

Nothing needs installing in the org: OrgPunk uses the same public app as sf cli. After login OrgPunk finds out whether the org is production, a sandbox or a scratch org, and whether it is a Dev Hub.

**Logging in on a remote machine:** the browser redirects to `localhost:1717`. Forward the port and open the printed URL locally:

```sh
ssh -L 1717:localhost:1717 user@server
opk org login --alias dev --no-browser
```

## Log in with JWT (CI)

```sh
opk org login --jwt --client-id 3MVG9... --key server.key \
  --username ci@acme.com --alias prod --instance-url https://acme.my.salesforce.com
```

No browser, no refresh token: when the token expires OrgPunk signs a new one with the same key file, so keep it where it was. In CI you usually don't log in at all — [CI mode](ci.md) reads the same values from environment variables.

### External Client App for JWT

Create it in each org you deploy to from CI:

1. Setup → External Client App Manager → New External Client App.
2. Enable OAuth; any callback URL, e.g. `http://localhost:1717/OauthRedirect`.
3. Scopes: `api` and `refresh_token, offline_access` (Salesforce insists on the latter even for JWT).
4. Enable JWT Bearer Flow and upload the certificate.
5. Policies: "Admin approved users are pre-authorized"; add the CI user's profile or permission set.
6. The Consumer Key is the client id.

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
  -keyout server.key -out server.crt -subj "/CN=opk-ci"
```

The key is a secret: never commit it.

## List, choose, log out

```sh
opk org list
```

```
* dev     me@acme.com.dev   00D5g000000AAAA   sandbox       valid   env qa
  prod    admin@acme.com    00D5g000000BBBB   prod+devhub   valid   env prod (master)
```

`*` marks the default org. The token column says `valid`, `expired`, `revoked` (log in again), `missing` or `unknown` (no network). The last column shows the org's environments.

```sh
opk org use dev              # default org for this project
opk org use dev --global     # for all projects
opk org logout dev           # revoke the token and forget the connection
```

## Where things are kept

| What | Where |
| --- | --- |
| connections (no secrets) | `~/.config/orgpunk/connections.toml` |
| tokens | the system keychain (macOS Keychain, Windows Credential Manager, Secret Service on Linux) |
| tokens without a keychain | an encrypted file in `~/.config/orgpunk` |

Tokens never appear in output, `--json` included. `ORGPUNK_SECRET_STORE=file` or `keychain` forces the store; `ORGPUNK_CONFIG_DIR` moves the folder.

## Permissions

Before sending, OrgPunk checks that the user has **Modify All Data** or **Modify Metadata Through Metadata API Functions**.
