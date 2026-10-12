# OrgPunk user guide

OrgPunk (`opk`) is a command-line tool for Salesforce development. One binary, no Node, no sf cli. It works with a regular sf project (`sfdx-project.json`, `.forceignore`, `replacements`).

## Start here

1. [Install](install.md) — one command.
2. [How OrgPunk works](philosophy.md) — the ideas behind it, in five minutes. Read this first: everything else follows from it.
3. [Quick start](quickstart.md) — from an existing project to a first deploy.
4. [Workflow on GitHub](github.md) — branches, pull requests and pipelines, ready to copy.

## Guides

| Topic | What it covers |
| --- | --- |
| [Your sandbox](sync.md) | `status`, `pull`, `push`: day-to-day work, like git with a remote |
| [Environments](env.md) | qa, uat, prod: deploy, promote, history, rollback, drift |
| [Deploy options](deploy.md) | what to deploy, tests, validation, deletions, big packages |
| [Connecting orgs](orgs.md) | login, JWT for CI, importing from sf cli, where tokens live |
| [Other CI systems](ci.md) | CI mode, GitLab |
| [Settings](config.md) | default org, API version |
| [Moving from sf cli](migrate-from-sf.md) | command map, what behaves differently |
| [JSON output and exit codes](output.md) | for scripts |
| [Troubleshooting](troubleshooting.md) | common errors and fixes |

## Commands at a glance

```
opk init                        set up a project
opk org login | import | list | use | logout
opk status | pull | push        your own sandbox
opk env list | add | set | remove
opk env deploy | history | rollback | sync
opk config get | set | unset | list
```

Every command has `--help`. `--json` gives machine-readable output.

## Not there yet

Scratch orgs (create, list, delete) are planned. Until then, use sf cli for them.
