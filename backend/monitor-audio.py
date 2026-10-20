#!/usr/bin/python3
"""Move an application's audio to the output belonging to the monitor its window is on.

Why this program has to exist at all: PipeWire attaches audio streams to a
*process*, never to a window, and KWin (the compositor, which is the only thing
on Wayland that knows where windows are) cannot run programs. So KWin reports
"this window just landed on that monitor" over D-Bus, and this service owns the
PipeWire plumbing that acts on it.

Which output a monitor means is worked out from the display's model name. KWin
reports a monitor's model ("2778X") and PipeWire puts that same model into the
description of the audio output that carries it ("... Digital Stereo (HDMI 3)
[2778X]"). That join survives everything that broke a table of hardcoded output
names: unplugging a display, switching the sound card profile (which renames
...hdmi-stereo to ...hdmi-stereo-extra2) and moving a cable to another port.

Matching a stream to a window needs two sources:
  * `pactl list sink-inputs` gives the stream id we can move, plus a pid for
    apps that connect through the PulseAudio compatibility layer.
  * `pw-dump` gives the pid of the *client* that owns each stream. Apps that use
    PipeWire directly (jellium-desktop plays its wallpaper audio through an
    embedded libmpv, whose stream is simply named "mpv") carry no pid on the
    stream at all, so without this the stream looks like it belongs to nobody.

Two rules keep this from becoming a nuisance, and both come from being wrong before:

  1. A stream is placed once per drag. `placed` remembers which stream ids a drag
     has already positioned. Without that, the periodic re-check fights any other
     party that moves the stream (Volume Control, the app itself, screen sharing)
     and yanks it back every few seconds, forever.
  2. Nothing is ever guessed. If one process owns several routed windows and the
     stream's tab title matches none of them, the stream is left where it is.
     Picking "the most recently moved window" sounds reasonable and silently
     sends audio to the wrong screen.

Debug:  journalctl --user -b -u monitor-audio.service
        python3 monitor-audio.py status
"""

import json
import os
import re
import signal
import subprocess
import sys
import time
import wave

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

BUS_NAME = "local.MonitorAudio"
OBJ_PATH = "/local/MonitorAudio"
IFACE = "local.MonitorAudio"

# Settings live in a json file the control panel writes, so nothing has to be
# edited by hand. Defaults apply when the file is absent:
#   {"enabled": true,
#    "fallback_kind": "analog",
#    "manual_output": {"MAG 275QF X30": "analog"}}
# manual_output maps a display model, exactly as the desktop reports it, to text
# identifying the output to use; use it when the automatic model matching picks
# the wrong output. fallback_kind is what a display with no audio of its own uses.
CONFIG_PATH = os.path.join(GLib.get_user_config_dir(), "monitor-audio", "config.json")

CONFIG_DEFAULTS = {
    "enabled": True,
    "fallback_kind": "analog",
    "keep_awake": True,
    "manual_output": {},
}

# Apps create their audio stream lazily, only once something actually plays, so a
# remembered route has to be re-checked to catch streams that appear later. The
# re-check may only ever place a stream it has not placed before (see rule 1).
RESCAN_SECONDS = 4.0

# A new episode, or a player resuming after a pause, creates a stream that
# PipeWire has already handed to the default output; every second before the next
# look is a second of audio out of the wrong speakers. So when something happens
# that means a stream is about to appear - a caption change, a stream that
# vanished, a stream just placed - look again quickly for a short while instead
# of waiting for the calm interval. Measured on a real desktop, calm polling left
# audio on the wrong output for 0.1-2.4 s (average 1.9 s, ceiling RESCAN_SECONDS).
BURST_SECONDS = 5.0
BURST_INTERVAL_MS = 150

