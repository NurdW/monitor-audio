#!/usr/bin/python3
"""Throwaway checks for what happens while routing is switched off.

These drive the module inside this process rather than the running service:
switching the real service off and on would move the user's own audio around for
no reason, and neither check below needs a live service to mean anything.
"""
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import time
import wave

# Located relative to this file, so a clone runs the copy it came with instead of
# whatever checkout happens to live on the machine.
BACKEND = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                       "backend", "monitor-audio.py")
spec = importlib.util.spec_from_file_location("ma", BACKEND)
ma = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ma)

# A throwaway settings file: these checks switch routing off and on on purpose,
# and the real one belongs to whoever is using the machine.
ma.CONFIG_PATH = os.path.join(tempfile.mkdtemp(prefix="ma-paused-test-"), "config.json")

# A display model no output mentions, so it resolves through the fallback and the
# test does not depend on which monitor the player's window happens to be on.
MODEL = "MAPAUSEDTESTMODEL"
fails = []


def check(name, got, want=True):
    ok = got == want
    print("%-62s %s" % (name, "ok" if ok else "\n      FAIL got=%r\n           want=%r" % (got, want)))
    if not ok:
        fails.append(name)


def write_config(enabled):
    os.makedirs(os.path.dirname(ma.CONFIG_PATH), exist_ok=True)
    with open(ma.CONFIG_PATH, "w") as handle:
        json.dump({"enabled": enabled, "fallback_kind": "analog",
                   "manual_output": {}}, handle)


def where():
    """The output the test player's stream is on right now."""
    name_by_index = {s["index"]: s["name"] for s in ma.sink_list()}
    for s in ma.sink_inputs():
        if pid in s["pids"]:
            return name_by_index.get(s["sink"], "?")
    return "gone"


def move_stream_to(name):
    """Move the test player's stream by hand, the way Volume Control would."""
    iid = next((s["iid"] for s in ma.sink_inputs() if pid in s["pids"]), None)
    return iid is not None and ma.run(["pactl", "move-sink-input", iid, name]) is not None


# Route(pid, output, resource_class, desktop_file, caption, window, model)
def route(window):
    return ma.Service.Route(None, pid, "", "paplay", "paplay",
                            "a paused-state test", window, MODEL)


sinks = ma.sink_list()
target = ma.resolve_output(MODEL, sinks, ma.load_config())[0]
other = next((s["name"] for s in sinks if s["name"] != target), None)
print("=== test setup ===")
print("  %d output(s) present; a window on %r resolves to %r" % (len(sinks), MODEL, target))
check("setup has an output for the fallback", target is not None, True)
check("setup has a second output to be moved off of", other is not None, True)

# The service recognises its own silent stream partly by the file it plays, so
# the test player needs a file of its own; borrowing the service's would make this
# stream look like the service's housekeeping and be left alone.
SILENCE = os.path.join(tempfile.mkdtemp(prefix="ma-paused-silence-"), "quiet.wav")
with wave.open(SILENCE, "w") as handle:
    handle.setnchannels(1)
    handle.setsampwidth(1)
    handle.setframerate(8000)
    handle.writeframes(b"\x80" * (8000 * 60))
player = subprocess.Popen(["paplay", "-n", "ma-paused", SILENCE],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
pid = str(player.pid)
time.sleep(1.5)

try:
    print("\n--- a window that arrives while routing is switched off ---")
    write_config(False)
    ma.wanted.clear()
    ma.placed.clear()
    answer = route("pw1")
    check("nothing is moved while off", answer.startswith("remembered window"), True)
    check("the window is remembered anyway", "pw1" in ma.wanted, True)
    check("with the display it landed on", ma.wanted.get("pw1", {}).get("model"), MODEL)

    print("\n--- while off: leave streams alone, but forget dead placements ---")
    check("the stream was moved off its target for the test", move_stream_to(other), True)
    time.sleep(0.5)
    # PipeWire reuses stream ids, so a placement left over from a stream that has
    # since gone would make a brand-new stream look already placed later on.
    ma.placed["999999"] = "pw1"
    ma.apply_pending()
    check("a re-check while off does not yank the stream back", where(), other)
    check("a placement of a dead stream is forgotten even while off",
          "999999" in ma.placed, False)

    print("\n--- switching back on routes it without another drag ---")
    write_config(True)
    ma.apply_pending()
    deadline = time.time() + 5
    while where() != target and time.time() < deadline:
        time.sleep(0.2)
    check("the remembered window is routed as soon as routing is back on",
          where(), target)
finally:
    player.terminate()
    player.wait(timeout=10)
    ma.wanted.clear()
    ma.placed.clear()

print("\n%s" % ("ALL PASS" if not fails else "FAILURES: %s" % fails))
sys.exit(1 if fails else 0)
