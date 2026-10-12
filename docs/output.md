# JSON output and exit codes

## Exit codes

| Code | Meaning | For example |
| --- | --- | --- |
| `0` | done | deployed |
| `1` | OrgPunk or its surroundings failed | no connection, no network, a broken `sfdx-project.json`, a package over the limits |
| `2` | refused by a rule | protect, a deletion without `--allow-destructive`, a conflict, drift, a dirty tree |
| `3` | the deploy failed | component or test errors, also with `--validate` |
| `4` | still running | `--wait` ran out; the deploy goes on in the org |

In scripts: `3` means fix the code, `1` means retry or fix the setup.

## `--json`

Every command takes `--json` (CI mode turns it on). stdout then holds exactly one JSON object; progress and hints go to stderr.

```sh
opk env deploy qa --json 2>/dev/null | jq '.result.numberComponentErrors'
```

```json
{ "version": 1, "status": 0, "result": { } }
{ "version": 1, "status": 1, "error": { "message": "connection `qa` not found; see `opk org list`" } }
```

`status` is the exit code; `version` changes only with incompatible changes. A failed deploy (`3`) or a timeout (`4`) still has a `result`: the report is what you need.

## Results

### Deploys: `env deploy`, `push`, `env rollback`

The Salesforce deploy result (`id`, `status`, `numberComponentErrors`, `numberTestErrors`, `details.componentFailures`, `details.runTestResult`, …) and `targetOrg: { alias, orgId }`. Then, depending on the command:

| Field | When |
| --- | --- |
| `validationId` | after a successful `--validate` |
| `history: { id, created, changed, commit }` | a deploy to an environment |
| `from: { env, commit, deploy }` | `env deploy --from` |
| `rollback: { to, commit, restored, deleted, skipped }` | `env rollback` |
| `pushed`, `deleted` | `push` |
| `parts: [{ part, deployId, result }]`, `partsTotal` | `--split` |
| `dryRun: true`, `files`, `zipBytes` (or `parts`) | `--dry-run` |

### `status`

```json
{ "org": "dev", "tracking": true,
  "local": [{ "member": "ApexClass:InvoiceService", "change": "modified" }],
  "remote": [{ "member": "Flow:Invoice_Approval", "change": "modified" }],
  "conflicts": [] }
```

### `env list`

`{ "environments": [{ "name", "org", "alias", "master", "protect", "branches", "next", "effectiveProtect" }] }` — `alias` is your connection to that org, or `null`.

### `env history`

`{ "env", "deploys": [{ "id", "at", "commit", "by", "changed", "created", "import", "sync", "rollbackTo", "partial" }] }`, newest first.

### `env sync`

`{ "env", "modified", "deleted", "baseline", "history": { "id", "commit" } }` — no `history` with `--dry-run` or when nothing differs.

### `org list`, `org login`, `org import`

`org list` is an array of `{ alias, username, orgId, instanceUrl, orgType, devHub, auth, target, token, master, envs }`. `org login` returns one connection; `org import` returns `{ imported, skipped }`.

### `config`

`get` and `list`: `{ key, value, source }`; `set`: `{ key, value, global }`; `unset`: `{ key, removed, global }`.

Tokens never appear in JSON.
