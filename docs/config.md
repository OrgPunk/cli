# Settings

A few defaults, set per project or globally, like sf's config variables.

| Setting | What for | Env variable |
| --- | --- | --- |
| `target-org` | the default org of `status`, `pull`, `push` | `ORGPUNK_TARGET_ORG` (or sf's `SF_TARGET_ORG`) |
| `api-version` | API version for requests; the package version if `sfdx-project.json` has no `sourceApiVersion` | `ORGPUNK_API_VERSION` (or `SF_ORG_API_VERSION`) |

```sh
opk config set target-org dev               # this project
opk config set api-version 66.0 --global    # all projects
opk config get target-org
opk config list
opk config unset target-org
```

`get` shows where a value comes from: `local`, `global` or `env ORGPUNK_TARGET_ORG`.

## Which value wins

1. an `ORGPUNK_*` environment variable;
2. the sf variable (`SF_TARGET_ORG`, …) — pipelines set up for sf keep working;
3. the project's `.orgpunk/config.toml`;
4. the global `~/.config/orgpunk/config.toml`.

```toml
# .orgpunk/config.toml
[config]
target-org = "dev"
api-version = "66.0"
```

The project file also holds the [environments](env.md); OrgPunk only edits the parts it owns and keeps your comments. Values are checked when set: `api-version 66` is refused, `66.0` is right.
