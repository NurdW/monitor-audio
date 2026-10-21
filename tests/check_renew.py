#!/usr/bin/python3
"""Throwaway checks for renewing a silent keep-alive stream before it runs out.

The live check (check_keepalive) watches a real output go to sleep and wake up,
which is the behaviour that matters but takes minutes and only ever sees a stream
that outlives the test. This one shortens the silence file to a couple of seconds
and drives the module in this process, so the renewal itself - the part that has
to overlap the old stream with the new one - is actually observed.

A throwaway null sink is used so nothing on the user's own outputs is touched.
"""
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import time

BACKEND = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                       "backend", "monitor-audio.py")
spec = importlib.util.spec_from_file_location("ma", BACKEND)
ma = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ma)

ma.CONFIG_PATH = os.path.join(tempfile.mkdtemp(prefix="ma-renew-test-"), "config.json")
ma.SILENCE_PATH = os.path.join(tempfile.mkdtemp(prefix="ma-renew-silence-"), "silence.wav")
# A two second file, renewed after one, so the renewal happens while the test
# is watching instead of half an hour from now.
ma.KEEPALIVE_SECONDS = 2
ma.KEEPALIVE_RENEW_BEFORE = 1

SINK = "marenew"
MODEL = "MARENEWMODEL"
fails = []


def sh(args):
    return subprocess.run(args, capture_output=True, text=True)


def check(name, got, want=True):
    ok = got == want
    print("%-62s %s" % (name, "ok" if ok else "\n      FAIL got=%r\n           want=%r" % (got, want)))
    if not ok:
        fails.append(name)


def sink_state():
    for sink in json.loads(sh(["pactl", "-f", "json", "list", "sinks"]).stdout or "[]"):
        if sink.get("name") == SINK:
            return sink.get("state")
    return None


def pid_alive(pid):
    """Is this process still there? stop_process waits on its children, so a
    replaced stream is reaped and shows up as gone rather than as a zombie."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError as exc:
        # Our own child cannot belong to anybody else; if it ever did, saying so
        # beats reporting a wrong answer about the stream.
        raise RuntimeError("cannot tell whether pid %s is alive: %s" % (pid, exc))
    return True


index = None


def held():
    """Is a silent stream on the throwaway output right now?

    A paplay that was just started takes a moment to appear, so this is asked
    repeatedly rather than once.
    """
    return any(ma.is_keepalive(s) and s["sink"] == index for s in ma.sink_inputs())


def wait_held(seconds=3.0):
    deadline = time.time() + seconds
    while not held() and time.time() < deadline:
        time.sleep(0.2)
    return held()


print("--- a stale silence file is rewritten to the length the service expects ---")
with open(ma.SILENCE_PATH, "wb") as handle:
    handle.write(b"\x00" * 100)          # far too short to be the real thing
check("ensure_silence() replaces a file that is too short", ma.ensure_silence(), True)
check("and the file is now the expected length",
      os.path.getsize(ma.SILENCE_PATH) >= 44 + 8000 * ma.KEEPALIVE_SECONDS, True)
size = os.path.getsize(ma.SILENCE_PATH)
check("a file that is long enough is left alone",
      ma.ensure_silence() and os.path.getsize(ma.SILENCE_PATH) == size, True)

sh(["pactl", "load-module", "module-null-sink", "sink_name=%s" % SINK,
    "sink_properties=device.description=%s" % MODEL])
time.sleep(1)

try:
    print("\n--- a route gets its silent stream ---")
    keeper = subprocess.Popen(["sleep", "600"])
    ma.wanted["rw1"] = {"pid": str(keeper.pid), "model": MODEL, "output": None,
                        "appkeys": set(), "captions": set(), "seen": time.time(),
                        "warned": False}
    ma.keepalive_tick()
    entry = ma.keepalive.get(SINK)
    index = next((s["index"] for s in ma.sink_list() if s["name"] == SINK), None)
    check("a silent stream was started on the routed output", entry is not None, True)
    first = entry["process"].pid if entry else None
    check("and it is running", entry is not None and entry["process"].poll() is None, True)
    check("and it really sits on the routed output", wait_held(), True)

    print("\n--- renewal replaces it before it runs out ---")
    time.sleep(ma.KEEPALIVE_SECONDS - ma.KEEPALIVE_RENEW_BEFORE + 0.4)
    ma.keepalive_tick()
    entry = ma.keepalive.get(SINK)
    check("the entry was replaced, not left to expire",
          entry is not None and entry["process"].pid != first, True)
    check("the replacement is running", entry is not None and entry["process"].poll() is None, True)
    check("the old stream is gone rather than orphaned", pid_alive(first), False)
    check("the output still holds a silent stream right now", wait_held(), True)

    print("\n--- forgetting the route releases it ---")
    ma.wanted.clear()
    ma.keepalive_tick()
    check("no silent stream is held after the last route goes", ma.keepalive, {})
    keeper.terminate()
finally:
    ma.stop_keepalive("test finished")
    for line in sh(["pactl", "list", "modules", "short"]).stdout.splitlines():
        if "module-null-sink" in line and SINK in line:
            sh(["pactl", "unload-module", line.split()[0]])
    check("the throwaway output is gone", sink_state(), None)

print("\n%s" % ("ALL PASS" if not fails else "FAILURES: %s" % fails))
sys.exit(1 if fails else 0)
