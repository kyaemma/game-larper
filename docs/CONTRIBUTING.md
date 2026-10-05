# Contributing

Game Larper is a small Rust workspace: a Slint tray app, a stand-in process, and a safe core.
This file is the contract for a change. The rest of `docs/` explains the design.

## Read this first

| File | Why |
| --- | --- |
| [ARCHITECTURE.md](ARCHITECTURE.md) | the three crates, threading rules, memory notes |
| [DEVELOPMENT.md](DEVELOPMENT.md) | toolchain, day-to-day commands, release flow |
| [DETECTION.md](DETECTION.md) | what the runner must look like, Windows manual checklist |
| [LINUX.md](LINUX.md) | Linux status table, `proc`/X11 mechanics, Linux manual test |
| [releases/](releases/) | versioned notes the Release workflow publishes |

Detection is never proven by CI. If your change touches anything a detector could notice (path,
window, process identity, lifetime, logs), say in the PR which manual checklist step you ran, on
what OS and Discord build, or say plainly that you could not run it.

## Setup

The Rust toolchain is pinned by `rust-toolchain.toml` to **1.98.1**; the workspace uses
Rust edition 2024 from `Cargo.toml`. rustup installs the pinned toolchain for you. Linux build
dependencies on Debian/Ubuntu:

```bash
sudo apt-get install --yes build-essential pkg-config libfontconfig1-dev
# plus xvfb if you want to run the X11 runner test without a display
```

X11, Wayland, GL and xkbcommon are loaded at runtime, so they need no headers.

```bash
git clone https://github.com/<you>/game-larper.git
cd game-larper
cargo build --workspace
cargo run -p game-larper
```

`cargo run` looks for `game-larper-runner` next to the host, which is where Cargo puts both
binaries. Keep the two together in any layout you copy them to. `cargo xtask install` (alias in
`.cargo/config.toml`) builds and installs both plus a desktop entry on Linux, no sudo.

Note: a bare `cargo build` builds the three default members; anything in the scripts uses
`--workspace`, which also covers `xtask`.

## Branch, commit, PR

- Branch from `main`. Branch names are `type/description`: `feat/linux-desktop-install`,
  `chore/credits-ui`, `perf/memory-pass`, `docs/readme-badges`, `audit/v0.2-optimization-pass`.
- Commits follow Conventional Commits with an optional scope: `feat(linux):`, `fix(ui):`,
  `perf(search):`, `style(xtask):`, `docs:`, `chore(release):`. The scope is usually the crate or
  the area, not the file.
- Open the PR against `main`. `.github/workflows/ci.yml` runs a Windows job and a Linux job on
  every PR; both must be green. Merges are GitHub merge commits (`Merge pull request #N from …`).
- One concern per PR. A perf change that also reformats the code it touches is reviewable; a perf
  change that also rewrites an unrelated module is not.
- Say what you tested. For behavior changes, quote the relevant log lines rather than "it works".

## The quality gate

`scripts/check.ps1` (Windows) and `scripts/check.sh` (Linux) are the same three commands:

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run them before you push. `cargo fmt --all` writes; CI and the scripts use `--check`.

The Linux CI job then adds `cargo build --workspace` and runs the tests under
`xvfb-run --auto-servernum` with `GAME_LARPER_REQUIRE_X11=1`, so the X11 runner test cannot be
skipped silently there. Locally that test runs whenever `DISPLAY` reaches an X server.

`scripts/build-release.ps1` / `scripts/build-release.sh` is the release gate: format, lint, test,
release build, package. It refuses to run on a Rust that is not 1.98.x.

## Tests

| Change | Where the test goes |
| --- | --- |
| catalog, search, path safety, config, queue, session, runner protocol | `crates/game-larper-core/tests/<module>.rs`, one file per module |
| runner window and process contract | `crates/game-larper-runner/tests/window.rs` (Windows), `linux.rs` (Linux) |
| host launch/stop/cleanup, platform glue, logging, artwork, app wiring | `#[cfg(test)] mod tests` inside the module |

Conventions:

- Test names are full sentences stating the contract: `runner_ends_on_sigterm`,
  `the_best_executable_is_chosen_while_parsing`, `corrupt_config_falls_back_to_defaults`.
- No network. Catalogs come from fixtures and temp dirs; a test that needs the real endpoint is
  a manual test, not a unit test.
- Tests must say what they do not prove. The runner tests cover the window, the `/proc` identity
  and the lifeline; they do not say that Discord Desktop shows the game.

Environment knobs:

- `GAME_LARPER_REQUIRE_X11=1` — fail instead of skip when no X server is reachable (CI sets it).
- `GAME_LARPER_RUNNER_WINDOW=none` — force the windowless runner, to check whether process
  identity alone is enough.

## Where things live

