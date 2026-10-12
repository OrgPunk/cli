# Workflow on GitHub

A complete setup: feature branches, pull requests validated against qa, deploys to qa on merge, approved promotions to uat and prod, hotfixes and drift checks. Copy it and change the names.

```
feature branch ──PR──▶ main ──merge──▶ qa ──promote──▶ uat ──promote──▶ prod
                  │                     ▲
          validate against qa      deploy (history commit pushed to main)
```

## The rules

- **One long-lived branch: `main`.** All environment deploys run from it, so the deploy history of every environment lives in one place.
- **Work happens in your own sandbox**, on a feature branch (see [Your sandbox](sync.md)).
- **Environments only get commits from main** and only through the pipeline.

## 1. Environments

Once, by the team lead. Add them from the end of the chain: `--next` must name an environment that exists.

```sh
opk env add prod prod --master --protect confirm,validate-first,branch,no-destructive --import
opk env add uat uat --protect confirm,validate-first,branch --next prod --import
opk env add qa qa --protect confirm --next uat --import
git add .orgpunk && git commit -m "chore: environments" && git push
```

`branch` allows deploys only from `main` (the default), with a clean tree and `HEAD` pushed. qa has no `branch` rule so that pull requests can be validated against it.

## 2. GitHub settings

**Environments** (Settings → Environments): create `qa`, `uat`, `prod`. For `uat` and `prod`, add required reviewers: a promotion then waits for an approval.

In each environment add:

| Kind | Name | Value |
| --- | --- | --- |
| secret | `ORGPUNK_CLIENT_ID` | Consumer Key of the org's External Client App ([setup](orgs.md#external-client-app-for-jwt)) |
| secret | `ORGPUNK_JWT_KEY` | the private key (PEM) |
| secret | `ORGPUNK_USERNAME` | the deploy user |
| variable | `ORGPUNK_INSTANCE_URL` | the org's login URL: `https://acme.my.salesforce.com`, `https://acme--qa.sandbox.my.salesforce.com` |
| variable | `ORG_ID` | the org's Org Id, for `--confirm-org` |

**Branch protection on main**: the deploy job pushes a history commit to `main`. Let GitHub Actions bypass the rules, or push with a GitHub App token that may.

## 3. Pull requests: `.github/workflows/pr.yml`

```yaml
name: pr

on:
  pull_request:
    branches: [main]

jobs:
  validate:
    runs-on: ubuntu-latest
    environment: qa
    env:
      ORGPUNK_CLIENT_ID: ${{ secrets.ORGPUNK_CLIENT_ID }}
      ORGPUNK_JWT_KEY: ${{ secrets.ORGPUNK_JWT_KEY }}
      ORGPUNK_USERNAME: ${{ secrets.ORGPUNK_USERNAME }}
      ORGPUNK_INSTANCE_URL: ${{ vars.ORGPUNK_INSTANCE_URL }}
    steps:
      - uses: actions/checkout@v4
      - name: Install opk
        run: |
          curl -fsSL https://raw.githubusercontent.com/OrgPunk/cli/main/install.sh | sh
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - name: Build the package
        run: opk env deploy qa --dry-run
      - name: Validate against qa
        run: opk env deploy qa --validate --test-level RunLocalTests --confirm-org "${{ vars.ORG_ID }}"
```

`--dry-run` needs no org: it catches a broken project in seconds. `--validate` compiles and runs tests in qa without changing anything there.

## 4. Merge: `.github/workflows/deploy.yml`

```yaml
name: deploy

on:
  push:
    branches: [main]

concurrency:
  group: opk-qa
  cancel-in-progress: false

permissions:
  contents: write

jobs:
  qa:
    runs-on: ubuntu-latest
    environment: qa
    env:
      ORGPUNK_CLIENT_ID: ${{ secrets.ORGPUNK_CLIENT_ID }}
      ORGPUNK_JWT_KEY: ${{ secrets.ORGPUNK_JWT_KEY }}
      ORGPUNK_USERNAME: ${{ secrets.ORGPUNK_USERNAME }}
      ORGPUNK_INSTANCE_URL: ${{ vars.ORGPUNK_INSTANCE_URL }}
    steps:
      - uses: actions/checkout@v4
      - name: Install opk
        run: |
          curl -fsSL https://raw.githubusercontent.com/OrgPunk/cli/main/install.sh | sh
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - name: Deploy to qa
        run: |
          git config user.name "github-actions[bot]"
          git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
          opk env deploy qa --test-level RunLocalTests --confirm-org "${{ vars.ORG_ID }}"
      - name: Push the history
        run: git pull --rebase origin main && git push origin HEAD:main
```

