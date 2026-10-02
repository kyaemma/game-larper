<p align="center">
  <img src="assets/branding/game-larper-banner.png" width="540" alt="Game Larper">
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

Windows builds are available from [Releases](https://github.com/kyaemma/game-larper/releases). Grab the latest `GameLarper-win-x64.zip`, extract it, and keep both executables together:

```text
GameLarper.exe
GameLarper.Runner.exe
```

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

## license

Game Larper is licensed under the [MIT License](LICENSE).

Have a good larping ! :D
