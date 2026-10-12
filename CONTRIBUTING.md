# Contributing to OrgPunk

Thanks for helping. This page has what you need to get a change in.

## Before you start

- **Bugs:** open an issue with the command, the output (`--json` gives the full error), `opk --version` and your OS.
- **Features and bigger changes:** open an issue first. OrgPunk has a deliberate model — read [How OrgPunk works](docs/guide/philosophy.md) — and a change that fits it is much easier to land.
- **Security issues:** do not open a public issue; write to the maintainer privately.

## Setup

You need [Rust](https://rustup.rs) (the version is pinned in `rust-toolchain.toml`) and, optionally, [Task](https://taskfile.dev).

```sh
git clone https://github.com/OrgPunk/cli.git
cd cli
task build              # or: cargo build --bin opk
task run -- --help      # or: cargo run --bin opk -- --help
```

`task` lists everything. The ones you need most:

| Task | What it runs |
| --- | --- |
| `task check` | everything the CI check job runs: dependency layers, `fmt`, `clippy -D warnings`, tests |
| `task test` | the tests |
| `task fmt` | formats the code |
| `task parity` | conversion parity with sf's SDR on public projects (needs Node; CI runs it on every PR) |

`task layers` needs `jq`.

## How the code is organised

A Cargo workspace in three layers. Dependencies only point down: `cli` → scenarios → core (`scripts/check-layers.sh` checks it).

| Layer | Crates | |
| --- | --- | --- |
| cli | `cli` | the `opk` binary: commands, human and JSON output, exit codes |
| scenarios | `init`, `sync`, `deploy`, `env`, `scratch` | what the commands do |
| core | `convert`, `sfapi`, `auth`, `config`, `repo` | metadata conversion, Salesforce APIs, login, config files, git |

The metadata registry in `crates/convert/sdr/` is data taken from SDR; do not edit it by hand.

## Rules for changes

- **English** in everything in the code: comments, errors, CLI output, help, tests, scripts.
- **Correct over fast.** A package with a silent hole is worse than a failed command: anything OrgPunk cannot handle exactly is an error.
- **Tests** for behaviour: unit tests next to the code; conversion changes must keep `task parity` green.
- **No `unsafe`** (the workspace forbids it); clippy warnings fail the build.
- **Docs:** user-facing changes update `docs/guide/` (English) in the same pull request.
- Keep a pull request to one topic; describe what changes for the user and how you checked it.

## Commits and sign-off

Sign off every commit (`git commit -s`). By that you certify the [Developer Certificate of Origin](https://developercertificate.org): you wrote the change or have the right to submit it under the project's license.

Commit messages: an imperative summary line (`fix: keep labels on pull`), a blank line, then the why if it is not obvious.

## License

OrgPunk is licensed under the [GNU AGPL v3.0 or later](LICENSE). By contributing you agree that your contribution is licensed under the same terms.
