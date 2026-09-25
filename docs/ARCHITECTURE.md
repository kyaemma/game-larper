# Architecture

Three crates.

`game-larper-core` is safe Rust. It parses the detectable catalog, ranks search, rejects unsafe executable paths, stores config and the queue, and owns the session clock plus the queue state machine. Time spent playing uses `Instant`. The wall clock is only for a one-shot scheduled start.

`game-larper-runner` is the dummy process. It has no console. Its only job is the off-screen window described in DETECTION.md.

`game-larper` is the Slint shell, the catalog download, the artwork cache, and the process owner. Slint 1.18 is built with winit and FemtoVG. Skia is not enabled. The window is frameless (`no-frame`, `WindowMoveArea`). The tray is Slint's `SystemTrayIcon`. Those two components do not share Slint state; Rust is the source of truth and pushes updates to both.

The UI thread does not wait on the child process. Launch and stop run on a worker that holds the runner host lock. Results come back through a channel drained on the Slint event loop. Artwork bytes are fetched off the UI thread. `slint::Image` values stay on the UI thread because they are not `Send`.

One fake game runs at a time. The queue is the same runner, advanced by the state machine: when a duration elapses, the runner stops, waits two seconds, then the next item starts. A failed launch stops the queue instead of skipping forever. An armed start time that is already past when the app opens is marked missed.

Paths, config, catalog, images, runtime copies, and logs live under `%LOCALAPPDATA%\GameLarper\`. Remote executable strings never leave that runtime root. Cleanup deletes only files this launch created, and it stops if a directory in the chain is a reparse point.
