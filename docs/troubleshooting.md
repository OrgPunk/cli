# Troubleshooting

Run the command again with `--json`: `error.message` holds the full text. If nothing here helps, open an issue with that message, `opk --version` and your OS.

## Connections

**`no org given and target-org is not set`** — pass `-o <alias>` or set a default: `opk org use <alias>`.

**`connection `qa` not found`** — check `opk org list`; log in with `opk org login --alias qa`. Two terminals with different `ORGPUNK_CONFIG_DIR` see different connections.

**The browser does not open** — on Linux without a desktop OrgPunk only prints the URL; open it yourself. On a remote machine forward port 1717 (see [orgs.md](orgs.md#log-in-with-the-browser)).

**Login hangs after signing in** — the redirect goes to `localhost:1717`. Make sure nothing else uses that port (a running `sf org login web`) and the browser runs on the same machine, or the port is forwarded.

**JWT: `refresh_token scope is required`** — add the `refresh_token, offline_access` scope to the External Client App.

**JWT: `user hasn't approved this consumer` / `invalid_grant`** — pre-authorize the user's profile or permission set in the app's policies; check the certificate matches the key and the login URL fits the org (sandbox or My Domain). App changes can take a few minutes.

**Token `revoked` or `expired` in `opk org list`** — log in again under the same alias. For JWT, the key file may have moved.

**`Keychain is not available`** — tokens are kept in an encrypted file instead. That is fine; `ORGPUNK_SECRET_STORE=file` silences the note.

## Your sandbox

**`N conflict(s) with dev`** — a component changed in the org and locally. `opk pull --force` takes the org's version, `opk push --force` yours.

**The first `status` is slow** — with no earlier sync OrgPunk fetches the components present on both sides to compare them. Later runs only look at what changed.

## Environments

**`no master environment`** — add one: `opk env add prod <alias> --master`.

**`the working tree has uncommitted changes`** — deploys and syncs of environments go from a commit. Commit or stash first.

**`changed in the org since opk last deployed them`** — drift: someone changed the org around opk. Keep it: `opk pull -o <alias>`, commit, `opk env sync <env>`. Overwrite it: `--force`. See [drift](env.md#drift-changes-made-around-orgpunk).

**`protect `branch`: HEAD is detached`** — check out an allowed branch. In CI, make sure the job runs on the branch, not a detached commit.

**`protect `confirm`: pass --confirm-org`** — in CI there is nobody to type: pass the org's Org Id.

**`[project] is no longer used`** — an old config. Remove `[project]` and add the master: `opk env add prod <alias> --master`.

**A rollback says `nothing to go back to`** — the history needs an earlier deploy with a commit. `opk env add <env> <alias> --import` sets a starting point.

## Building the package

**`sfdx-project.json uses features opk does not support yet`** — e.g. `sourceBehaviorOptions`. Deploy that project with sf cli for now.

**`no such component in the project` / `unknown metadata type`** — check the type's spelling (`ApexClass`, case matters), that `.forceignore` does not exclude the file, and that the path is in a package directory.

**`has no file of its own, select the whole …`** — some children cannot be picked alone; select the parent.

**`--metadata ApexClass:Foo*` matches nothing** — the shell expanded `*`. Quote it: `'ApexClass:Foo*'`.

**`invalid value `66` for `api-version`** — write `66.0`.

## Deploying

**`needs Modify All Data or Modify Metadata Through Metadata API Functions`** — the user lacks deploy permission.

**`deleting components requires --allow-destructive`** — the package deletes things. Add the flag if that is intended.

**`package exceeds a Salesforce limit`** — deploy in parts by path or `--metadata`, or use `--split` (see [deploy.md](deploy.md#big-packages---split)).

**`N components reference each other … exceed the limits alone`** — with `--split`, a cycle of components is too big for one deploy. Exclude what you don't need with `.forceignore`.

**Exit code 4** — the deploy is still running in the org; watch it in Setup → Deployment Status and raise `--wait` next time.

**Quick deploy refused** — validations expire after 10 days or when Apex is deployed to the org. Validate again.

## Network

OrgPunk honours `HTTPS_PROXY` and `NO_PROXY`.
