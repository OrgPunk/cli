# Quick start

You have an sf project, your own dev sandbox and a production org. In ten minutes you will sync with the sandbox and make a first protected deploy.

## 1. Set up the project

In the project root:

```sh
opk init
```

This adds `.orgpunk/config.toml` and a few `.gitignore` lines. Your files stay as they are.

Starting from scratch? `opk init my-app` creates the whole project with a git repository. Coming from an old metadata-format repo (`src/` with `package.xml`)? `opk init --from-mdapi src` converts it.

## 2. Connect your orgs

Already logged in with sf cli? Take those connections over:

```sh
opk org import
```

Otherwise, log in through the browser:

```sh
opk org login --alias dev --sandbox --set-default
opk org login --alias prod
```

`--set-default` makes `dev` the org that `status`, `pull` and `push` use.

```sh
opk org list
```

```
* dev     me@acme.com.dev   00D5g000000AAAA   sandbox      valid
  prod    me@acme.com       00D5g000000BBBB   production   valid
```

## 3. Declare production

```sh
opk env add prod prod --master --import
git add .orgpunk && git commit -m "chore: opk environments"
```

- `--master` makes prod the protected source of truth: a deploy needs a typed confirmation, is validated first, and only runs from `main`.
- `--import` records what prod has today, so a rollback can always get back to it.

Until a master is declared, `push` and `env deploy` stay off.

## 4. Work in your sandbox

```sh
opk status       # what changed in the org and here
opk pull         # bring in changes made in the org
# ... write code ...
opk push         # send your changes
```

```
In the org (opk pull) (1):
  modified  Flow:Invoice_Approval
Local (opk push) (2):
  modified  ApexClass:InvoiceService
  added     LightningComponentBundle:invoiceList
```

Commit when it works. More in [Your sandbox](sync.md).

## 5. Deploy to production

```sh
git switch main && git pull
opk env deploy prod --dry-run     # see what would go; nothing is sent
opk env deploy prod
```

OrgPunk asks you to confirm, validates, deploys, records the deploy and commits that record. Push it: `git push`.

Something went wrong?

```sh
opk env history prod
opk env rollback prod
```

## Next

- [Workflow on GitHub](github.md): the same with pull requests and pipelines.
- [Environments](env.md): qa → uat → prod.
