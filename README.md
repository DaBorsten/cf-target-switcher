# cf-ts

Interactively switch your Cloud Foundry org and space without logging in again.

`cf-ts` lists the orgs and spaces you have access to, lets you fuzzy-search them, and then runs `cf target -o ORG -s SPACE` for you. It starts with your current target selected.

## Requirements

- The [cf CLI](https://github.com/cloudfoundry/cli) on your `PATH`
- An active session (`cf login`)

## Installation

**Linux and macOS**

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/DaBorsten/cf-target-switcher/main/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/DaBorsten/cf-target-switcher/main/install.ps1 | iex
```

The installer downloads the right binary for your system from the latest release and checks it against the published SHA-256 checksum. It also puts the install directory on your `PATH`, so `cf-ts` works in any new terminal:

| OS            | Installs to               | PATH is set in                                        |
| ------------- | ------------------------- | ----------------------------------------------------- |
| Linux, macOS  | `~/.local/bin/cf-ts`      | your shell profile (`.zshrc`, `.bashrc`, `.bash_profile`, fish config or `.profile`) |
| Windows       | `%LOCALAPPDATA%\cf-ts\bin\cf-ts.exe` | the user `Path` environment variable (no admin rights needed) |

Run the same command again to update. You can set these environment variables to change the defaults:

| Variable               | Effect                                              |
| ---------------------- | --------------------------------------------------- |
| `CF_TS_VERSION`        | Install a specific release, e.g. `v0.1.0`           |
| `CF_TS_INSTALL_DIR`    | Install somewhere else                              |
| `CF_TS_NO_MODIFY_PATH` | Set to `1` to leave your `PATH` alone               |

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/DaBorsten/cf-target-switcher/main/install.sh | CF_TS_VERSION=v0.1.0 sh
```

### Manual download

Each [release](https://github.com/DaBorsten/cf-target-switcher/releases/latest) has these archives and a `SHA256SUMS.txt`. Unpack the archive and put `cf-ts` (`cf-ts.exe` on Windows) in a directory on your `PATH`.

| Platform            | Archive                                   |
| ------------------- | ----------------------------------------- |
| Linux x86_64        | `cf-ts-x86_64-unknown-linux-musl.tar.gz`  |
| Linux ARM64         | `cf-ts-aarch64-unknown-linux-musl.tar.gz` |
| macOS Intel         | `cf-ts-x86_64-apple-darwin.tar.gz`        |
| macOS Apple Silicon | `cf-ts-aarch64-apple-darwin.tar.gz`       |
| Windows x86_64      | `cf-ts-x86_64-pc-windows-msvc.zip`        |
| Windows ARM64       | `cf-ts-aarch64-pc-windows-msvc.zip`       |

The macOS binary is not notarized. If you downloaded it in a browser and Gatekeeper blocks it, run `xattr -d com.apple.quarantine ./cf-ts`. The install script does not have this problem.

### Verifying a download

Every archive comes with a signed [build provenance attestation](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations). It proves that the file was built by this repository's release workflow from the tagged commit. You can check it with the [GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify cf-ts-x86_64-unknown-linux-musl.tar.gz --repo DaBorsten/cf-target-switcher
```

Releases are immutable: once published, their files cannot be replaced.

### From source

```sh
cargo install --git https://github.com/DaBorsten/cf-target-switcher
```

### Uninstall

Delete the binary. If you want, also remove the `PATH` entry the installer added: the line marked `# Added by the cf-ts installer` in your shell profile, or `%LOCALAPPDATA%\cf-ts\bin` from your user `Path` on Windows.

## Usage

```sh
cf-ts
```

1. Pick an org (type to filter, arrow keys to move, Enter to select).
2. Pick a space.
3. `cf target` is run with your choice.

Press Esc in the space list to go back to the orgs, for example after just looking which spaces an org has. Press Esc in the org list to cancel without changing the target.

```text
Options:
  -h, --help     Print help
  -V, --version  Print version
```

### Favorites

Org names can be hard to tell apart. In the org list, press Tab on an org and type a name of your own for it. Favorites are listed first and show your name in front of the org name. Typing filters on both.

```text
? Org › type to filter
❯ ★ Shared (example-shared)
  ★ Team A Test (example-team-a-test)
  ───────────────────────────────────
    example-team-a-dev
    example-team-b-dev

   Tab  name selected entry   Esc  quit
```

Press Tab on a favorite again to rename it, or clear the name to remove it. Esc leaves it as it was. Favorites belong to the API endpoint you are logged in to, so an org with the same name on another endpoint is not affected. The names are stored in `$CF_HOME/.cf/cf-ts.json`, by endpoint and org name, so you can also edit the file by hand:

```json
{
  "https://api.cf.example.com": {
    "orgs": {
      "example-shared": "Shared",
      "example-team-a-test": "Team A Test"
    }
  }
}
```

### Saved targets

If you keep going back to the same org and space, save the two together. Pick the org, then press Tab on the space and type a name. Saved targets are listed above the orgs, and Enter on one switches to its org and space at once, without asking for the space.

```text
? Org › type to filter
❯ ◆ Team B dev (example-team-b-dev / dev)
  ───────────────────────────────────────
  ★ Shared (example-shared)
  ★ Team A Test (example-team-a-test)
  ───────────────────────────────────────
    example-team-a-dev
    example-team-b-dev

   Tab  name selected entry   Esc  quit
```

Press Tab on a saved target, in either list, to rename it, or clear the name to remove it. Targets are stored next to the favorites in `cf-ts.json`, by org name and space name:

```json
{
  "https://api.cf.example.com": {
    "targets": {
      "example-team-b-dev": {
        "dev": "Team B dev"
      }
    }
  }
}
```

`cf-ts` reads the current target from `$CF_HOME/.cf/config.json` (falling back to your home directory), the same location the cf CLI uses.

## Security

`cf-ts` never sees your password and makes no network requests of its own. It only runs `cf curl` and `cf target`, plus `cf oauth-token` to check for a session when `cf curl` returns nothing, so authentication stays with the cf CLI. The token that prints is not used. From `config.json` it only uses the API endpoint and the names and GUIDs of the current org and space. The tokens in that file are neither stored, logged nor sent anywhere. The only file it writes is `cf-ts.json` with your favorites. Org and space names are passed to `cf` as plain arguments, never through a shell.

## Releasing

1. Bump `version` in `Cargo.toml` and run `cargo build` so `Cargo.lock` is updated.
2. Commit, then tag and push:

   ```sh
   git tag v0.1.0
   git push origin main v0.1.0
   ```

The [Release workflow](.github/workflows/release.yml) checks that the tag matches the crate version, builds all targets, attests them, and publishes a GitHub release with the archives and checksums. It then verifies the attestations and runs both install scripts against the new release on Linux, macOS and Windows.

Because releases are immutable, a published tag cannot be reused. If something is wrong with a release, publish a new patch version.

## License

[MIT](LICENSE)
