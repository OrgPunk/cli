# Your sandbox: `status`, `pull`, `push`

Your own org (a dev sandbox, a scratch org) works like a git remote. You change things on both sides — clicks in the org, code locally — and these three commands bring them together.

```sh
opk status       # what changed, on which side
opk pull         # take the org's changes
opk push         # send yours
```

They use the default org (`opk org use dev`); any other with `-o dev2`.

## `opk status`

Downloads nothing and changes nothing. It shows what changed since the last `pull` or `push`:

```
In the org (opk pull) (1):
  modified  Flow:Invoice_Approval
Local (opk push) (2):
  modified  ApexClass:InvoiceService
  added     LightningComponentBundle:invoiceList
Conflicts (changed on both sides; --force picks a side) (1):
  modified  Layout:Account-Account Layout
```

## `opk pull`

Brings the org's changes into your working copy. Files that exist are updated in place; new components land in the default package directory.

## `opk push`

Sends your changes to the org. Only some of them:

```sh
opk push force-app/main/default/classes/InvoiceService.cls
opk push --metadata 'ApexClass:Invoice*'
```

Components you deleted locally are deleted in the org too, with `--allow-destructive`.

No commit needed: edit, push, try it in the org, edit again.

## Conflicts

A component changed in the org and locally is a conflict. `pull` and `push` stop and change nothing. Pick a side:

- `opk pull --force` — the org's version wins;
- `opk push --force` — your version wins.

## The first sync with an org

With no earlier sync there is nothing to compare against. OrgPunk fetches the components that exist on both sides and compares them byte for byte. Identical ones count as synced; only the ones that really differ become conflicts.

```
Comparing 412 component(s) present locally and in dev with no earlier sync…
405 identical on both sides, recorded as synced.
```

## How changes are found

OrgPunk keeps a snapshot of the last sync per org in `.orgpunk/state/` (not committed): each component's revision in the org and the hashes of its local files. Sandboxes with source tracking report their changes directly; other orgs are listed with `listMetadata`, which is slower.

## Bigger pushes

`push` uses the same pipeline as a [deploy](deploy.md):

```sh
opk push --dry-run                                # what would go, nothing sent
opk push --validate --test-level RunLocalTests    # check in the org, change nothing
opk push --quick 0Af5g00000XXXXX                  # deploy that validation
opk push --split                                  # too big for one deploy: in parts
```

Between `--validate` and `--quick`, keep your changes as they were: `--quick` marks the current changes as synced.

## Pushing to an environment

If the org is one of your [environments](env.md), `push` follows its rules: protect, a clean working tree, history. For environments use `opk env deploy`.