# WirePlumber suspends an output after five idle seconds, and an HDMI output
# takes a moment to come back, which is the mute people hear when resuming after
# being away. Re-asserting "not suspended" does not help - it is re-suspended
# immediately - so the only way to hold an output awake without writing any audio
# configuration is to keep one silent stream on it. That stream is visible in the
# volume mixer, which is why it can be switched off in settings.
KEEPALIVE_NAME = "monitor-audio-keepalive"
KEEPALIVE_SECONDS = 300
keepalive = {}
failed_keepalive = set()

# Name of the silent file the keep-alive streams read. In the runtime directory so
# it disappears on its own and never outlives the session.
SILENCE_PATH = os.path.join(GLib.get_user_runtime_dir() or "/tmp",
                            "monitor-audio-silence.wav")

# Minimum length before a window caption and a stream title are allowed to count
# as the same thing; shorter strings collide far too easily.
MIN_TITLE_LEN = 5

# window id (str) -> {"pid", "model", "appkeys", "captions", "seen", "warned"}
# Keyed by window and not by process: one Firefox can have a window on the TV and
# a window on the desk, and their audio must not follow each other.
wanted = {}

# stream id (str) -> window id that placed it. The point of this map is that we
# place a stream once and then leave it alone until the next real drag.
placed = {}

# Polling is one-shot and re-armed each time, because the wait between passes is
# not constant: fast while a stream is expected, calm otherwise. A repeating
# timeout cannot change its own interval, hence the handle.
timer = {"source": None}
burst_until = 0.0


def log(msg):
    print(time.strftime("%H:%M:%S"), msg, flush=True)


def run(args):
    """Run a helper command, returning its output, or None if it failed."""
    try:
        result = subprocess.run(args, capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.SubprocessError) as exc:
        log("%s failed: %s" % (args[0], exc))
        return None
    if result.returncode != 0:
        log("%s exited %d: %s" % (args[0], result.returncode, result.stderr.strip()[:200]))
        return None
    return result.stdout


def norm(value):
    """Fold a name into something comparable across sources.

    A stream says "Firefox" while the window says "org.mozilla.firefox", so for a
    reverse-DNS id the last part is what has to be compared.
    """
    raw = (value or "").lower()
    parts = re.split(r"[.\-]", raw)
    name = parts[-1] if len(parts) >= 3 else raw
    return re.sub(r"[^a-z0-9]+", "", name) or re.sub(r"[^a-z0-9]+", "", raw)


def sink_list():
    """[{"name", "description", "index"}] for every output device present right now.

    The index is needed because a stream reports the output it is on by index
    while everything else here identifies outputs by name.
    """
    output = run(["pactl", "list", "sinks"])
    if not output:
        return []
    sinks = []
    cur = None
    for line in output.splitlines():
        m = re.match(r"^Sink #(\d+)", line)
        if m:
            cur = {"name": None, "description": "", "index": m.group(1)}
            sinks.append(cur)
            continue
        if cur is None:
            continue
        m = re.match(r"^\tName: (\S+)", line)
        if m:
            cur["name"] = m.group(1)
            continue
        m = re.match(r"^\tDescription: (.+)$", line)
        if m:
            cur["description"] = m.group(1).strip()
    return [s for s in sinks if s["name"]]


def client_pids():
    """{client node id: process id} straight from PipeWire.

    pactl never reports client pids, and natively-connected apps put their pid
    only on the client node, never on the stream. Pids belonging to the PipeWire
    daemon itself are dropped: every PulseAudio-compatibility stream is owned by
    that daemon, so keeping it would make unrelated streams look related.
    """
    raw = run(["pw-dump"])
    if not raw:
        return {}
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as exc:
        log("pw-dump returned unreadable json: %s" % exc)
        return {}

    daemon_pids = set()
    for obj in data:
        props = obj.get("info", {}).get("props", {})
        if props.get("application.name") in ("pipewire", "pipewire-pulse", "wireplumber"):
            pid = props.get("pipewire.sec.pid")
            if isinstance(pid, int):
                daemon_pids.add(str(pid))

    pids = {}
    for obj in data:
        if obj.get("type") != "PipeWire:Interface:Client":
            continue
        pid = obj.get("info", {}).get("props", {}).get("pipewire.sec.pid")
        if isinstance(pid, int) and pid > 0 and str(pid) not in daemon_pids:
            pids[str(obj.get("id"))] = str(pid)
    return pids


