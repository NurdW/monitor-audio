# Monitor Audio

Move an application's sound to the speakers of the monitor its window is on.

Drag your browser to the TV and its sound goes to the TV. Drag it back to your
main screen and the sound comes back to your desk speakers. Games, Discord and
music players follow the same rule.

## Get it

Prebuilt file for x86-64: download `monitor-audio-1.0.0-x86_64.AppImage` from
[Releases](../../releases), then in a terminal:

```bash
chmod +x ~/Downloads/monitor-audio-1.0.0-x86_64.AppImage
~/Downloads/monitor-audio-1.0.0-x86_64.AppImage
```

Needs KDE Plasma on Wayland, and `python3-pyside6` (Arch `python-pyside6`) for
the control panel. The AppImage writes files into your home directory only, and
tells you what it is about to do before it does it.

## What you need

| Requirement | Why |
|---|---|
| KDE Plasma 6 on Wayland | the window-to-monitor detection is a KWin script |
| PipeWire | the audio side (`pactl` and `pw-dump`) |
| `python3` with PySide6 | the control panel window |

Every KDE Plasma 6 desktop already has the first two. If PySide6 is missing the
AppImage tells you which one package to install instead of crashing.

## Using the AppImage

```bash
chmod +x monitor-audio-1.0.0-x86_64.AppImage
./monitor-audio-1.0.0-x86_64.AppImage
```

Or just double-click it. On a few distributions (recent Ubuntu, Fedora with
`fuse3` restrictions) double-clicking fails because the AppImage cannot mount
itself; run it with `--appimage-extract-and-run` in that case.

Press **Install** once. That puts three things in place:

- `~/.local/share/monitor-audio/monitor-audio.py` - the background service
- `~/.local/share/kwin/scripts/monitor-audio/` - the KWin script that notices
  windows moving between monitors
- `~/.config/systemd/user/monitor-audio.service` - so the service starts on login

Then drag a window to another monitor. **Remove** undoes all three. Your display
choices in `~/.config/monitor-audio/config.json` are kept either way.

## Choosing which speaker each monitor uses

The Monitors tab has one dropdown per display, and what it shows is what is
actually in use. It reads `Automatic - <output>` while the service is deciding,
and the plain output name once you have chosen one yourself. Hover it for the
reason: an output that names this display, a display with no audio of its own
using the fallback, or your own choice.

Usually nothing is needed: a monitor whose model name PipeWire advertises (an
HDMI or DisplayPort audio output carrying sound to that screen) is picked
automatically, and a monitor with no speakers of its own falls back to the
analogue output.

When that guess is wrong - two screens showing the same model, a monitor that
reports a name PipeWire never uses - pick the output from that dropdown. The
choice is stored under the display's model name, so it survives replugging
cables and changing the sound profile.

The panel asks the running service what each display resolves to rather than
working it out itself, so this table cannot describe a decision the routing is
not making.

## Two things worth knowing

**Audio belongs to applications, not windows.** PipeWire routes per application
stream. Two windows of the same program on two monitors still share one stream,
unless the program puts something distinguishing in the window title - which
browsers do, so separate Firefox tabs can be told apart.

**It never fights you.** Once a stream has been placed the service leaves it
alone. If you move something by hand in Volume Control, or a screen-sharing app
grabs it, that stays put. Only dragging the window to a different monitor takes
control back.

## Why a new episode used to start from the wrong speakers

A player that starts a new episode, or resumes after a pause, makes a brand-new
audio stream, and PipeWire hands new streams the *default* output. The service
corrects that afterwards, so the gap between the stream appearing and the
correction was audible - measured at up to 2.4 seconds, average 1.9, because the
service only looked every four seconds.

It now looks again every 150 ms for a few seconds whenever something suggests a
stream is coming: a window arriving on a display, or a stream it had placed
disappearing (which is what pausing does). Measured on the same machine after
the change: 0.00 seconds in five out of five trials. Title changes alone do not
trigger this, because browsers rewrite titles constantly and polling fast after
each one would keep a core busy for no reason.

## Why resuming after being away used to go quiet

An output that is not being used goes to sleep after five seconds, and an HDMI
output takes a moment to come back. Re-asking it to stay awake does nothing - it
is re-suspended immediately - so the service holds one **silent stream** on every
output you are routing to, which is the only thing that works without writing any
audio configuration.

The cost is honest: that silent stream shows up in your volume mixer as
`monitor-audio-keepalive` while a window is routed to that output. Untick
**Keep routed outputs awake** to stop holding them, and accept the wake-up delay.
It is also off entirely while routing is switched off, and it never appears in
this program's own window.

## Settings

`~/.config/monitor-audio/config.json`, written by the control panel:

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | routing on or off without uninstalling |
| `keep_awake` | `true` | hold outputs awake with a silent stream |
| `manual_output` | `{}` | display model -> output, when the automatic guess is wrong |
| `fallback_kind` | `"analog"` | what a display with no speakers of its own uses |

## Building it yourself

```bash
./build-appimage.sh          # -> dist/monitor-audio-<version>-<arch>.AppImage
```

Nothing needs installing first: the script downloads `appimagetool`, which
carries its own `mksquashfs`. Add `--assemble-only` to stop before packing and
inspect or run the unpacked directory.

## Layout

```
backend/monitor-audio.py       the service; owns the D-Bus name local.MonitorAudio
kwin-script/monitor-audio/     KWin script; tells the service where windows went
ui/monitor_audio_ui.py         the control panel
packaging/                     AppRun, .desktop, icon, systemd unit
tests/                         checks that need a live KDE + PipeWire session
VERSION                        stamped into the output file name
```

## Command line

```bash
python3 ~/.local/share/monitor-audio/monitor-audio.py status   # what it decided
python3 ~/.local/share/monitor-audio/monitor-audio.py json     # same, as data
python3 ~/.local/share/monitor-audio/monitor-audio.py reset    # forget placements
journalctl --user -u monitor-audio.service                     # its log
```

## Removing every trace by hand

```bash
systemctl --user disable --now monitor-audio.service
rm ~/.config/systemd/user/monitor-audio.service
rm ~/.local/share/monitor-audio/monitor-audio.py
rm -r ~/.local/share/kwin/scripts/monitor-audio
kwriteconfig6 --file kwinrc --group Plugins --key monitor-audioEnabled --delete
```
