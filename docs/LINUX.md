# Linux (experimental)

Linux support is experimental until Discord detection has been verified by hand. The code builds,
passes lint, and its process and X11 mechanics are tested automatically on Linux CI. Whether Discord
on Linux shows the game has **not** been checked yet, in any environment.

## Status

| Environment | Status | Why |
| --- | --- | --- |
| Windows, native Discord | **Verified** by hand (ELDEN RING) | See [DETECTION.md](DETECTION.md). |
| Native Discord + X11 | Unverified; mechanics validated automatically | Process identity and the X11 window are tested in CI under Xvfb. Discord's reaction is unknown. First manual target. |
| Native Discord + XWayland (Wayland session) | Unverified | Same runner path as X11 through XWayland. Not separately tested: XWayland is a different X server, and how Discord itself runs there decides what it can see. |
| Native Discord + pure Wayland (no `DISPLAY`) | Unverified | No window is created (Wayland clients cannot enumerate other clients' windows). Only process identity is left, which may or may not be enough. |
| Discord Flatpak | Unverified; expected not to work | The Flatpak sandbox runs Discord in its own PID namespace, so host processes are not visible in its `/proc`. Not tested. No workaround is attempted. |

What the automated Linux CI job proves: the workspace compiles, passes `clippy -D warnings`, and the
tests pass. The tests cover the runner's `/proc` identity, its lifeline and `SIGTERM` exits, its X11
window under Xvfb, and the host's launch, stop and cleanup. It does not prove anything about Discord.

## How it works

Same idea as Windows: copy a small runner to the path a Discord detectable rule names, keep it
alive, and let Discord's own client decide.

### Which rule

Discord's public catalog lists executables per OS. In the cached catalog of 2026‑10‑01, 10,459
entries have `win32` rules, 62 `darwin` and 8 `linux`. ELDEN RING has only
`win32: game/eldenring.exe`. Game Larper therefore keeps using the Windows rule on Linux and stages
an ELF file named `eldenring.exe` under `…/game/`.

This is a **hypothesis**: Discord on Linux is reported to detect games run through Wine/Proton,
whose processes carry Windows executable names, so it plausibly matches `win32` rules against Linux
process names or paths. Nobody has confirmed that it accepts a native ELF with that name.

### Process identity

The runner is started as the staged file itself (`Command::new(staged path)`), so:

| Source | Value |
| --- | --- |
| `/proc/<pid>/exe` | `$XDG_DATA_HOME/GameLarper/runtime/<app id>/game/eldenring.exe` |
| `/proc/<pid>/cmdline` `argv[0]` | the same absolute path |
| `/proc/<pid>/comm` | `eldenring.exe` (the kernel keeps 15 bytes; longer names are cut, e.g. `Cyberpunk2077.e`) |

The host reads all three after launch, logs them, and warns if any does not name the staged file.
`argv[0]` is not rewritten.

Under Wine, `cmdline` looks more like `Z:\…\game\eldenring.exe` instead. If Linux detection fails,
that difference is the first thing to look at.

### Window

- **X11 reachable (`DISPLAY` set: X11 sessions and XWayland):** a 1×1 window at (-32000, -32000),
  created with x11rb (pure Rust, already in the tree through winit).
  - It is `override-redirect`: never managed, decorated, focused or shown in a taskbar, yet still a
    mapped, viewable child of the root window.
  - `WM_CLASS` = `eldenring.exe`/`eldenring.exe` (like Wine), `WM_NAME`/`_NET_WM_NAME` =
    `eldenring`, plus `_NET_WM_PID` and `WM_CLIENT_MACHINE`.
  - Trade-off: unmanaged windows are absent from `_NET_CLIENT_LIST` and have no `WM_STATE`. A
    scanner that only lists managed windows would not see it.
- **No `DISPLAY`:** no window. Wayland gives clients no way to list other clients' toplevels, and
  has no off-screen coordinates, so a Wayland window would only risk focus and taskbar noise.
- `GAME_LARPER_RUNNER_WINDOW=none` forces the windowless mode. It is useful to test whether
  process identity alone is enough.

### Lifetime, and why the PR's runner exited at once

The runner blocks on its stdin, a pipe whose write end only the host holds. Rust creates that pipe
close-on-exec, so no other child of Game Larper inherits it. EOF comes when the host closes the
pipe to stop it, or when Game Larper dies (the kernel closes its descriptors however it exits). The
runner then exits.

The Linux PR used `PR_SET_PDEATHSIG` instead. prctl(2) says that signal is sent when *the thread
that created the child* terminates, not the process. Game Larper launches from short-lived worker
threads (`spawn_launch`, `dispatch_queue_action`), so the runner received `SIGTERM`/`SIGKILL` as
soon as the launching thread returned. That matches "the runner stops right after being launched",
but it was found by reading the code and the man page, not by watching it on a Linux desktop. The
test `runner_outlives_the_thread_that_launched_it` covers it now.

The same PR also read its exit pipe on the calling (UI) thread, which would have frozen the UI for
as long as the runner lived once it did stay up. The exit watch now runs on its own thread.

Stopping:

1. Close the lifeline.
2. Wait 2 s.
3. Send `SIGTERM`, wait 2 s.
4. Send `SIGKILL`, wait.
5. Reap, log the exit code or signal, remove the staged copy.

Signals only ever go to the host's own unreaped child, so the pid cannot have been reused. Nothing
is killed by name.

### Diagnostics

The runner writes tab-separated lines on stderr (`game_larper_core::runner_protocol`). The host
forwards them into the live console and daily log under `[runner]`, prefixed with the PID. Limits:

- At most 200 lines per launch.
- Lines are capped at 1 KiB.
- The pipe is always drained, so a chatty runner never blocks.

The runner sends no full paths; the host logs paths itself, redacted. Any non-protocol output (a
panic, say) shows up as a warning.

### Files, IPC, and redaction

- **Data root:** `$XDG_DATA_HOME/GameLarper`, else `~/.local/share/GameLarper`. Relative or empty
  values are ignored, per the XDG Base Directory spec.
  - Same layout as Windows: `config.json`, `queue.json`, `cache/`, `runtime/`, `logs/`.
  - Runtime copies are created with `O_CREAT|O_EXCL`, so a planted symlink is never written
    through. Copies and runtime folders are `0700`.
  - Any symlink in the runtime tree is refused, like Windows reparse points.
- **Single instance:** an advisory `flock` on `$XDG_RUNTIME_DIR/game-larper.lock` (else in the data
  root) picks the primary.
  - The kernel drops the lock when the holder dies, so it is never stale.
  - Only the lock holder replaces the `0600` activation socket next to it.
  - A second start knocks on the socket and exits.
- **Autostart:** `$XDG_CONFIG_HOME/autostart/game-larper.desktop` (else `~/.config/…`).
  - `Exec=` is quoted per the Desktop Entry spec: reserved characters, `%%`, and backslash escaping.
  - Control characters are refused.
- **Clipboard:** arboard, already linked by Slint. It uses Wayland data-control when the
  compositor offers it, X11/XWayland otherwise.
- **Logs:** paths under `$XDG_RUNTIME_DIR`, `$XDG_DATA_HOME` and `$HOME` are written as those
  variables (`~` for home).

## Known limitations

- **Tray:** Slint's tray uses StatusNotifierItem (D-Bus). GNOME shows it only with an AppIndicator
  extension.
  - Without a tray host, "close to tray" hides the window with no icon to bring it back. Start
    Game Larper again: the running instance shows its window.
  - Or turn off "Close button minimizes to tray".
- **Docked panel:** best effort. Wayland compositors may ignore window positions, and there is no
  owner/skip-taskbar hint, so the panel can show up as a separate window.
- **Fonts:** the UI asks for Segoe UI Variable and Consolas, which Linux usually lacks; fontconfig
  falls back to other fonts. Cosmetic.
- **No window icon** is applied to the Linux runner (no known detection role).
- **No Linux release artifact.** Build from source. A future artifact should contain
  `GameLarper` and `GameLarper.Runner` (the host also finds `game-larper-runner`) side by side, and
  should only ship after the manual test below passes.

## Manual test (Ubuntu VM, native Discord)

Prepare:

1. Ubuntu Desktop 24.04 VM. Log in once with "Ubuntu on Xorg" (X11) and once with the default
   Wayland session (XWayland).
2. Build the integration branch:

   ```bash
   sudo apt-get install --yes build-essential pkg-config libfontconfig1-dev
   git clone https://github.com/kyaemma/game-larper.git
   cd game-larper
   git checkout integrate/linux-support
   cargo build --release
   ```

3. Install native Discord (the `.deb` from discord.com, not the Flatpak or Snap). Log in. Enable
   Settings → Activity Privacy → "Share your detected activities".

Run:

1. Start Game Larper:

   ```bash
   ./target/release/game-larper
   ```

2. Open Settings → Logs → View.
3. Search **ELDEN RING**, select it, press **Play**. In the console, expect:
   - `Launching ELDEN RING … as game/eldenring.exe`
   - `Staged …/runtime/1402418436809953330/game/eldenring.exe`
   - `Ready in … ms: backend=x11 X11 window=0x…`. On Wayland without `DISPLAY` this says
     `backend=none`.
   - `/proc identity: exe=… argv0=… comm=eldenring.exe` with no mismatch warning.
   - `[SUCCESS] Running PID=…`
4. Check from a terminal (`<pid>` from the log):

   ```bash
   cat /proc/<pid>/comm
   readlink /proc/<pid>/exe
   tr '\0' ' ' < /proc/<pid>/cmdline; echo
   xwininfo -root -tree | grep -i eldenring
   xprop -id <window id from the log>
   ```

5. Wait about a minute. Look at Discord: the user panel activity and Settings → Registered Games.
   Record what Discord shows (nothing, the game, or a different name).
6. If nothing shows up, repeat with `GAME_LARPER_RUNNER_WINDOW=none ./target/release/game-larper`.
   This tells whether the X11 window matters at all.
7. Press **Stop**. Expect `PID=… exited after … ms: exit code 0` and `PID=… stopped`, and
   `pgrep -f eldenring.exe` prints nothing.
8. Close Game Larper with **Play** running, then run `pgrep -f eldenring.exe`. Also try
   `kill -9` on Game Larper. In both cases the runner must be gone within a second.
9. Use **Copy** in the console and attach the text to the test report.

Then repeat in the Wayland session (step 3 shows whether XWayland was used), on Fedora and Arch, and
separately with the Flatpak Discord (expected not to detect).
