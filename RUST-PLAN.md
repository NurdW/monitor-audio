# Rust rewrite plan

Target: replace the Python backend **and** panel with Rust in one step, on branch
`rust-rewrite`. Python stays on `main` and keeps shipping until Rust provably
matches. The KWin script stays JavaScript — KWin only loads QML/JS scripts, so
"entire" cannot include it.

## Toolchain

`rustup` minimal profile into `~/.cargo` (no sudo, no system package). Verify
with `cargo --version` before starting.

## Crate choices

| Need | Crate | Why |
| --- | --- | --- |
| D-Bus service + calls | `zbus` (tokio) | pure Rust, no libdbus; owns `local.MonitorAudio` |
| Main loop / timers | `tokio` | burst polling and keep-alive are both timers |
| PipeWire state | `serde_json` over `pw-dump` output | same source pactl cannot see: client pids |
| Subprocess | `std::process::Command` | `pw-dump`, `pactl`, `wpctl` |
| GUI | `egui` + `eframe` | pure Rust, no system Qt, keeps AppImage self-contained |
| WAV tone / silence | hand-rolled 44-byte header + samples | trivial, avoids a dep |
| Errors | `anyhow` for glue, `thiserror` for the few typed ones | |

Do **not** use `libpulse-binding`. Everything the Python version does through
`pactl` is a `Command` call; matching its behaviour exactly matters more than
avoiding a subprocess.

## File layout

```
rust/
  Cargo.toml                 # workspace
  crates/
    monitor-audio-core/      # library: no GUI, no main()
      src/lib.rs
      src/config.rs          # read/write ~/.config/monitor-audio/config.json
      src/snapshot.rs        # pw-dump + pactl parsing -> Sink, Stream, Snapshot
      src/routing.rs         # resolve_output, stream_belongs, owner_pids, no-fight
      src/keepalive.rs       # silent stream lifecycle
      src/dbus.rs            # local.MonitorAudio: Route Status JsonStatus Reset Forget
      src/silence.rs         # write the silence wav
    monitor-audio-daemon/    # thin main(): dbus + timers
      src/main.rs
    monitor-audio-panel/     # eframe app
      src/main.rs
      src/tabs.rs            # Monitors / Applications / Audio streams / Log
      src/install.rs         # install_files, install_backend, remove_files, uninstall_backend
  packaging/                 # AppRun rewritten for a single binary
```

`core` is a library so the checks can drive it without a running GUI or service.

## Port map — behaviour that must survive

Each item is a Python function and the invariant it carries. These are the
reasons the code looks the way it does; losing one reintroduces a fixed bug.

- `resolve_output` (`backend/monitor-audio.py`) — order is manual override, then
  display model match, then `fallback_kind`. Matching is on `QScreen.model()`,
  **not** sink names, because sink names change with profile switches.
- `placed` / `wanted` — place once, never fight. Only a real display change may
  re-move a stream, and only with `force`.
- `stream_belongs` + `owner_pids` — never guess ownership. If one process owns
  several routed windows and no title matches, leave the stream alone. Filter
  PipeWire's own pids: pulse-compat streams all report `pipewire-pulse`'s pid.
- `load_config` — missing file, invalid json, non-object json, wrong types: each
  is logged and recoverable, never fatal.
- Burst polling — `BURST_SECONDS 5.0` at `BURST_INTERVAL_MS 150`, triggered by a
  window arriving on a display or a placed stream vanishing. Title changes alone
  must **not** trigger it.
- Keep-alive — one silent stream per routed output, `KEEPALIVE_SECONDS 300`,
  skipped when `keep_awake` is false or routing is off, cleaned up on SIGTERM,
  and never counted as a routable stream (`is_keepalive`).
- `Route` takes 7 string args over D-Bus. KWin's `callDBus` passes everything as
  a string, so the signature stays `sssssss`.
- `Type=simple` in the systemd unit, not `Type=dbus`: Fedora 44 SIGTERMs
  `Type=dbus` units immediately.
- Panel: never rebuild a table row that has not changed (keeps open dropdowns
  alive); connect the override signal **after** loading the saved value (or
  opening the panel rewrites the settings file); the Monitors dropdown is both
  the display of the current output and the control, and it asks the daemon
  rather than re-deriving the answer.

## Order of work

1. `cargo init` the workspace, get `core` compiling with `config` + `snapshot`.
2. Port `routing.rs` and port `tests/check_backend.py` to `cargo test` **in the
   same commit**. The Python checks are the specification; read them first.
3. `dbus.rs` + daemon main. Verify against the live bus with `gdbus call` before
   touching the panel.
4. `keepalive.rs`, then port `tests/check_keepalive.py`.
5. Panel in egui: four tabs, install/remove, per-display dropdown, tone test.
   Port `check_ui.py` and `check_layout.py` as egui state tests, not pixel tests.
6. `install.rs` last — it writes into `~/.local/share` and `~/.config`, so it is
   the only part that can damage a working install. Port `check_roundtrip.py`.
7. Rewrite `packaging/AppRun` for one binary, rebuild the AppImage, compare
   `--version` and `json` output against the Python build.

## Non-negotiables

- No stubs. A function that returns a placeholder while its name promises
  behaviour is a defect, not progress.
- Every port step ships with its check, run, in the same commit.
- Errors get logged with context or propagated. No empty handlers.
- The KWin script is copied, not rewritten.
- Comments explain *why*.
