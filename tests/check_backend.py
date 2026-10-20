#!/usr/bin/python3
"""Throwaway checks for monitor-audio: settings, output resolution, matching, no-fight."""
import importlib.util
import json
import os
import subprocess
import time

spec = importlib.util.spec_from_file_location(
    "ma", "/home/Nurd/monitor-audio/backend/monitor-audio.py")
ma = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ma)

# Settings live in a throwaway file: these checks write and corrupt it on purpose.
import tempfile
ma.CONFIG_PATH = os.path.join(tempfile.mkdtemp(prefix="ma-backend-test-"), "config.json")

fails = []


def wait_for(predicate, seconds=6.0):
    """Poll until the predicate holds; a fixed sleep here flakes on a busy machine."""
    deadline = time.time() + seconds
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(0.2)
    return predicate()


def check(name, got, want):
    ok = got == want
    print("%-62s %s" % (name, "ok" if ok else "\n      FAIL got=%r\n           want=%r" % (got, want)))
    if not ok:
        fails.append(name)


print("--- load_config(): defaults, real file, and a broken file ---")
CFG = ma.load_config()
check("defaults when the file is absent",
      (CFG["enabled"], CFG["fallback_kind"], CFG["manual_output"]),
      (True, "analog", {}))

os.makedirs(os.path.dirname(ma.CONFIG_PATH), exist_ok=True)
existed = os.path.exists(ma.CONFIG_PATH)
backup = open(ma.CONFIG_PATH).read() if existed else None
try:
    with open(ma.CONFIG_PATH, "w") as handle:
        json.dump({"enabled": False, "fallback_kind": "hdmi",
                   "manual_output": {"MAG 275QF X30": "analog"}}, handle)
    cfg = ma.load_config()
    check("reads settings the control panel wrote",
          (cfg["enabled"], cfg["fallback_kind"], cfg["manual_output"]),
          (False, "hdmi", {"MAG 275QF X30": "analog"}))

    with open(ma.CONFIG_PATH, "w") as handle:
        handle.write("{ this is not json")
    check("broken json falls back to defaults instead of dying",
          ma.load_config()["enabled"], True)

    with open(ma.CONFIG_PATH, "w") as handle:
        json.dump({"manual_output": "not an object"}, handle)
    check("wrong type for manual_output is ignored",
          ma.load_config()["manual_output"], {})

    with open(ma.CONFIG_PATH, "w") as handle:
        json.dump([1, 2, 3], handle)
    check("a json list instead of an object is ignored",
          ma.load_config()["enabled"], True)
finally:
    if existed:
        with open(ma.CONFIG_PATH, "w") as handle:
            handle.write(backup)
    else:
        os.remove(ma.CONFIG_PATH)

CFG = ma.load_config()
sinks = ma.sink_list()
print("\n=== outputs present ===")
for s in sinks:
    print("  %s\n     %s" % (s["name"], s["description"]))
digital = next((s["name"] for s in sinks if "hdmi" in s["name"]), None)
analog = next((s["name"] for s in sinks if "analog" in s["name"]), None)

print("\n--- resolve_output(): the display model decides ---")
check("a display whose model PipeWire advertises -> that output",
      ma.resolve_output("SAMSUNG", sinks, CFG)[1], "display")
check("a display with no audio -> fallback",
      ma.resolve_output("MAG 275QF X30", sinks, CFG)[1], "fallback")
check("unknown display -> fallback, not a crash",
      ma.resolve_output("Totally Unknown 99", sinks, CFG)[1], "fallback")
check("no outputs at all -> None, not a crash",
      ma.resolve_output("SAMSUNG", [], CFG)[0], None)
check("manual override wins over automatic",
      ma.resolve_output("SAMSUNG", sinks, {"manual_output": {"SAMSUNG": "analog"},
                                           "fallback_kind": "analog", "enabled": True})[1],
      "override")
check("override naming a missing output is reported, not guessed",
      ma.resolve_output("SAMSUNG", sinks, {"manual_output": {"SAMSUNG": "nope-9999"},
                                           "fallback_kind": "analog", "enabled": True})[1],
      "override-miss")

