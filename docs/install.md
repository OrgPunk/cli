# Install

## macOS and Linux

```sh
curl -fsSL https://raw.githubusercontent.com/OrgPunk/cli/main/install.sh | sh
```

The script picks the build for your OS and CPU, checks it against the release checksums and puts `opk` into `~/.local/bin`. If that folder is not on your `PATH`, the script tells you the line to add.

Check it:

```sh
opk --version
```

Options, set before `sh`:

| Variable | Default | What it does |
| --- | --- | --- |
| `ORGPUNK_VERSION` | latest release | a specific release, e.g. `v0.2.0` |
| `ORGPUNK_INSTALL_DIR` | `~/.local/bin` | where to put the binary |

```sh
curl -fsSL https://raw.githubusercontent.com/OrgPunk/cli/main/install.sh | ORGPUNK_VERSION=v0.2.0 sh
```

## Shell completion

Tab completes commands and flags, and also your environment names, connection aliases, config variables and metadata types — read from the project you are in. Add one line to your shell's startup file:

| Shell | File | Line |
| --- | --- | --- |
| bash | `~/.bashrc` | `source <(opk completion bash)` |
| zsh | `~/.zshrc` | `source <(opk completion zsh)` |
| fish | `~/.config/fish/config.fish` | `opk completion fish \| source` |
| PowerShell | `$PROFILE` | `opk completion powershell \| Out-String \| Invoke-Expression` |

Open a new terminal, then try `opk env deploy <Tab>`. The line generates the script on every start, so completion stays in step when you update opk.

## Update and uninstall

To update, run the install command again. To uninstall, delete the binary (`rm ~/.local/bin/opk`) and, if you want, its settings and connections in `~/.config/orgpunk`.

## Windows

Download `opk-x86_64-pc-windows-msvc.zip` from the [releases page](https://github.com/OrgPunk/cli/releases), unpack `opk.exe` and put it in a folder on your `PATH`.

## Platforms

| OS | CPU |
| --- | --- |
| Linux | x86_64, arm64 (static binaries: any distro, Alpine and distroless included) |
| macOS | Apple Silicon, Intel |
| Windows | x86_64 |

## From source

You need Rust; `rustup` picks the right version from `rust-toolchain.toml`.

```sh
git clone https://github.com/OrgPunk/cli.git
cd cli
cargo install --path crates/cli
```

Next: [How OrgPunk works](philosophy.md).
