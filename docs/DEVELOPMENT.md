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

The GitHub `Release` workflow builds and publishes the Windows ZIP. Run it manually from the Actions tab with a semantic version tag such as `v0.1.0`, or push an existing `v*` tag.

For a manual run, the workflow:

1. runs the full release build,
2. creates the requested annotated tag at the selected `main` commit,
3. computes the ZIP SHA-256,
4. creates the GitHub Release, and
5. uploads `GameLarper-win-x64.zip`.

The release archive contains `GameLarper.exe`, `GameLarper.Runner.exe`, and the MIT `LICENSE`. The two executables must stay together.
