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

Game Larper is a small Windows tray app. You pick a game Discord already knows how to recognize. It starts a tiny local stand-in whose path matches that game. Discord does the rest: the name, the timer, the icon, the profile.

It does not launch the real game. It does not touch your Steam library. It does not log into Discord, and it does not farm Quests or rewards.

## why

why not

## does it work

Sometimes. Discord's detector is undocumented and it moves. The useful manual test is ELDEN RING: search it, press Play, wait a few seconds, look at your profile.

Pause should make the activity go away. Resume should bring it back. Quit should leave no extra process behind.

If nothing shows up, the log has the process id, the fake path, and the window handle. Settings → Logs → View shows it live (Copy grabs it for a bug report); the daily files are in `%LOCALAPPDATA%\GameLarper\logs`, with your profile folder written as `%LOCALAPPDATA%` / `%USERPROFILE%`. The checklist lives in [docs/DETECTION.md](docs/DETECTION.md).

## a queue, because of course

You can line up a few games with durations, or arm the whole queue for a local time. The app has to be running. If it was closed when that time passed, the schedule is marked missed and nothing starts late.

## download

Grab the latest `GameLarper-win-x64.zip` from [Releases](https://github.com/kyaemma/game-larper-rust/releases), extract it, and keep both executables together:

```text
GameLarper.exe
GameLarper.Runner.exe
```

Windows may show an unknown-publisher / SmartScreen warning because the binaries are not code-signed yet.

## build, if you're weird

Windows x64, Rust stable 1.98, then:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\build-release.ps1
```

Run `artifacts\release\win-x64\GameLarper.exe`. Keep `GameLarper.Runner.exe` beside it.


## license

Game Larper is licensed under the [MIT License](LICENSE).
