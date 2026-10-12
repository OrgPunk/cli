# OrgPunk

A fast command-line tool for Salesforce development, in one binary — no Node, no sf cli. The command is `opk`.

- **Your sandbox like a git remote:** `opk status`, `pull`, `push`.
- **Shared orgs as environments:** qa → uat → prod, deployed from commits, with protect rules, a deploy history, rollback and drift checks.
- **Your project as it is:** a regular sf project (`sfdx-project.json`, `.forceignore`, `replacements`); the conversion matches sf's, checked in CI on public projects.

```sh
curl -fsSL https://raw.githubusercontent.com/OrgPunk/cli/main/install.sh | sh
```

```sh
opk init
opk org import                              # your sf cli logins
opk env add prod prod --master --import     # production is the source of truth

opk pull                                    # your sandbox → git
opk push                                    # git → your sandbox
opk env deploy prod                         # a commit → production
```

## Documentation

- [User guide](docs/guide/README.md) — start with [How OrgPunk works](docs/guide/philosophy.md) and the [quick start](docs/guide/quickstart.md).
- [Workflow on GitHub](docs/guide/github.md) — pipelines ready to copy.
- [Moving from sf cli](docs/guide/migrate-from-sf.md).

## Status

OrgPunk is young. Syncing, environments and deploys work; scratch orgs are next. The plan and progress (in Russian) are in [docs/orgpunk.md](docs/orgpunk.md).

## Contributing

Bug reports, ideas and pull requests are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

OrgPunk is free software under the [GNU Affero General Public License v3.0 or later](LICENSE): you may use, change and share it, and anything built on it must stay open under the same terms. The Salesforce metadata registry it embeds comes from [SDR](https://github.com/forcedotcom/source-deploy-retrieve) under Apache-2.0 (`crates/convert/sdr/`).
