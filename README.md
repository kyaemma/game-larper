<p align="center">
  <img src="assets/branding/game-larper-banner.png" width="540" alt="Game Larper">
</p>

<h1 align="center">LARPINNNGGG</h1>

<p align="center">
Pick a game<br>
Press play<br>
Discord gets lied to
</p>

<p align="center">You are now gaming. Allegedly.</p>

## what

Game Larper is a small Windows and Linux tray app. You pick a game Discord already knows how to recognize. It starts a tiny local stand-in whose path matches that game. Discord does the rest: the name, the timer, the icon, the profile.

It does not launch the real game. It does not touch your Steam library. It does not log into Discord, and it does not farm Quests or rewards.

## why

why not

## does it work

On tested setups, yes. Discord's detector is undocumented and it moves, so this is never going to be a universal guarantee.

Windows has been manually verified with ELDEN RING. The Linux implementation in `main` has also been manually verified end-to-end by a Linux contributor, including Discord detection. The full Linux matrix — X11, XWayland, pure Wayland and sandboxed Discord packages — is still not exhaustively characterized.

Pause should make the activity go away. Resume should bring it back. Quit should leave no extra process behind.

If nothing shows up, Settings → Logs → View shows the live debug trace and Copy grabs it for a bug report. Daily files live in `%LOCALAPPDATA%\GameLarper\logs` on Windows and `$XDG_DATA_HOME/GameLarper/logs` (or `~/.local/share/GameLarper/logs`) on Linux. User-specific path prefixes are redacted in diagnostics. The detection checklist lives in [docs/DETECTION.md](docs/DETECTION.md).

## a queue, because of course

You can line up a few games with durations, or arm the whole queue for a local time. The app has to be running. If it was closed when that time passed, the schedule is marked missed and nothing starts late.

## linux

Linux support is now in `main`. It keeps the same fake-process idea as Windows, with Linux-specific process identity/lifecycle handling and an X11 window when available.

The current Linux build has been manually tested successfully with Discord, but desktop/session/package differences still matter. X11/XWayland/pure Wayland and Flatpak are documented separately because they do not all expose processes and windows the same way.

There is no Linux release artifact yet, so Linux currently builds from source. The implementation details, known limitations and validation checklist live in [docs/LINUX.md](docs/LINUX.md).

## download

Windows builds are available from [Releases](https://github.com/kyaemma/game-larper/releases). Grab the latest `GameLarper-win-x64.zip`, extract it, and keep both executables together:

```text
GameLarper.exe
GameLarper.Runner.exe
```

Windows may show an unknown-publisher / SmartScreen warning because the binaries are not code-signed yet.

There is no prebuilt Linux archive yet.

## build, if you're weird

Rust 1.98.1.

### Windows x64

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\build-release.ps1
```

Run `artifacts\release\win-x64\GameLarper.exe`. Keep `GameLarper.Runner.exe` beside it.

### Linux x64

Debian/Ubuntu build dependencies:

```bash
sudo apt-get install --yes build-essential pkg-config libfontconfig1-dev
```

Then:

```bash
cargo build --release
./target/release/game-larper
```

If you move the binaries elsewhere, keep `game-larper` and `game-larper-runner` together.

## license

Game Larper is licensed under the [MIT License](LICENSE).
