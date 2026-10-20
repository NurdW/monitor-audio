#!/usr/bin/env python3
"""Throwaway check: an output a route points at is held awake, and released again.

Uses a throwaway dummy output rather than the TV, so it runs whatever the TV is
doing, and unloads that dummy again whatever happens below.
"""
import json
import os
import subprocess
import sys
import time

SERVICE = "monitor-audio.service"
KEEPALIVE_NAME = "monitor-audio-keepalive"
DUMMY = "matest"
MODEL = "MATESTMODEL"

fails = []


def sh(args):
    return subprocess.run(args, capture_output=True, text=True)


def dbus(method, *args):
    return sh(["gdbus", "call", "--session", "--dest", "local.MonitorAudio",
               "--object-path", "/local/MonitorAudio",
               "--method", "local.MonitorAudio." + method] + list(args)).stdout


def sinks():
    return {s["name"]: s["state"]
            for s in json.loads(sh(["pactl", "-f", "json", "list", "sinks"]).stdout or "[]")}


def keepalive_streams():
    """Every silent stream the service is holding, wherever it is holding it.

    Not scoped to this test's dummy: node.target is not a property on the stream,
    so it cannot be attributed that way. The service may be holding the user's own
    outputs awake at the same time, which is why the checks below ask for "at
    least one" and rely on the dummy going back to sleep as the proof of release.
    pw-dump because pactl leaves media.name off sink-inputs for pulse clients.
    """
    nodes = json.loads(sh(["pw-dump"]).stdout or "[]")
    found = []
    for node in nodes:
        props = (node.get("info") or {}).get("props") or {}
        # A playback stream is Stream/Output/Audio: output from the app's side.
        # paplay puts -n on application.name/node.name and the file path in
        # media.name, so look for it where it actually lands.
        if props.get("media.class") == "Stream/Output/Audio" and \
           props.get("application.name") == KEEPALIVE_NAME:
            found.append(node.get("info", {}).get("id"))
    return found


def check(name, got, want=True):
    ok = got == want
    print("%-56s %s" % (name, "ok" if ok else "FAIL got=%r want=%r" % (got, want)))
    if not ok:
        fails.append(name)


try:
    sh(["pactl", "load-module", "module-null-sink", "sink_name=%s" % DUMMY,
        "sink_properties=device.description=%s" % MODEL])
    time.sleep(1)
    check("dummy output exists", DUMMY in sinks(), True)

    print("\n--- an idle output sleeps on its own ---")
    for _ in range(12):
        time.sleep(1)
        if sinks().get(DUMMY) == "SUSPENDED":
            break
    check("idle output suspended itself", sinks().get(DUMMY), "SUSPENDED")

    print("\n--- routing a window to it holds it awake ---")
    keeper = subprocess.Popen(["sleep", "600"])
    try:
        # Route(pid, output, resource_class, desktop_file, caption, window, model)
        dbus("Route", str(keeper.pid), "", "jellium", "jellium", "Jellium Desktop", "MA-1", MODEL)
        awake = False
        for _ in range(15):
            time.sleep(1)
            if sinks().get(DUMMY) != "SUSPENDED":
                awake = True
                print("     woke after %d s" % (_ + 1))
                break
        check("output a route points at stays awake", awake, True)
        time.sleep(4)
        # An awake output with nothing actually rendering reports IDLE; the thing
        # being prevented is SUSPENDED, which is what costs the first moments.
        check("and stays awake a few seconds later",
              sinks().get(DUMMY) != "SUSPENDED", True)
        check("a silent stream appeared for this route",
              len(keepalive_streams()) >= 1, True)

        # Read it the way the panel does: the command line prints plain json.
        raw = sh([sys.executable, os.path.expanduser(
            "~/.local/share/monitor-audio/monitor-audio.py"), "json"]).stdout
        report = json.loads(raw[raw.find("{"):])
        shown = [s for s in report["streams"]
                 if KEEPALIVE_NAME in " ".join(s["apps"] + s["titles"])]
        check("the panel is not shown our own silent stream", shown, [])
        check("the routed output still appears as an output",
              any(DUMMY in o["name"] for o in report["outputs"]), True)

        print("\n--- forgetting the route releases it ---")
        dbus("Forget", "MA-1")
        time.sleep(6)
        # No count assertion here on purpose: the service may be holding the
        # user's own outputs awake at the same time and a stream cannot be
        # attributed to a sink from pw-dump properties, so a global count would
        # pass or fail depending on what else is open. The dummy going back to
        # sleep below is the proof that this route released its stream.
        sleeping = False
        for _ in range(15):
            time.sleep(1)
            if sinks().get(DUMMY) == "SUSPENDED":
                sleeping = True
                break
        check("output allowed to sleep again", sleeping, True)
    finally:
        keeper.terminate()
finally:
    for line in sh(["pactl", "list", "modules", "short"]).stdout.splitlines():
        if "module-null-sink" in line and DUMMY in line:
            sh(["pactl", "unload-module", line.split()[0]])
    print("\ndummy output removed:", DUMMY not in sinks())

print("\n%s" % ("ALL PASS" if not fails else "FAILURES: %s" % fails))
sys.exit(1 if fails else 0)
