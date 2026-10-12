# Moving from sf cli

Your project stays as it is: OrgPunk reads `sfdx-project.json`, `.forceignore` and `replacements` like sf does, and both tools can live in one repository.

## Three steps

```sh
opk init                 # add opk to the project
opk org import           # take over sf cli's logins
opk env add prod prod --master --import
```

An old metadata-format repository (`src/` with `package.xml`) becomes an sf project with `opk init --from-mdapi src`. The `src/` folder stays until you delete it.

## Commands

| sf cli | OrgPunk |
| --- | --- |
| `sf org login web --alias dev` | `opk org login --alias dev` |
| `sf org login web --instance-url https://test.salesforce.com` | `opk org login --sandbox` |
| `sf org login jwt …` | `opk org login --jwt --client-id … --key … --username …` |
| `sf org list` | `opk org list` |
| `sf config set target-org dev` | `opk org use dev` |
| `sf project deploy preview` | `opk status` |
| `sf project retrieve start` | `opk pull` |
| `sf project deploy start` (your sandbox) | `opk push` |
| `sf project deploy start --target-org prod` | `opk env deploy prod` |
| `sf project deploy start --source-dir <path>` | `opk push <path>` or `opk env deploy <env> <path>` |
| `sf project deploy start --metadata ApexClass:Foo` | `--metadata ApexClass:Foo` on either |
| `sf project deploy validate` | `opk env deploy <env> --validate` |
| `sf project deploy quick --job-id <id>` | `opk env deploy <env> --quick <id>` |
| `sf project convert source --output-dir mdapi` | `opk env deploy <env> --dry-run --output-dir mdapi` |
| `sf project convert mdapi --root-dir src` | `opk init --from-mdapi src` |

`--test-level`, `--tests` and `--wait` mean the same.

## What behaves differently

- **No deploy "anywhere".** Your own org is the target of `status` / `pull` / `push`; shared orgs are [environments](env.md), deployed from commits.
- **Deletions** need `--allow-destructive`.
- **Limits and permissions** are checked before anything is sent.
- **Unsupported metadata is an error**, not a silently skipped file.
- **`--dry-run`** runs locally; sf validates in the org for that.
- **Exit codes** differ: see [output.md](output.md).

## Environment variables

OrgPunk reads its own `ORGPUNK_*` variables first, then sf's (`SF_TARGET_ORG`, `SF_ORG_API_VERSION`), so pipelines set up for sf keep working.

## Not there yet

Scratch orgs, `sf project generate`, `sf apex run`, `sf data` and packages (unlocked, 2GP). Keep sf cli for those.
