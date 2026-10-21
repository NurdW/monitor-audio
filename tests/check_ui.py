#!/usr/bin/python3
"""Throwaway headless checks for the control panel window.

These use the real settings file, because the panel writes it and the running
service reads it, and a test that points only one of them somewhere else would
prove nothing about the two agreeing. It is restored exactly - including not
existing - whatever happens below.
"""
import importlib.util
import os
import sys

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
sys.path.insert(0, os.path.expanduser("~/monitor-audio/ui"))

import monitor_audio_ui as ui
from PySide6.QtWidgets import QApplication, QComboBox

CONFIG = ui.CONFIG_PATH
backup = open(CONFIG).read() if os.path.exists(CONFIG) else None
fails = []


def check(name, got, want=True):
    ok = got == want
    print("%-58s %s" % (name, "ok" if ok else "FAIL got=%r want=%r" % (got, want)))
    if not ok:
        fails.append(name)


try:
    app = QApplication(sys.argv)

    print("--- the panel and the service must mean the same file ---")
    spec = importlib.util.spec_from_file_location(
        "ma", os.path.expanduser("~/monitor-audio/backend/monitor-audio.py"))
    ma = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(ma)
    check("same settings path in both", ui.CONFIG_PATH, ma.CONFIG_PATH)

    window = ui.Window()
    check("window title", window.windowTitle(), "Monitor Audio")
    check("headline says running or not running",
          "Running" in window.headline.text() or "Not running" in window.headline.text())

    print("\n--- displays table ---")
    screens = [s.model() or s.name() or "Unknown display" for s in app.screens()]
    models = [window.displays.item(r, 0).text() for r in range(window.displays.rowCount())]
    print("     qt screens:", screens, " rows:", models)
    check("every connected screen gets a row",
          all(model in models for model in screens), True)
    check("no row is blank", all(model for model in models), True)
    combo = window.displays.cellWidget(0, 2)
    check("a display row offers one dropdown that is both answer and control",
          isinstance(combo, QComboBox), True)
    check("the dropdown lists Automatic plus every output", combo.count() >= 2, True)
    check("the table has no separate override column", window.displays.columnCount(), 3)
    automatic_text = combo.itemText(0)
    print("     automatic entry reads:", automatic_text)
    check("automatic names a real output instead of saying \"none\"",
          "no output found" in automatic_text, False)
    check("the first entry is the automatic one", combo.itemData(0), ui.AUTOMATIC)

    print("\n--- live tables and test outputs ---")
    check("test output picker filled", window.test_output.count() >= 1, True)
    check("routes table has its four columns", window.routes.columnCount(), 4)
    check("streams table has its four columns", window.streams.columnCount(), 4)

    print("\n--- pause switch writes settings ---")
    window.set_setting("enabled", False)
    app.processEvents()
    check("switching off is saved", ui.Window.read_config(window)["enabled"], False)
    window.set_setting("enabled", True)
    app.processEvents()
    check("switching back on is saved", ui.Window.read_config(window)["enabled"], True)

    print("\n--- keep-awake switch ---")
    window.set_setting("keep_awake", False)
    app.processEvents()
    check("switching keep-awake off is saved",
          ui.Window.read_config(window)["keep_awake"], False)
    window.set_setting("keep_awake", True)
    app.processEvents()
    check("switching keep-awake on is saved",
          ui.Window.read_config(window)["keep_awake"], True)

    print("\n--- display override round-trip ---")
    model = models[0]
    combo = window.displays.cellWidget(0, 2)
    before_text = combo.currentText()
    target = next((combo.itemData(i) for i in range(combo.count())
                   if combo.itemData(i) != ui.AUTOMATIC), None)
    target_text = next((combo.itemText(i) for i in range(combo.count())
                        if combo.itemData(i) == target), None)
    window.set_override(model, target)
    app.processEvents()
    check("override stored under the display model",
          model in ui.Window.read_config(window)["manual_output"], True)
    check("the service sees the same override",
          ma.load_config()["manual_output"].get(model), target)
    window.displays_signature = None
    window.refresh()
    app.processEvents()
    shown = window.displays.cellWidget(0, 2)
    print("     before override:", before_text)
    print("     after override :", shown.currentText())
    check("the output shown for the display changes when overridden",
          shown.currentText(), target_text)
    check("and it no longer reads as automatic",
          shown.currentText().startswith("Automatic"), False)

    print("\n--- rebuilding must not rewrite settings it was only shown ---")
    with open(CONFIG) as handle:
        as_written = handle.read()
    window.displays_signature = None
    window.refresh()
    app.processEvents()
    with open(CONFIG) as handle:
        after_rebuild = handle.read()
    check("rebuilding the table leaves the file byte-identical", after_rebuild, as_written)

    window.set_override(model, ui.AUTOMATIC)
    check("choosing Automatic removes the override",
          model in ui.Window.read_config(window).get("manual_output", {}), False)

    print("\n--- tone generator ---")
    import tempfile
    import wave
    path = os.path.join(tempfile.mkdtemp(), "t.wav")
    ui.make_tone(path, seconds=0.2)
    with wave.open(path) as handle:
        check("tone is stereo 48kHz", (handle.getnchannels(), handle.getframerate()),
              (2, 48000))
        check("tone has samples", handle.getnframes() > 1000, True)

    window.close()
finally:
    if backup is None:
        try:
            os.remove(CONFIG)
        except FileNotFoundError:
            pass
    else:
        with open(CONFIG, "w") as handle:
            handle.write(backup)

print("\n%s" % ("ALL PASS" if not fails else "FAILURES: %s" % fails))
# Without this the runner sees success whatever the checks found, because a
# script that only prints its verdict always exits 0.
sys.exit(1 if fails else 0)
