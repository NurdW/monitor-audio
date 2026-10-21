#!/usr/bin/python3
"""Throwaway round-trip check: remove everything, then install from the bundle."""
import os
import subprocess
import sys
import time

BUNDLE = os.path.expanduser("~/monitor-audio/build/monitor-audio.AppDir/usr/share/monitor-audio")
os.environ["MONITOR_AUDIO_BUNDLE"] = BUNDLE
os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
sys.path.insert(0, os.path.expanduser("~/monitor-audio/ui"))

import monitor_audio_ui as ui
from PySide6.QtWidgets import QApplication

fails = []


def check(name, got, want=True):
    ok = got == want
    print("%-56s %s" % (name, "ok" if ok else "FAIL got=%r want=%r" % (got, want)))
    if not ok:
        fails.append(name)


def unit_state():
    return subprocess.run(["systemctl", "is-active", "--user", ui.SERVICE],
                          capture_output=True, text=True).stdout.strip()


app = QApplication(sys.argv)
window = ui.Window()

print("--- starting point (should already be installed) ---")
check("service active", unit_state(), "active")
check("kwin script installed", ui.script_installed(), True)
check("kwin script enabled in KDE", ui.script_enabled(), True)
check("service reachable", ui.service_running(), True)

print("\n--- remove ---")
check("remove reported no problems", window.remove_files(), [])
time.sleep(2)
check("service stopped", unit_state(), "inactive")
check("unit file gone", os.path.exists(ui.UNIT_TARGET), False)
check("backend file gone", os.path.exists(ui.BACKEND_TARGET), False)
check("kwin script gone", ui.script_installed(), False)
check("kwin script no longer enabled", ui.script_enabled(), False)
check("service unreachable", ui.service_running(), False)

print("\n--- install from the bundle ---")
check("install reported no problems", window.install_files(), [])
time.sleep(4)
check("service active again", unit_state(), "active")
check("unit file back", os.path.exists(ui.UNIT_TARGET), True)
check("backend file back", os.path.exists(ui.BACKEND_TARGET), True)
check("kwin script back", ui.script_installed(), True)
check("kwin script enabled again", ui.script_enabled(), True)
check("service reachable again", ui.service_running(), True)

print("\n--- the reinstalled service actually works ---")
ok, out = ui.run([sys.executable, ui.BACKEND_TARGET, "json"], timeout=20)
check("status command succeeded", ok, True)
if ok:
    import json
    report = json.loads(out[out.find("{"):])
    check("reports itself running", report["service_running"], True)
    check("sees the audio outputs", len(report["outputs"]) >= 1, True)

ok, out = ui.run(["journalctl", "--user", "-u", ui.SERVICE, "-n", "15",
                  "--no-pager", "-o", "cat"], timeout=20)
recent = [line for line in out.splitlines() if "Traceback" in line or "Failed" in line]
check("no crash in the service log", recent, [])

window.close()
print("\n%s" % ("ALL PASS" if not fails else "FAILURES: %s" % fails))
# Without this the runner sees success whatever the checks found, because a
# script that only prints its verdict always exits 0.
sys.exit(1 if fails else 0)
