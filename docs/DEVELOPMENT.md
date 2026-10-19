# Development

Rust stable 1.98.1, edition 2024, MSVC x64. Slint 1.18.1 with `backend-winit` and `renderer-femtovg`. Default features are off so Skia is not compiled in. `scripts/check.ps1` is the short quality pass. `scripts/build-release.ps1` formats, lints, tests, builds release, and copies:

```text
artifacts/release/win-x64/GameLarper.exe
artifacts/release/win-x64/GameLarper.Runner.exe
```

Day to day:

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p game-larper
```

`cargo run` looks for `game-larper-runner.exe` next to itself, which is where Cargo puts both binaries. The release script renames them to `GameLarper.exe` and `GameLarper.Runner.exe`.

The runner test copies the binary to `eldenring.exe`, checks the extended style, the off-screen position, the title, and a clean `WM_CLOSE`. It does not prove that Discord Desktop will show the game.

## Linux (experimental)

Linux x64, same toolchain. Build dependencies on Debian/Ubuntu: `build-essential pkg-config libfontconfig1-dev`. X11, Wayland, GL and xkbcommon are loaded at runtime. `scripts/check.sh` is the counterpart of `check.ps1`. CI runs the tests under `xvfb-run` with `GAME_LARPER_REQUIRE_X11=1`, so the X11 runner test cannot be skipped there. Locally it runs whenever `DISPLAY` reaches an X server.

Platform code stays behind target modules: `platform/{windows,linux}.rs` for desktop glue, `host/{windows,linux}.rs` for runner process mechanics under the shared `host/mod.rs`, and `game-larper-runner/src/{windows,linux}.rs`. Anything Linux-specific is gated on `target_os = "linux"`, and other targets fail to compile on purpose. See [LINUX.md](LINUX.md) for the design and the manual test.

Config is camelCase JSON so a file written by the older C# build still loads. The catalog cache is `cache/catalog.json`. If that file is missing, a legacy `cache/discord-detectables.json` is read once.


## Publishing a release

The GitHub `Release` workflow (`.github/workflows/release.yml`) builds the Windows and Linux packages and publishes them as one GitHub Release. It starts in one of two ways:

- **Push to `main`** that changes `Cargo.toml`, when the head commit message starts with `release: v`. The tag is `v` plus the workspace version.
- **Manual run** from the Actions tab, with a tag such as `v0.2.1`. The release is built from the commit the workflow is run on.

Before building, the workflow reads the version from `[workspace.package]` in `Cargo.toml` and fails if the tag is not a semantic version tag (`vMAJOR.MINOR.PATCH`, optional suffix), if it does not match that version, or if a GitHub Release for it already exists.

Both builds run on Rust 1.98.1 and use the release scripts, so they format, lint and test before building:

| Artifact | Runner | Script |
| --- | --- | --- |
| `GameLarper-win-x64.zip` | `windows-latest` | `scripts/build-release.ps1` |
| `GameLarper-linux-x64.tar.gz` | `ubuntu-latest` | `scripts/build-release.sh` |

Each package holds the app, its runner and the MIT `LICENSE`. The Windows ZIP has `GameLarper.exe` and `GameLarper.Runner.exe`; the Linux archive has `game-larper` and `game-larper-runner`. The app and runner must stay together.

When both builds pass, the publish job:

1. uses `docs/releases/v<version>.md` as the release notes, or a short generic note if that file does not exist,
2. appends a `SHA-256` section with the hash of each package,
3. creates the release tag at the release commit, reusing the tag if it already exists, and
4. creates the GitHub Release with both packages attached.

Write `docs/releases/v<version>.md` and merge it before releasing.
