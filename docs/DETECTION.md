# Detection

Game Larper does not talk to Discord. It copies a small runner to a relative path taken from Discord's public detectable catalog and keeps an off-screen top-level window alive. Discord's own client decides whether that process counts as a game.

This is not Rich Presence. There is no bot token, no client patch, and no local Discord credential.

## What the live catalog says

Fetched without credentials from `https://discord.com/api/v10/applications/detectable` (v9 returns the same document). ELDEN RING is:

- application id `1402418436809953330`
- Steam app `1245620`
- the only Windows executable rule: `game/eldenring.exe`

A bare `eldenring.exe` does not match that row. The fake path has to end with `game\eldenring.exe`.

Rules prefixed with `>` are shared hosts (`hl2.exe`, `javaw.exe`) paired with command-line arguments. They are not file names. Game Larper will not turn them into a path. A game is selectable only when it has another safe, non-launcher Windows `.exe` that is not on Discord's exclusion list (crash handlers, `launcher.exe`, `vcredist*.exe`, and the rest of that list).

## What the runner does

The runner is a GUI-subsystem process. It registers `GameLarper.RunnerWindow`, creates an ownerless `WS_OVERLAPPEDWINDOW` at `(-32000, -32000)` sized 1×1, with `WS_EX_APPWINDOW | WS_EX_NOACTIVATE`, shows it with `SW_SHOWNOACTIVATE`, and pumps messages until close. The title is the executable stem (`eldenring`). Idle CPU should be a blocked `GetMessage`.

The parent copies that binary under `%LOCALAPPDATA%\GameLarper\runtime\<application id>\`, starts it in its own directory, and refuses the launch if the process dies or no visible ownerless window appears. Stop posts `WM_CLOSE`, then terminates that exact process handle. Nothing is killed by image name.

If the destination file already exists and is not this runner, it is left alone and the launch uses `runtime\<id>\session-…\` in front of the same relative suffix, so Discord still sees `game\eldenring.exe`.

## What the runner does on Linux

Same staging, same relative path: the runner is copied to `…/runtime/<application id>/game/eldenring.exe` (forward slashes; the catalog rule's `game/eldenring.exe` suffix is what matters).

Process identity comes from `/proc` instead of a window enum: the host accepts the child once `/proc/<pid>/comm` or any `cmdline` argument ends in `eldenring.exe` (a script runs as `sh <path>`, so `argv[0]` alone is not enough). The launch fails with the same "runner did not create its game window" error if the process dies or never shows that identity within the startup budget.

The window, when a display server is reachable:

- **X11** (preferred whenever `DISPLAY` is set): a 1×1, undecorated, non-resizable, override-redirect window at `(-32000, -32000)` — unmanaged, off-screen, but a real mapped window in the X11 window tree. `WM_CLASS` is instance `eldenring` / class `eldenring.exe`, title `eldenring`. Discord's Linux client runs under XWayland, so this is the window it can enumerate.
- **Wayland** (fallback): a toplevel with app id `eldenring.exe`, minimized the moment it appears so the 1×1 surface never holds keyboard focus. Wayland has no off-screen coordinates and no cross-client window enumeration.
- **No display**: no window at all; the process name and path still stand, and the runner idles.

Lifecycle: the host arms `PR_SET_PDEATHSIG(SIGTERM)` in the child before `exec`, the runner re-arms `SIGKILL` on itself, so the staged process dies with the app (no job objects here). Stop sends `SIGTERM`, waits up to 5s, then `SIGKILL` — still only that exact PID, never a kill by image name. The log line is the Windows one: `HWND=0x0` is expected (there is no handle), and `integrity=uid:N` replaces the Windows integrity level.

Single instance is a Unix socket at `runtime/activate.sock` instead of a named mutex: binding it makes you the primary, a second copy connects, sends `activate`, and exits.

## Manual checklist

Do not treat a green test run as proof that Discord Desktop recognized the game.

1. Start Discord Desktop with activity sharing / game detection enabled.
2. Start the Rust build of Game Larper.
3. Search for ELDEN RING and press Play.
4. Confirm the log shows a live PID, a path ending in `game\eldenring.exe`, and a non-zero HWND. On Linux the path ends with `game/eldenring.exe` and `HWND=0x0` is normal; check `title=eldenring` and `integrity=uid:` instead.
5. Wait for Discord's scan.
6. Open your profile activity / Registered Games.
7. Confirm ELDEN RING is the detected game, including the native profile if Discord offers one.
8. Pause and wait for the activity to disappear.
9. Resume and wait for it to return.
10. Stop and wait for it to disappear.
11. Quit Game Larper from the tray and confirm the fake process is gone.

The development build has been manually verified with ELDEN RING on Discord Desktop on Windows. That is evidence that the detection path works, not a guarantee that every game or future Discord client build will behave the same way. The Linux path has the same checklist above minus the HWND step; treat it as unverified until you have run it against your own Discord client.
