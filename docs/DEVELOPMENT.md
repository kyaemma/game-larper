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

Config is camelCase JSON so a file written by the older C# build still loads. The catalog cache is `cache/catalog.json`. If that file is missing, a legacy `cache/discord-detectables.json` is read once.

## Linux

Linux x64, Rust stable 1.98.1, edition 2024. `scripts/check.sh` is the counterpart of `check.ps1` (fmt check, clippy, test) and is what CI runs:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p game-larper
```

Only fontconfig is needed at build time (`libfontconfig1-dev` on Debian/Ubuntu); the rest of the stack, X11 and Wayland included, is loaded at runtime. The CI workflow has a Windows job and a Linux job, both pinned to 1.98.1, both running the check script.

Platform-specific code lives in `crates/game-larper/src/platform/` (`windows.rs` / `unix.rs`) and `crates/game-larper/src/host.rs` (process spawn/stop per target). On Linux:

- Data lives in `$XDG_DATA_HOME/GameLarper`, falling back to `~/.local/share/GameLarper`.
- Single instance is a Unix socket at `runtime/activate.sock`; a second copy knocks on it and exits.
- Startup is an XDG autostart entry (`~/.config/autostart/game-larper.desktop`), and "open folder" goes through `xdg-open`.
- The runner test is `crates/game-larper-runner/tests/proc.rs`: it checks the `/proc` identity, and when `DISPLAY` is set to a reachable X server, the X11 window (title, `WM_CLASS`, off-screen position) plus a clean `SIGTERM`. It skips the window assertions headless.


## Publishing a release

The GitHub `Release` workflow builds and publishes the Windows ZIP. Run it manually from the Actions tab with a semantic version tag such as `v0.1.0`, or push an existing `v*` tag.

For a manual run, the workflow:

1. runs the full release build,
2. creates the requested annotated tag at the selected `main` commit,
3. computes the ZIP SHA-256,
4. creates the GitHub Release, and
5. uploads `GameLarper-win-x64.zip`.

The release archive contains `GameLarper.exe`, `GameLarper.Runner.exe`, and the MIT `LICENSE`. The two executables must stay together.
