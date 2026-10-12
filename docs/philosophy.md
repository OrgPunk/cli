# How OrgPunk works

OrgPunk is built around a few simple ideas. Once you have them, the commands are obvious.

## 1. Git is the source of truth

What runs in your shared orgs should be something you can point at in git. OrgPunk deploys commits, records which commit went where, and goes back by deploying an older commit. The org is never the place where the truth lives.

## 2. Two kinds of orgs

| | **Your orgs** | **Environments** |
| --- | --- | --- |
| Examples | your dev sandbox, a scratch org | qa, uat, prod |
| Who changes them | you, freely | the pipeline |
| How | `opk status` / `pull` / `push` | `opk env deploy` |
| What goes in | your changes since the last sync | a whole commit |
| Safety rails | none — it is your playground | protect rules, history, rollback, drift checks |
| Described in | nowhere: just a login | `.orgpunk/config.toml`, committed |

**Your orgs** work like a git remote. You change things in the org (Setup, Flow Builder) and locally (code), then `pull` and `push` to bring both sides together. Uncommitted work is fine here.

**Environments** are the orgs the whole team shares. They are listed in the project config by Org Id, so everybody — and CI — sees the same roles and rules, whatever their aliases are. You never edit them by hand; you deploy commits to them.

One environment is the **master**: the source of truth, usually production.

## 3. Environments only get commits

A deploy to an environment needs a clean working tree. OrgPunk then records the deploy in the environment's history (`.orgpunk/deployments/<OrgId>.json`) and commits that file for you. So the history always says exactly which commit is in each org, and:

- **promotion** takes what was tested: `opk env deploy uat --from qa` deploys the commit that qa runs, not whatever your branch holds now;
- **rollback** is a deploy of an older commit: `opk env rollback uat`.

OrgPunk commits only its own records. It never pushes and never touches your branches: git stays yours.

## 4. Nothing happens silently

- Someone changed an environment around OrgPunk (a hotfix in Setup)? The next deploy stops and names the components. You choose: keep the org's version or overwrite it. See [drift](env.md#drift-changes-made-around-orgpunk).
- A file OrgPunk does not understand? An error, not a package with a hole in it.
- Deletions? Only with `--allow-destructive`.
- Too big for Salesforce? You learn it before anything is sent.
- Protected org? It asks you to type its name, or wants `--confirm-org` in CI.

## The workflow

This is the flow OrgPunk is designed for. [Workflow on GitHub](github.md) has it ready to copy.

```
 your sandbox            git                           environments
 ────────────            ───                           ────────────
 pull ⇄ code ⇄ push  →   feature branch → PR → main  →  qa ──(--from)──▶ uat ──(--from)──▶ prod
                                       │                ▲
                                 validate against qa    deploy on merge
```

1. **Develop in your own sandbox.** `opk pull` brings in what you clicked together in the org, you write code, `opk push` sends it. Repeat. Commit when it works.
2. **Open a pull request.** CI validates the change against qa (`--validate`): it compiles and runs tests there, but changes nothing.
3. **Merge to main.** CI deploys main to qa. The history records the commit.
4. **Promote.** When qa looks good, `opk env deploy uat --from qa` moves exactly that commit on; later the same to prod. Each step can need an approval.
5. **Hotfix or rollback.** Something broke: `opk env rollback prod`. Someone fixed prod by hand: pull the change into git, then `opk env sync prod` so the history knows.

**Run environment deploys from one branch** (main in this guide). The history files live in git; if qa were deployed from one branch and uat from another, each branch would know only part of the history.

Next: [Quick start](quickstart.md).