def sink_inputs():
    """One dict per playing stream: what to move, where it is, who owns it."""
    output = run(["pactl", "list", "sink-inputs"])
    if not output:
        return []
    streams = []
    cur = None
    for line in output.splitlines():
        if line.startswith("Sink Input #"):
            cur = {
                "iid": line.strip().split("#", 1)[1].split()[0],
                "sink": None, "client": None,
                "pids": set(), "appkeys": set(), "titles": set(),
            }
            streams.append(cur)
            continue
        if cur is None:
            continue
        m = re.match(r"\s*Sink:\s*(\d+)", line)
        if m:
            cur["sink"] = m.group(1)
            continue
        m = re.match(r'\s*([a-zA-Z0-9._-]+)\s*=\s*"([^"]*)"\s*$', line)
        if not m:
            continue
        key, value = m.group(1), m.group(2)
        if key == "application.process.id":
            cur["pids"].add(value)
        elif key == "client.id":
            cur["client"] = value
        elif key in ("application.name", "application.id",
                     "application.process.binary", "node.name"):
            cur["appkeys"].add(norm(value))
        elif key == "media.name":
            cur["titles"].add(norm(value))
    return streams


def load_config():
    """Read the control panel's settings, falling back to defaults.

    Re-read on every pass so a change in the window takes effect immediately and
    a broken file degrades to defaults instead of taking routing down with it.
    """
    try:
        with open(CONFIG_PATH) as handle:
            data = json.load(handle)
    except FileNotFoundError:
        return dict(CONFIG_DEFAULTS)
    except json.JSONDecodeError as exc:
        log("%s is not valid json (%s); using defaults" % (CONFIG_PATH, exc))
        return dict(CONFIG_DEFAULTS)
    except OSError as exc:
        log("could not read %s (%s); using defaults" % (CONFIG_PATH, exc))
        return dict(CONFIG_DEFAULTS)

    if not isinstance(data, dict):
        log("%s should hold a json object; using defaults" % CONFIG_PATH)
        return dict(CONFIG_DEFAULTS)

    config = dict(CONFIG_DEFAULTS)
    for key in CONFIG_DEFAULTS:
        if key in data:
            config[key] = data[key]
    if not isinstance(config["enabled"], bool):
        config["enabled"] = bool(config["enabled"])
    if not isinstance(config["manual_output"], dict):
        log("manual_output in %s should be an object; ignoring it" % CONFIG_PATH)
        config["manual_output"] = {}
    if not isinstance(config["fallback_kind"], str):
        config["fallback_kind"] = CONFIG_DEFAULTS["fallback_kind"]
    return config


def resolve_output(model, sinks, config):
    """Which audio output belongs to the display called `model`.

    Returns (sink name or None, how it was chosen). Re-decided on every pass, so
    unplugging a display or changing the sound card profile needs no restart.
    """
    override = config["manual_output"].get(model)
    if override:
        needle = norm(override)
        hits = [s["name"] for s in sinks
                if needle in norm(s["name"]) or needle in norm(s["description"])]
        if hits:
            return hits[0], "override"
        log("override %r matches no output; outputs are: %s"
            % (override, ", ".join(s["name"] for s in sinks) or "none"))
        return None, "override-miss"

    key = norm(model)
    if key:
        hits = [s["name"] for s in sinks if key in norm(s["description"])]
        if len(hits) == 1:
            return hits[0], "display"
        if len(hits) > 1:
            log("display %r matches several outputs (%s); using %s"
                % (model, ", ".join(hits), hits[0]))
            return hits[0], "display"

    fallback = [s["name"] for s in sinks if config["fallback_kind"] in s["name"]]
    if fallback:
        return fallback[0], "fallback"
    return None, "none"


