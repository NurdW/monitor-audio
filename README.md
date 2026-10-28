# Monitor Audio

Drag a window onto another monitor and its audio follows it. Built for KDE
Plasma on Wayland, where PipeWire decides an application's output when the stream
starts and never revisits that choice.

## What you need

- KDE Plasma 6 on Wayland
- PipeWire (the default on Fedora, Arch, openSUSE, Ubuntu 22.04+)

## Run it

Download `monitor-audio-rust-<version>-x86_64.AppImage` from
[Releases](../../releases), then in a terminal:

```bash
chmod +x ~/Downloads/monitor-audio-rust-1.0.1-x86_64.AppImage
~/Downloads/monitor-audio-rust-1.0.1-x86_64.AppImage
```

The window offers **Install**, which puts three things in your home directory and
tells you what it is about to do first: the service, a KWin script, and a systemd
user unit. Nothing needs root. **Remove** takes all three back out and leaves your
display choices alone.

## How it works

KWin tells the service which monitor each window sits on, along with that window's
title. The service pairs a display with an audio output by matching the monitor's
model name against PipeWire's output descriptions, then moves that window's
streams with `pactl move-sink-input`.

Two rules matter, and they are the reason the code looks the way it does:

- **A stream is placed once and then left alone.** Only a real move to another
  monitor may move it again, or an application that opens a stream while you are
  mid-drag would fight you for it.
- **Ownership is never guessed.** A browser is one process for several windows, so
  the only thing that says which window a stream belongs to is the tab title. If
  the title matches nothing, the stream is left where it is rather than moved on a
  hunch. That is what lets one Firefox tab play on the TV while another keeps
  working on your main monitor.

## Choosing which speaker each monitor uses

The Monitors tab has one dropdown per display, and what it shows is what is
actually in use: `Automatic - <output>` while the service is deciding, the plain
output name once you have chosen. Hover it for the reason. Settings live in
`~/.config/monitor-audio/config.json` and overrides are stored under the display's
model name, so they survive replugging cables and changing the sound profile.

## The silent stream, and why it is there

WirePlumber suspends an idle output after about five seconds, and an HDMI output
takes a moment to wake, which is the mute people hear when a video resumes after a
pause. The service holds one silent stream on every output you route to so it never
sleeps. That stream is visible in KDE's Volume Control as `monitor-audio-keepalive`
— it is the only way to do this without writing PipeWire configuration. **Keep
routed outputs awake** switches it off if you would rather not see it.

## Building from source

```bash
cd rust
cargo build --release          # service and panel
../check.sh                    # tests and clippy
../rust/build-appimage.sh      # -> dist/monitor-audio-rust-<version>-x86_64.AppImage
```

Needs Rust (`rustup` minimal profile is enough) and, for the panel, whatever
`eframe` pulls in. The AppImage build also needs `appimagetool` in `tools/`; the
build script prints the download command if it is missing.

### Layout

| path | what it is |
| --- | --- |
| `rust/crates/monitor-audio-core` | the decisions: settings, PipeWire parsing, output and ownership matching. No D-Bus, no GUI, so it is testable alone |
| `rust/crates/monitor-audio-daemon` | the service: D-Bus name `local.MonitorAudio`, the burst timer, the keep-alive |
| `rust/crates/monitor-audio-panel` | the window (egui), and the install/remove logic |
| `kwin-script/` | the KWin integration. JavaScript, because KWin only loads QML and JavaScript |

The panel finds the service next to itself, which is what makes the two belong to
one file rather than depending on what the machine already has.

## The Python version

The original implementation was Python and PySide6. It is kept on the
`python-legacy` branch and at tags `v1.0.0` and `v1.0.1`. It is not built or
maintained here any more; the Rust build needs no Python on the target machine and
that was the point of the rewrite.

## License

MIT — see `LICENSE`.
