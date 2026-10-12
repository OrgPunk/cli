# Deploy options

`opk env deploy <env>` builds the package on your machine and deploys it to the environment's org with the Metadata API. `opk push` and `opk env rollback` use the same pipeline, so most options below work there too.

```sh
opk env deploy qa
```

## What to deploy

| | |
| --- | --- |
| `opk env deploy qa` | the whole project: every package directory |
| `opk env deploy qa force-app/main/default/classes` | components with files under these paths |
| `opk env deploy qa --metadata ApexClass:InvoiceService` | these components; repeat the flag for more |
| `opk env deploy qa --metadata-dir ./mdapi` | a ready metadata-format folder with `package.xml` |
| `opk env deploy uat --from qa` | the commit qa runs now ([promotion](env.md#deploy)) |

`--metadata` takes `Type`, `Type:Name` or `Type:Prefix*`. Quote the star: `--metadata 'ApexClass:Invoice*'`. A child such as `CustomField:Account.Region__c` goes with its parent object and no other fields.

When building from source, OrgPunk does what sf does:

- **`.forceignore`** is applied, with sf's built-in rules;
- **`replacements`** from `sfdx-project.json` are applied on every deploy;
- the package version is `sourceApiVersion`, or the `api-version` setting.

Warnings (a skipped file, a component in two package directories, a replacement that matched nothing) go to stderr. Anything OrgPunk cannot handle exactly — an unknown metadata type, `sourceBehaviorOptions` — is an error: OrgPunk never sends a package with silent holes.

## Look before you send: `--dry-run`

```sh
opk env deploy qa --dry-run
opk env deploy qa --dry-run --output-dir ./pkg
```

Builds the package, checks limits and deletions, prints the size. Nothing is sent; no login needed. `--output-dir` saves the package exactly as it would go: `package.xml`, `destructiveChanges*.xml` and the files.

## Tests

```sh
opk env deploy qa --test-level RunLocalTests
opk env deploy qa --tests InvoiceServiceTest,InvoiceBatchTest
```

| `--test-level` | Runs |
| --- | --- |
| `NoTestRun` | nothing (not allowed in production for Apex) |
| `RunSpecifiedTests` | the classes in `--tests` |
| `RunLocalTests` | all tests except managed packages' |
| `RunAllTestsInOrg` | everything |

`--tests` alone means `RunSpecifiedTests`. Without either, Salesforce decides: no tests in a sandbox, local tests in production.

## Validate now, deploy later

```sh
opk env deploy prod --validate --test-level RunLocalTests
# Validation id: 0Af5g00000XXXXX (deploy it: the same command with `--quick 0Af5g00000XXXXX`)
opk env deploy prod --quick 0Af5g00000XXXXX
```

`--validate` compiles and runs tests in the org without changing anything. `--quick` deploys that validation without running tests again. Salesforce accepts it for 10 days, as long as no Apex was deployed to the org since.

An environment with `validate-first` does both steps on every deploy by itself.

## Deleting components

A package with `destructiveChanges*.xml` is refused unless you pass `--allow-destructive`. Exit code 2 means nothing was sent.

## Big packages: `--split`

Salesforce limits one deploy to 10,000 files, a 39 MB zip and 600 MB unzipped. OrgPunk checks this before sending. A bigger package can go in parts:

```sh
opk env deploy qa --split --dry-run    # see the plan
opk env deploy qa --split
```

```
source (8120 components): 14210 files → 2 parts
  Part 1: 9800 files, 410.2 MB, zip ~31.0 MB — CustomObject 640, ApexClass 2100, …
  Part 2: 4410 files, 120.5 MB, zip ~12.4 MB — Profile 80, PermissionSet 300, …
```

Parts follow the dependencies between components; profiles and permission sets go last. **Each part is its own deploy**: if part 3 fails, parts 1 and 2 stay in the org and OrgPunk says so. `--split` cannot be combined with `--validate` or `--quick`.

## Waiting: `--wait`

OrgPunk waits up to 33 minutes. If time runs out, the deploy keeps going in the org; OrgPunk exits with code 4 and prints its id. Raise it with `--wait 120`.

## Environments: from a commit only

- The working tree must be clean (except with `--validate` and `--dry-run`).
- After a deploy OrgPunk commits the environment's history. `--no-git-commit` leaves it to you; the next deploy refuses until it is committed.
- Changes made in the org around OrgPunk stop the deploy ([drift](env.md#drift-changes-made-around-orgpunk)); `--force` overwrites them.
- Protected environments ask to confirm; in CI pass `--confirm-org <OrgId>`.

## What you see

Progress goes to stderr while the deploy runs. At the end:

```
Failed: components 410/412, errors 2 · tests 120/120, failed 1

Component failures:
  classes/InvoiceService.cls:14:9  ApexClass InvoiceService — Variable does not exist: total

Test failures:
  InvoiceServiceTest.calculatesTotal — System.AssertException: Assertion Failed
      Class.InvoiceServiceTest.calculatesTotal: line 22, column 1
```

For scripts: [JSON output](output.md).

## The order of checks

Everything that can fail locally fails before the org is touched:

1. environment, protect rules, clean tree;
2. building the package;
3. limits and deletions — `--dry-run` stops here;
4. login and the user's deploy permission;
5. deploy, wait, report, history.