def owner_pids(stream, pids_of_client):
    """Every process id that can claim this stream."""
    pids = set(stream["pids"])
    owner = pids_of_client.get(stream["client"])
    if owner:
        pids.add(owner)
    return pids


def title_match(stream, route):
    """Does this stream's tab title belong to this window?

    Only meaningful when one process owns several windows: every window of a
    browser shares a pid and an application name, so the tab title is the single
    thing that says which window a stream came from.
    """
    for title in stream["titles"]:
        for caption in route["captions"]:
            if len(title) >= MIN_TITLE_LEN and len(caption) >= MIN_TITLE_LEN \
                    and (title in caption or caption in title):
                return True
    return False


def name_match(stream, route):
    """Fallback for streams that expose no usable pid at all."""
    stream_apps = {k for k in stream["appkeys"] if len(k) >= 3}
    route_apps = {k for k in route["appkeys"] if len(k) >= 3}
    return bool(stream_apps & route_apps)


def stream_belongs(stream, wid, route, pids_of_client):
    """Is this stream this window's audio? Never answers by guessing."""
    owners = owner_pids(stream, pids_of_client)
    if route["pid"] in owners:
        # Same process owns several routed windows: only the tab title may decide.
        rivals = [w for w, r in wanted.items() if w != wid and r["pid"] == route["pid"]]
        return True if not rivals else title_match(stream, route)
    return not owners and name_match(stream, route)


def snapshot():
    """One consistent read of settings, outputs, owners and streams, shared by one pass."""
    sinks = sink_list()
    return {
        "config": load_config(),
        "sinks": sinks,
        "index_by_name": {s["name"]: s["index"] for s in sinks},
        "clients": client_pids(),
        "streams": sink_inputs(),
    }


def is_keepalive(stream):
    """Is this our own silent stream rather than somebody's audio?

    paplay puts the -n name on application.name and node.name, and puts the file
    path in media.name, so both have to be checked; matching only the title would
    never match and our stream would be treated as an app playing nothing.
    """
    return KEEPALIVE_NAME in stream["appkeys"] or KEEPALIVE_NAME in " ".join(stream["titles"])


def apply_window(wid, reason, force, snap=None):
    """Place this window's streams on the output of the display it sits on.

    `force` is set only by a real monitor change, and it is the one thing allowed
    to move a stream that has been placed before. A caption change, and every
    re-check, may only place streams nobody has ever placed.
    """
    route = wanted.get(wid)
    if not route:
        return 0
    snap = snap or snapshot()
    config = snap["config"]
    target, how = resolve_output(route["model"], snap["sinks"], config)
    if not target:
        # Nothing to send audio to. Say so once, not every few seconds.
        if not route.get("warned"):
            route["warned"] = True
            log("display %r (window %s) has no usable output; outputs are: %s"
                % (route["model"], wid,
                   ", ".join(s["name"] for s in snap["sinks"]) or "none"))
        return 0
    route["warned"] = False
    if route.get("output") != target:
        route["output"] = target
        log("display %r -> %s (%s)" % (route["model"], target, how))

    moved = 0
    for stream in snap["streams"]:
        if is_keepalive(stream):
            continue
        if not force and stream["iid"] in placed:
            continue
        if not stream_belongs(stream, wid, route, snap["clients"]):
            continue
        if stream["sink"] == snap["index_by_name"].get(target):
            placed[stream["iid"]] = wid
            continue
        if run(["pactl", "move-sink-input", stream["iid"], target]) is None:
            continue
        placed[stream["iid"]] = wid
        moved += 1
        log("stream %s (window %s) -> %s [%s]" % (stream["iid"], wid, target, reason))
    return moved


