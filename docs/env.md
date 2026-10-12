# Environments

Environments are the orgs your team shares: qa, uat, prod. They are listed in `.orgpunk/config.toml` by Org Id and committed, so everyone — and CI — has the same roles and rules.

One of them is the **master**: the source of truth, usually production. Until there is a master, `push` and `env deploy` stay off.

## Set them up

```sh
opk env add prod prod --master --import
opk env add uat uat --protect confirm,validate-first,branch --next prod --import
opk env add qa qa --protect confirm --next uat --import
opk env list
git add .orgpunk && git commit -m "chore: environments"
```

```
prod   00D…prd   prod   branch, confirm, validate-first        branches main  master
qa     00D…qa    qa     confirm                                → uat
uat    00D…uat   uat    branch, confirm, validate-first        branches main → prod
```

- The second argument is a connection alias (see `opk org list`), a username or an Org Id. With an Org Id you can add an environment you have no login for.
- Add from the end of the chain: `--next` must name an existing environment. It documents the order; OrgPunk reminds you when a promotion goes elsewhere.
- `--import` records what the org has today as the start of its history (see [below](#a-starting-point---import)).

Change or remove later:

```sh
opk env set qa --protect confirm,branch --branches develop
opk env set qa --protect none
opk env remove qa
```

## Protect rules

| Rule | Effect | In CI |
| --- | --- | --- |
| `confirm` | type the org's alias or Org Id before a deploy | `--confirm-org <OrgId>` |
| `validate-first` | validate first, then quick-deploy that validation | same |
| `branch` | only from allowed branches (`--branches`, default `main`), clean tree, `HEAD` pushed | same |
| `no-destructive` | no deletions | — |
| `locked` | no deploys at all | — |

The master gets `confirm, validate-first, branch` unless you set `--protect`. If the same org appears in several environments, their rules add up.

Protect guards against mistakes. Real protection is the deploy user's permissions in the org.

## Deploy

```sh
opk env deploy qa
opk env deploy qa force-app/main/default/classes      # only part of the project
opk env deploy uat --from qa                          # what qa runs now
```

- The working tree must be **clean**: the history records the commit that went to the org.
- After the deploy OrgPunk **commits the history** (`chore: opk deploy qa #43`). Push it with git. `--no-git-commit` skips the commit.
- `--from qa` takes the commit of qa's last complete deploy, whatever your branch holds — what was tested goes on. Before sending it shows what qa runs and what the target runs now.

All deploy options: [Deploy options](deploy.md).

## History and rollback

```sh
opk env history uat
```

```
#44   2026-10-13 07:00:12  -            ci@acme.com    sync: 412 recorded, 0 gone
#43   2026-10-12 14:02:11  a1b2c3d4e5f6 ci@acme.com    12 changed, 2 created
#42   2026-10-11 18:30:02  9f8e7d6c5b4a ci@acme.com    3 changed, 0 created
#41   2026-10-10 11:15:40  5a4b3c2d1e0f ci@acme.com    import
```

```sh
opk env rollback uat          # to the deploy before the last
opk env rollback uat 41       # to #41
```

A rollback is a deploy: components changed since go back to their version in that deploy's commit, components created since are deleted (needs `--allow-destructive`). It passes the same protect rules and appears in the history.

## Drift: changes made around OrgPunk

Before each deploy OrgPunk compares the org with the history. If a component of the package was changed some other way — in Setup, with sf cli, by a change set — the deploy stops:

```
Error: changed in the org since opk last deployed them: Flow:Invoice_Approval.
To keep the org's versions: `opk pull -o <alias>`, commit, then `opk env sync uat`;
to overwrite them: --force
```

**Keep the change:**

```sh
opk pull -o uat
git commit -am "fix: invoice flow change from uat"
opk env sync uat
```

**Overwrite it:**

```sh
opk env deploy uat --force
```

### `opk env sync`

Records the org as it is now in the history. It changes no files.

```sh
opk env sync uat --dry-run     # only show the differences
opk env sync uat
```

```
Changed in the org around opk (1):
  Flow:Invoice_Approval    2026-10-13T09:12:40 by Admin User
Gone from the org (1):
  Layout:Account-Old
Not in the history yet, get a baseline (14): ApexClass:LegacyX, …
History #44: env.uat recorded as it is now.
```

- Changed components stop being drift. Their versions from the org are **not** in git because of this: pull them first if you want to keep them, or the next deploy overwrites them.
- Components the history did not know get a baseline, so their manual changes are caught from now on.
- Components deleted from the org are forgotten.
- A sync is not a deploy: rollback and `--from` skip it.
- Needs a clean tree and commits the history (`chore: opk env sync uat #44`).

## A starting point: `--import`

A rollback can only restore versions that are in git. For components that existed before OrgPunk, record the org's current versions once:

```sh
opk env add uat uat --import
```

OrgPunk fetches the project's components from the org, commits them (`chore: opk import uat`) and records that commit as the start of the history. Unlike `env sync`, this brings files into git. It needs a clean tree; `env add` on a terminal offers it by itself, and running it again for the same org is safe.

## The config file

```toml
[env.qa]
org = "00D5g000000QQQQ"
protect = ["confirm"]
next = "uat"

[env.prod]
org = "00D5g000000PPPP"
master = true
protect = ["confirm", "validate-first", "branch", "no-destructive"]
```

You can edit it by hand; `env add` / `set` / `remove` look up Org Ids for you and check the result.
