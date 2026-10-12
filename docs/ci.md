# Other CI systems

For GitHub Actions see [Workflow on GitHub](github.md). This page has what any CI needs, and GitLab.

## CI mode

OrgPunk switches to CI mode by itself when `CI=true` (GitHub, GitLab and most others set it), or with `--ci`:

- no prompts: whatever needs a confirmation needs a flag (`--confirm-org <OrgId>`), else exit code 2;
- JSON on stdout;
- login by JWT from environment variables only, nothing stored:

| Variable | Value |
| --- | --- |
| `ORGPUNK_CLIENT_ID` | Consumer Key of the org's [External Client App](orgs.md#external-client-app-for-jwt) |
| `ORGPUNK_JWT_KEY` | the private key (PEM text or a path to it) |
| `ORGPUNK_USERNAME` | the deploy user |
| `ORGPUNK_INSTANCE_URL` | the login URL: My Domain, or `https://test.salesforce.com` for a sandbox |

The org the login lands in must be the environment's org, or OrgPunk refuses: a wrong secret never deploys to the wrong org.

## What a pipeline does

```sh
curl -fsSL https://raw.githubusercontent.com/OrgPunk/cli/main/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"

git config user.name "ci"
git config user.email "ci@acme.com"
opk env deploy qa --test-level RunLocalTests --confirm-org "$QA_ORG_ID"
git push origin HEAD:main          # the history commit opk made
```

The pipeline needs permission to push to the branch it deploys from.

## GitLab

```yaml
deploy-qa:
  image: alpine:3.20
  rules:
    - if: $CI_COMMIT_BRANCH == "main"
  resource_group: qa
  variables:
    ORGPUNK_INSTANCE_URL: https://acme--qa.sandbox.my.salesforce.com
  script:
    - apk add --no-cache curl git
    - curl -fsSL https://raw.githubusercontent.com/OrgPunk/cli/main/install.sh | sh
    - export PATH="$HOME/.local/bin:$PATH"
    - git checkout -B "$CI_COMMIT_BRANCH" "$CI_COMMIT_SHA"
    - git branch --set-upstream-to "origin/$CI_COMMIT_BRANCH"
    - git config user.name "ci" && git config user.email "ci@acme.com"
    - opk env deploy qa --test-level RunLocalTests --confirm-org "$QA_ORG_ID"
    - git push "https://oauth2:${CI_PUSH_TOKEN}@${CI_SERVER_HOST}/${CI_PROJECT_PATH}.git" HEAD:main
```

`ORGPUNK_CLIENT_ID`, `ORGPUNK_JWT_KEY`, `ORGPUNK_USERNAME` and `QA_ORG_ID` are masked project variables; `CI_PUSH_TOKEN` is a project access token with write access. GitLab checks out a detached commit, so the two `git` lines put it back on its branch: the `branch` protect rule needs a branch. `resource_group` keeps one deploy per environment at a time.

## Tips

- `--dry-run` needs no secrets: run it on every merge request.
- Exit codes: 3 means the deploy or its tests failed, 1 means the environment did (network, login), 4 means the deploy is still running after `--wait`.
- Pin the version: `ORGPUNK_VERSION=v0.2.0` before `sh`.