def apply_pending():
    """Re-check every route, but only for streams never placed before."""
    if not wanted:
        return 0
    snap = snapshot()
    if not snap["config"]["enabled"]:
        return 0
    live = {s["iid"] for s in snap["streams"]}
    gone = [iid for iid in placed if iid not in live]
    for iid in gone:
        del placed[iid]
    if gone:
        # A player that drops its stream while paused usually makes a new one the
        # instant it resumes; that new stream is what needs catching quickly.
        start_burst("%d placed stream(s) disappeared" % len(gone))
    moved = 0
    for wid in list(wanted):
        moved += apply_window(wid, "new stream", force=False, snap=snap)
    return moved


def describe_streams():
    snap = snapshot()
    lines = []
    for s in snap["streams"]:
        if is_keepalive(s):
            continue
        lines.append("  stream %s sink=%s pids=%s apps=%s titles=%s%s" % (
            s["iid"], s["sink"], sorted(owner_pids(s, snap["clients"])) or "none",
            sorted(s["appkeys"]) or "none", sorted(s["titles"]) or "none",
            "  (placed by %s)" % placed[s["iid"]] if s["iid"] in placed else ""))
    return lines


def prune_dead():
    """Forget windows whose process is gone; KWin reports closes, this is the net."""
    for wid in list(wanted):
        pid = wanted[wid]["pid"]
        try:
            os.kill(int(pid), 0)
        except (ValueError, OSError) as exc:
            log("forgetting window %s (pid %s is not running: %s)" % (wid, pid, exc))
            del wanted[wid]


def report(service_running):
    """Everything the control panel shows, as plain data.

    Routes and placements live in the running service, so this is only complete
    when called inside it; the command line uses it for a partial answer when the
    service is not reachable.
    """
    snap = snapshot()
    config = snap["config"]
    name_by_index = {index: name for name, index in snap["index_by_name"].items()}

    routes = []
    for wid, route in sorted(wanted.items()):
        target, how = resolve_output(route["model"], snap["sinks"], config)
        routes.append({
            "window": wid,
            "pid": route["pid"],
            "model": route["model"],
            "output": target,
            "how": how,
            "apps": sorted(route["appkeys"]),
            "captions": sorted(route["captions"]),
        })

    streams = []
    for stream in snap["streams"]:
        if is_keepalive(stream):
            # Our own housekeeping stream; showing it would look like an app
            # playing nothing.
            continue
        streams.append({
            "id": stream["iid"],
            "output": name_by_index.get(stream["sink"]),
            "apps": sorted(stream["appkeys"]),
            "titles": sorted(stream["titles"]),
            "pids": sorted(owner_pids(stream, snap["clients"])),
            "placed_by": placed.get(stream["iid"]),
        })

    return {
        "service_running": service_running,
        "config": config,
        "config_path": CONFIG_PATH,
        "outputs": [{"name": s["name"], "description": s["description"],
                     "index": s["index"]} for s in snap["sinks"]],
        "routes": routes,
        "streams": streams,
    }


def start_burst(reason):
    """Look again quickly for a while, because a stream is probably about to appear."""
    global burst_until
    already = time.time() < burst_until
    burst_until = time.time() + BURST_SECONDS
    if not already:
        log("looking for new streams for %.0f s (%s)" % (BURST_SECONDS, reason))
    # Only re-arm when a slow pass is already queued; during a pass the caller's
    # own scheduling picks up the new interval, and arming twice would double-tick.
    if timer["source"] is not None:
        schedule()


def schedule():
    if timer["source"] is not None:
        GLib.source_remove(timer["source"])
    delay = (BURST_INTERVAL_MS if time.time() < burst_until
             else RESCAN_SECONDS * 1000)
    timer["source"] = GLib.timeout_add(int(delay), tick)