| Area | Files |
| --- | --- |
| catalog parse/cache, search ranking, path safety, config, queue, session clock | `crates/game-larper-core/src/*.rs` + `tests/*.rs` |
| runner diagnostics protocol (tab-separated on stderr) | `game-larper-core/src/runner_protocol.rs` |
| stand-in process | `crates/game-larper-runner/src/{windows,linux}.rs`, `src/linux/x11.rs` |
| process mechanics, launch/stop, cleanup | `crates/game-larper/src/host/{mod,windows,linux}.rs` |
| desktop glue (tray, autostart, clipboard, single instance) | `crates/game-larper/src/platform/{windows,linux}.rs` |
| Slint shell, state push, event loop | `crates/game-larper/src/{app,commands,panel,main}.rs` |
| UI markup, theme, icons in markup | `crates/game-larper/ui/*.slint` |
| catalog download, artwork fetch/cache | `crates/game-larper/src/{net,art_cache}.rs` |
| console/log levels, redaction | `crates/game-larper/src/log.rs` |
| Linux desktop installer | `xtask/src/main.rs` |
| CI and release automation | `.github/workflows/{ci,release}.yml` |

Slint files are compiled by `crates/game-larper/build.rs`; the generated Rust goes through
Cargo's build output (`OUT_DIR` under `target/`) and is not source-controlled. Never commit
generated UI code. UI icons are monochrome white SVGs tinted at runtime; see
`assets/ui/README.md`. Branding lives in `assets/branding/`.

## Rules the code depends on

- `game-larper-core` stays safe Rust. The workspace denies `unsafe_op_in_unsafe_fn`; keep `unsafe`
  out of core, and justify any `unsafe` elsewhere in the PR description.
- Platform code stays behind target modules: `platform/{windows,linux}.rs` for desktop glue,
  `host/{windows,linux}.rs` under the shared `host/mod.rs` for runner process mechanics, and
  `game-larper-runner/src/{windows,linux}.rs`. Gate Linux work on `target_os = "linux"`. Other
  targets are meant to fail to compile (`compile_error!` in the runner); do not soften that.
- The UI thread never waits on the child process. Launch and stop run on a worker holding the
  runner host lock, results come back through a channel drained on the Slint event loop, artwork
  is fetched off-thread. `slint::Image` is not `Send` — keep it on the UI thread.
- Rust is the source of truth. The main window and the tray are separate Slint components; both
  receive pushed updates instead of sharing state.
- Nothing is ever killed by image name. Stop goes to the exact process handle (Windows) or to the
  host's own unreaped child (Linux). Runtime copies are created with `O_CREAT|O_EXCL`, symlinks in
  the runtime tree are refused (Windows: reparse points), cleanup deletes only files this launch
  created, and remote executable strings never leave the runtime root. Logs redact paths.
- Config stays camelCase JSON so a file written by the older C# build still loads. The catalog
  cache is `cache/catalog.json`, with a one-shot read of legacy `cache/discord-detectables.json`.
- Application data stays under `%LOCALAPPDATA%\GameLarper\` on Windows and
  `$XDG_DATA_HOME/GameLarper` (else `~/.local/share/GameLarper`) on Linux. OS integration is
  the deliberate exception: Windows startup uses the registry; Linux autostart / desktop install
  uses the relevant XDG config/data locations and `~/.local/bin`.
- Dependencies: Slint stays `~1.18.1` with `default-features = false`, `backend-winit` and
  `renderer-femtovg` (no Skia). Prefer crates already in the tree — x11rb comes via winit, arboard
  via Slint — and record the reason for a new one in the PR.

## Documentation

- `docs/` files are `UPPERCASE.md`, dense, and factual. Link relatively:
  `[LINUX.md](LINUX.md)`.
- Update the doc with the behavior: design or threading change → ARCHITECTURE.md; window, path,
  lifetime or detection detail → DETECTION.md; Linux status and mechanics → LINUX.md; commands and
  toolchain → DEVELOPMENT.md.
- Write the way LINUX.md does: separate what CI proves from what a human verified by hand, and
  label a hypothesis as a hypothesis. Do not upgrade a claim you did not test, and do not leave a
  stale one behind.
- Release notes live in `docs/releases/v<version>.md`. The Release workflow publishes that file
  when it exists, so a version bump in `[workspace.package]` in `Cargo.toml` should land with its
  notes.

## Releases are maintainer-run

`.github/workflows/release.yml` either runs manually (`workflow_dispatch` with a `v*` tag) or on a
push to `main` whose commit message starts with `release: v` and whose `Cargo.toml` changed. The
tag must equal the workspace version. It builds both platforms, creates the tag, computes the
SHA-256, and publishes `GameLarper-win-x64.zip` plus `GameLarper-linux-x64.tar.gz`. Ask for a
release in an issue instead of cutting one yourself.

## Out of scope

The project deliberately does none of the following. Please do not add them:

- Talking to Discord beyond the public detectable catalog: no bot token, no user token, no client
  patch, no Rich Presence.
- Launching the real game, touching a Steam library, or farming Quests and rewards.
- Killing processes by name, writing outside the data root, or relaxing the path and symlink
  checks to make a launch succeed.

## License

MIT. Your contribution ships under the same [LICENSE](../LICENSE) as the rest of the project.