print("\n--- stream_belongs(): never guess which window a stream came from ---")
ma.wanted.clear()
stream = {"iid": "1", "sink": None, "client": None,
          "pids": {"500"}, "appkeys": {"firefox"}, "titles": {"somewebsiteyoutube"}}
ma.wanted["winA"] = {"pid": "500", "model": "x", "appkeys": set(),
                     "captions": {"somewebsiteyoutube"}, "seen": 1.0}
check("single window of a process claims its stream",
      ma.stream_belongs(stream, "winA", ma.wanted["winA"], {}), True)
ma.wanted["winB"] = {"pid": "500", "model": "x", "appkeys": set(),
                     "captions": {"someothertab"}, "seen": 2.0}
check("two windows, title matches the other one -> refuse",
      ma.stream_belongs(stream, "winB", ma.wanted["winB"], {}), False)
ma.wanted["winC"] = {"pid": "500", "model": "x", "appkeys": set(),
                     "captions": {"unrelatedtitle"}, "seen": 99.0}
del ma.wanted["winA"]
check("title matches nobody -> refuse",
      ma.stream_belongs(stream, "winC", ma.wanted["winC"], {}), False)
ma.wanted.clear()
orphan = {"iid": "2", "sink": None, "client": None,
          "pids": set(), "appkeys": {"vesktop"}, "titles": set()}
ma.wanted["winD"] = {"pid": "777", "model": "x", "appkeys": {"vesktop"},
                     "captions": set(), "seen": 1.0}
check("pid-less stream claimed by matching app name",
      ma.stream_belongs(orphan, "winD", ma.wanted["winD"], {}), True)

print("\n--- live: place once, never fight ---")
ma.wanted.clear()
ma.placed.clear()
subprocess.run(["python3", "-c",
                "import wave;w=wave.open('/tmp/ma-silence.wav','w');w.setnchannels(2);"
                "w.setsampwidth(2);w.setframerate(48000);"
                "w.writeframes(b'\\x00\\x00'*48000*2*60);w.close()"])
player = subprocess.Popen(["paplay", "-n", "ma-regress", "/tmp/ma-silence.wav"],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1.5)
pid = str(player.pid)


def where():
    live_sinks = {s["index"]: s["name"] for s in ma.sink_list()}
    for s in ma.sink_inputs():
        if pid in s["pids"]:
            return live_sinks.get(s["sink"], "?")
    return "gone"


def move_to(name):
    iid = next((s["iid"] for s in ma.sink_inputs() if pid in s["pids"]), None)
    subprocess.run(["pactl", "move-sink-input", iid, name])
    time.sleep(0.6)
    return iid


model_with_audio = next((m for m in ("SAMSUNG", "2778X")
                         if ma.resolve_output(m, sinks, CFG)[1] == "display"), None)
model_without = "MAG 275QF X30"
check("test setup found a display that has audio", model_with_audio is not None, True)

ma.wanted["w1"] = {"pid": pid, "model": model_with_audio, "appkeys": {"paplay"},
                   "captions": set(), "seen": 1.0, "warned": False}
ma.apply_window("w1", "test", force=True)
time.sleep(0.6)
check("window on %s -> its own output" % model_with_audio, where(), digital or analog)

iid = move_to(analog if where() != analog else digital)
before = where()
check("external move took effect", before in (analog, digital), True)
for _ in range(3):
    ma.apply_pending()
time.sleep(0.3)
check("re-check does not yank it back (the Discord fight)", where(), before)

ma.wanted["w1"]["captions"] = {"a brand new title"}
ma.apply_window("w1", "caption", force=False)
time.sleep(0.3)
check("caption change does not yank it back", where(), before)

ma.wanted["w1"]["model"] = model_without
ma.apply_window("w1", "window moved", force=True)
check("moving the window to another display moves the audio",
      wait_for(lambda: where() == analog), True)

ma.wanted["w1"]["model"] = model_with_audio
move_to(analog)
ma.apply_window("w1", "window moved", force=True)
check("a drag re-claims a stream someone moved by hand",
      wait_for(lambda: where() == (digital or analog)), True)

player.terminate()
player.wait(timeout=10)
ma.wanted.clear()
ma.placed.clear()
print("\n%s" % ("ALL PASS" if not fails else "FAILURES: %s" % fails))