def tick():
    global burst_until
    timer["source"] = None
    prune_dead()
    if apply_pending():
        # A player that opens one stream often opens another a moment later, so
        # stay quick rather than going straight back to the calm interval.
        burst_until = max(burst_until, time.time() + BURST_SECONDS)
    keepalive_tick()
    schedule()
    return False


def ensure_silence():
    """Write the silent file the keep-alive streams read. True when it is usable."""
    if os.path.exists(SILENCE_PATH):
        return True
    try:
        with wave.open(SILENCE_PATH, "w") as handle:
            handle.setnchannels(1)
            handle.setsampwidth(1)
            handle.setframerate(8000)
            # Unsigned 8-bit silence is 0x80, not zero.
            handle.writeframes(b"\x80" * (8000 * KEEPALIVE_SECONDS))
        return True
    except OSError as exc:
        log("could not write %s to keep outputs awake: %s" % (SILENCE_PATH, exc))
        return False


def stop_process(process):
    """End a silent stream, forcefully if it will not be asked politely."""
    process.terminate()
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        process.kill()


def stop_keepalive(reason=""):
    if keepalive and reason:
        log("stopped holding %d output(s) awake (%s)" % (len(keepalive), reason))
    for process in keepalive.values():
        stop_process(process)
    keepalive.clear()