After the deploy OrgPunk commits `chore: opk deploy qa #N`. The last step pushes it; `--rebase` handles a merge that landed in the meantime. A push made with the workflow's own token does not start the workflow again.

## 5. Promotion: `.github/workflows/promote.yml`

```yaml
name: promote

on:
  workflow_dispatch:
    inputs:
      to:
        description: Environment to promote to
        type: choice
        options: [uat, prod]

concurrency:
  group: opk-${{ inputs.to }}
  cancel-in-progress: false

permissions:
  contents: write

jobs:
  promote:
    runs-on: ubuntu-latest
    environment: ${{ inputs.to }}
    env:
      ORGPUNK_CLIENT_ID: ${{ secrets.ORGPUNK_CLIENT_ID }}
      ORGPUNK_JWT_KEY: ${{ secrets.ORGPUNK_JWT_KEY }}
      ORGPUNK_USERNAME: ${{ secrets.ORGPUNK_USERNAME }}
      ORGPUNK_INSTANCE_URL: ${{ vars.ORGPUNK_INSTANCE_URL }}
      FROM: ${{ inputs.to == 'uat' && 'qa' || 'uat' }}
    steps:
      - uses: actions/checkout@v4
        with:
          ref: main
      - name: Install opk
        run: |
          curl -fsSL https://raw.githubusercontent.com/OrgPunk/cli/main/install.sh | sh
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - name: Promote
        run: |
          git config user.name "github-actions[bot]"
          git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
          opk env deploy "${{ inputs.to }}" --from "$FROM" --test-level RunLocalTests --confirm-org "${{ vars.ORG_ID }}"
      - name: Push the history
        run: git pull --rebase origin main && git push origin HEAD:main
```

Run it from Actions → promote → Run workflow. The environment's reviewers approve, then `--from` deploys **the commit that runs in the previous environment** — what was tested there — not the latest main. `validate-first` validates before deploying.

## 6. Hotfixes and rollback

**Something broke after a deploy:**

```sh
opk env history prod      # find the deploy to go back to
opk env rollback prod     # the one before the last
git push
```

Changed components go back to their versions in that deploy's commit; components created since are deleted (add `--allow-destructive`).

**Someone fixed prod by hand in Setup.** The next deploy to prod stops: the component changed around opk. Bring the fix into git, then tell the history:

```sh
git switch -c hotfix/invoice-flow
opk pull -o prod                       # the org's version into your working copy
git commit -am "fix: invoice flow hotfix from prod"
# PR, review, merge to main
git switch main && git pull
opk env sync prod                      # prod as it is now becomes the known state
git push
```

To throw the manual change away instead: `opk env deploy prod --force`.

## 7. Nightly drift check (optional)

Catch manual changes the day they happen, not on the next release:

```yaml
name: drift

on:
  schedule:
    - cron: "0 5 * * *"

jobs:
  prod:
    runs-on: ubuntu-latest
    environment: prod
    env:
      ORGPUNK_CLIENT_ID: ${{ secrets.ORGPUNK_CLIENT_ID }}
      ORGPUNK_JWT_KEY: ${{ secrets.ORGPUNK_JWT_KEY }}
      ORGPUNK_USERNAME: ${{ secrets.ORGPUNK_USERNAME }}
      ORGPUNK_INSTANCE_URL: ${{ vars.ORGPUNK_INSTANCE_URL }}
    steps:
      - uses: actions/checkout@v4
      - name: Install opk
        run: |
          curl -fsSL https://raw.githubusercontent.com/OrgPunk/cli/main/install.sh | sh
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - name: Compare prod with its history
        run: |
          opk env sync prod --dry-run --json > drift.json
          jq -e '.result.modified + .result.deleted | length == 0' drift.json \
            || { jq '.result' drift.json; exit 1; }
```

A red run lists what changed around opk.

## Tips

- Pin the OrgPunk version in CI: `... | ORGPUNK_VERSION=v0.2.0 sh`.
- One deploy per environment at a time: the `concurrency` groups above make sure of it.
- `--dry-run --output-dir pkg` plus `actions/upload-artifact` keeps the exact package of a run.
