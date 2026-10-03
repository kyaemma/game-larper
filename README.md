<p align="center">
  <img src="assets/branding/game-larper-banner.png" width="540" alt="Game Larper">
</p>

<p align="center">
  <a href="https://www.rust-lang.org/"><img alt="Rust 1.98.x" src="https://img.shields.io/badge/Rust-1.98.x-000000?style=flat-square&logo=rust&logoColor=white"></a>
  <a href="https://slint.dev/"><img alt="Slint 1.18.1" src="https://img.shields.io/badge/Slint-1.18.1-2379F4?style=flat-square&logo=slint&logoColor=white"></a>
  <img alt="Windows 10 / 11" src="https://img.shields.io/badge/Windows-10%20%2F%2011-0078D4?style=flat-square&logo=windows11&logoColor=white">
  <img alt="Linux supported" src="https://img.shields.io/badge/Linux-supported-FCC624?style=flat-square&logo=linux&logoColor=black">
  <a href="https://github.com/kyaemma/game-larper/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/kyaemma/game-larper/ci.yml?branch=main&style=flat-square&label=CI"></a>
  <a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/github/license/kyaemma/game-larper?style=flat-square"></a>
</p>

<p align="center">
  <a href="https://slint.dev/"><img alt="Made with Slint" src="https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png" height="24"></a>
</p>

## <p align="center">For my larpers 🤗</p>

<p align="center">
Pick a game<br>
Press play<br>
ANNNND MAGIC ✨<br>
<br>
Discord gets lied to
</p>

<p align="center">You are now gaming... (ALMOST)</p>

## what

Game Larper is a small Windows and Linux tray app. You pick a game Discord already knows how to recognize. It starts a tiny local stand-in whose path matches that game. Discord does the rest : the name, the timer, the icon, the profile... (Discord fell into the genjutsu)

It does not launch the real game. It does not touch your Steam library. It does not log into Discord, and it does not farm Quests or rewards. (Edit : Maybe it does, for badges... But SHHHHHHH)

## why

why not

## does it work

yes.

Wants details ? Check the doc you nerd ! [docs/DETECTION.md](docs/DETECTION.md).

## a queue, because of course

It's a way of writing your future 🙏

## linux

YESSSS, penguin is OK.

THANKS TO [DankDown10256](https://github.com/DankDown10256) for the help 🌹

## download

Windows and Linux x64 builds are available from [Releases](https://github.com/kyaemma/game-larper/releases).

### Windows x64

Grab `GameLarper-win-x64.zip`, extract it, and keep both executables together:

```text
GameLarper.exe
GameLarper.Runner.exe
```

### Linux x64

Grab `GameLarper-linux-x64.tar.gz`, extract it, and keep both binaries together:

```text
game-larper
game-larper-runner
```

Then run `./game-larper`. The archive also includes the MIT `LICENSE`.

## build

Rust 1.98.x. CI uses 1.98.1.

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

To build and install Game Larper as a per-user desktop application instead:

```bash
cargo xtask install
```

This installs both binaries to `~/.local/bin`, adds a desktop entry under your XDG data directory, and installs the Game Larper icon. No `sudo` is required.

## license

Game Larper is licensed under the [MIT License](LICENSE).

Have a good larping ! :D