def keepalive_tick():
    """Hold one silent stream on every output a route points at that is otherwise idle.

    Without this the output suspends after five idle seconds and the first
    seconds of whatever follows are lost while it wakes up. The stream is visible
    in the volume mixer, so settings can switch the whole thing off.
    """
    if not wanted:
        stop_keepalive()
        return
    snap = snapshot()
    config = snap["config"]
    if not config["enabled"] or not config["keep_awake"]:
        stop_keepalive("switched off")
        return
    if not ensure_silence():
        return

    # Hold one silent stream on every output a route points at, whether or not
    # something is playing there. Deciding "is anything playing?" cannot be done
    # from pactl, which leaves media.name off Pulse clients - our own stream would
    # look like a stranger's, and stopping and restarting it every pass was worse
    # than leaving it up. It is silent, and the panel does not list it.
    targets = set()
    for route in wanted.values():
        target = resolve_output(route["model"], snap["sinks"], config)[0]
        if target:
            targets.add(target)
    # An output whose stream would not start stays out of the way; retrying every
    # pass would fill the log with the same failure.
    targets -= failed_keepalive

    for name in [n for n in keepalive if n not in targets]:
        stop_process(keepalive.pop(name))

    for name in targets:
        process = keepalive.get(name)
        if process is not None and process.poll() is None:
            continue
        if process is not None:
            log("silent stream on %s ended early; starting another" % name)
        try:
            keepalive[name] = subprocess.Popen(
                ["paplay", "-n", KEEPALIVE_NAME, "--device=%s" % name, SILENCE_PATH],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except OSError as exc:
            # Most likely paplay is missing on this machine; holding outputs awake
            # is a nicety, so say so once per output and carry on routing.
            log("could not start the silent stream to hold %s awake: %s" % (name, exc))
            keepalive.pop(name, None)
            failed_keepalive.add(name)
            continue
        log("holding %s awake with a silent stream" % name)


class Service(dbus.service.Object):
    def __init__(self, loop):
        bus = dbus.SessionBus()
        # Keep this object referenced on purpose: dbus-python releases the bus
        # name when the BusName instance is garbage-collected, so a local
        # temporary would unregister us a moment after startup.
        self.bus_name = dbus.service.BusName(BUS_NAME, bus=bus, do_not_queue=True)
        super().__init__(bus, OBJ_PATH)
        schedule()

    @dbus.service.method(IFACE, in_signature="sssssss", out_signature="s")
    def Route(self, pid, output, resource_class, desktop_file, caption, window, model):
        pid, window = str(pid), (str(window) or str(pid))
        if not pid.isdigit() or int(pid) <= 0:
            # KWin reports -1 when it cannot resolve a window's owner. Remembering
            # such a route would let a phantom window claim streams by name.
            log("Route ignored for window %s: unusable pid %r" % (window, pid))
            return "ignoring window %s with unusable pid" % window

        appkeys = {norm(str(resource_class)), norm(os.path.basename(str(desktop_file)))}
        if not load_config()["enabled"]:
            return "paused: monitor-audio routing is switched off"
        try:
            with open("/proc/%s/comm" % pid) as handle:
                appkeys.add(norm(handle.read()))
        except OSError as exc:
            log("could not read the process name of pid %s (window %s): %s"
                % (pid, window, exc))
        appkeys.discard("")
        captions = {norm(str(caption))} - {""}
        model = str(model).strip()

        old = wanted.get(window)
        moved_monitor = bool(old) and old["model"] != model
        if old and old["model"] == model and old["captions"] == captions:
            return "unchanged: window %s stays on %s" % (window, model or "unknown display")

        wanted[window] = {
            "pid": pid,
            "model": model,
            "output": old.get("output") if old else None,
            "appkeys": appkeys,
            "captions": captions,
            "seen": time.time(),
            "warned": old.get("warned", False) if old else False,
        }
        if not old:
            log("Route: window %s pid %s display %s class=%s caption=%s"
                % (window, pid, model, resource_class, str(caption)[:70]))
        elif moved_monitor:
            log("Route: window %s moved to display %s caption=%s"
                % (window, model, str(caption)[:70]))

        # A window arriving, or crossing onto another display, is followed shortly
        # by that app creating a stream. A caption change alone is not worth it:
        # browsers rewrite titles constantly and polling fast after each one would
        # keep a core busy while browsing. A stream disappearing is the other
        # trigger, and that is covered in apply_pending.
        if not old or moved_monitor:
            start_burst("window %s on %s" % (window, model or "unknown display"))

        # Only a display change is allowed to move audio that is already placed.
        moved = apply_window(window, "window moved", force=moved_monitor)
        if moved == 0 and moved_monitor:
            detail = "\n".join(describe_streams()) or "  (nothing is playing)"
            return ("window %s -> display %s, but no stream was moved; streams now:\n%s"
                    % (window, model, detail))
        return "window %s on %s (%d stream(s) moved)" % (window, model, moved)

    @dbus.service.method(IFACE, in_signature="", out_signature="s")
    def JsonStatus(self):
        return json.dumps(report(True))

    @dbus.service.method(IFACE, in_signature="", out_signature="s")
    def Reset(self):
        """Forget every route and placement, e.g. after rewiring the speakers."""
        count = len(wanted)
        wanted.clear()
        placed.clear()
        log("reset: forgot %d route(s)" % count)
        return "forgot %d route(s)" % count

    @dbus.service.method(IFACE, in_signature="", out_signature="s")
    def Status(self):
        snap = snapshot()
        config = snap["config"]
        lines = ["settings (%s):" % CONFIG_PATH,
                 "  enabled=%s fallback_kind=%r manual_output=%s"
                 % (config["enabled"], config["fallback_kind"],
                    config["manual_output"] or "{}")]
        lines.append("outputs present:")
        for s in snap["sinks"]:
            lines.append("  %s  (%s)" % (s["name"], s["description"]))
        if not wanted:
            lines.append("no routes remembered yet (drag a window to another monitor)")
        for wid, route in sorted(wanted.items()):
            target, how = resolve_output(route["model"], snap["sinks"], config)
            lines.append("window %s (pid %s) display %r -> %s [%s] (names: %s; captions: %s)" % (
                wid, route["pid"], route["model"], target, how,
                ", ".join(sorted(route["appkeys"])) or "?",
                " | ".join(sorted(route["captions"])) or "?"))
        lines.append("placed streams: %d" % len(placed))
        lines.append("live streams:")
        lines.extend(describe_streams() or ["  (nothing is playing)"])
        return "\n".join(lines)

    @dbus.service.method(IFACE, in_signature="s", out_signature="s")
    def Forget(self, window):
        """Drop one window's route, or every route of a process when given a pid."""
        key = str(window)
        if wanted.pop(key, None):
            dropped = [key]
        else:
            dropped = [w for w, r in wanted.items() if r["pid"] == key]
            for w in dropped:
                del wanted[w]
        for iid in [i for i, w in placed.items() if w in dropped]:
            del placed[iid]
        if dropped:
            log("forgot %d route(s) for %s" % (len(dropped), key))
            return "forgot %d route(s) for %s" % (len(dropped), key)
        return "nothing remembered for %s" % key


def cli_status():
    try:
        obj = dbus.SessionBus().get_object(BUS_NAME, OBJ_PATH)
        print(obj.Status(dbus_interface=IFACE))
    except dbus.DBusException as exc:
        print("monitor-audio service is not reachable: %s" % exc)
        return 1
    return 0


def cli_json():
    """Print the report as json; the control panel reads this."""
    try:
        obj = dbus.SessionBus().get_object(BUS_NAME, OBJ_PATH)
        print(obj.JsonStatus(dbus_interface=IFACE))
    except dbus.DBusException as exc:
        log("service not reachable (%s); reporting what is visible without it" % exc)
        print(json.dumps(report(False)))
    return 0


def cli_reset():
    try:
        obj = dbus.SessionBus().get_object(BUS_NAME, OBJ_PATH)
        print(obj.Reset(dbus_interface=IFACE))
    except dbus.DBusException as exc:
        print("monitor-audio service is not reachable: %s" % exc)
        return 1
    return 0


def shutdown(loop):
    """Leave no silent stream behind; an orphaned one would play silence forever."""
    stop_keepalive("service stopping")
    loop.quit()
    return False


def cli_resolve(models_json):
    """Answer "which output would each of these displays use?"

    The control panel asks instead of working it out itself. The rules for pairing
    a display with an output live in resolve_output(); a second copy in the panel
    is what let its table say "none" about a display whose audio was playing
    happily out of the fallback output, and ignore overrides entirely.
    """
    try:
        models = json.loads(models_json)
    except json.JSONDecodeError as exc:
        print("could not read the list of displays (%s): %s" % (models_json[:80], exc))
        return 1
    if not isinstance(models, list):
        print("expected a json list of display names, got %s" % type(models).__name__)
        return 1

    snap = snapshot()
    descriptions = {s["name"]: s["description"] for s in snap["sinks"]}
    answer = {}
    for model in models:
        target, how = resolve_output(str(model), snap["sinks"], snap["config"])
        answer[str(model)] = {
            "output": target,
            "description": descriptions.get(target) if target else None,
            "how": how,
        }
    print(json.dumps(answer))
    return 0


def main():
    command = sys.argv[1] if len(sys.argv) > 1 else ""
    if command == "status":
        sys.exit(cli_status())
    if command == "json":
        sys.exit(cli_json())
    if command == "reset":
        sys.exit(cli_reset())
    if command == "resolve":
        sys.exit(cli_resolve(sys.argv[2] if len(sys.argv) > 2 else "[]"))

    DBusGMainLoop(set_as_default=True)
    loop = GLib.MainLoop()
    Service(loop)
    # systemd stops this with SIGTERM; without a handler the silent keep-alive
    # streams would be orphaned and go on holding an output awake with nobody
    # owning them. Quitting through the loop keeps the shutdown order sane.
    signal.signal(signal.SIGTERM, lambda signum, frame: GLib.idle_add(shutdown, loop))
    log("listening on %s" % BUS_NAME)
    try:
        loop.run()
    except KeyboardInterrupt:
        log("shutting down on interrupt")
    stop_keepalive("interrupted")


if __name__ == "__main__":
    main()
