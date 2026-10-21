#!/usr/bin/python3
"""Control panel for monitor-audio.

monitor-audio moves an application's audio to the speakers of the monitor its
window sits on. This window installs or removes that background service, lets you
choose which audio output each display should use, and shows what it decided.

It deliberately talks to the rest of the system through commands rather than
linking against PipeWire or KDE libraries, so the same file runs from a source
tree, from an installed copy, or from inside an AppImage.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

from PySide6.QtCore import Qt, QTimer
from PySide6.QtGui import QFont
from PySide6.QtWidgets import (
    QApplication, QAbstractItemView, QCheckBox, QComboBox, QHBoxLayout, QHeaderView,
    QLabel, QMessageBox, QPushButton, QTableWidget, QTableWidgetItem,
    QPlainTextEdit, QTabWidget, QVBoxLayout, QWidget,
)

SERVICE = "monitor-audio.service"
DBUS_NAME = "local.MonitorAudio"
DBUS_PATH = "/local/MonitorAudio"
DBUS_IFACE = "local.MonitorAudio"
KWIN_SCRIPT_ID = "monitor-audio"

BACKEND_TARGET = os.path.expanduser("~/.local/share/monitor-audio/monitor-audio.py")
SCRIPT_TARGET_DIR = os.path.expanduser("~/.local/share/kwin/scripts/%s" % KWIN_SCRIPT_ID)
UNIT_TARGET = os.path.expanduser("~/.config/systemd/user/%s" % SERVICE)

# Same file the service reads; kept in one place here so the path is not spelled
# out differently in three functions.
CONFIG_PATH = os.path.join(os.environ.get("XDG_CONFIG_HOME")
                           or os.path.expanduser("~/.config"),
                           "monitor-audio", "config.json")

# Where the files to install live: inside an AppImage this is the bundled copy,
# otherwise the source tree this file sits in.
BUNDLE = os.environ.get("MONITOR_AUDIO_BUNDLE") or os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))

# Stored value meaning "no override, let the service decide". It is deliberately
# not a display string: the dropdown spells out what automatic resolves to, and
# that text changes with the hardware, so it cannot double as the stored value.
AUTOMATIC = ""


def run(args, timeout=30):
    """Run a command, returning (ok, output)."""
    try:
        result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError) as exc:
        return False, str(exc)
    text = (result.stdout or "") + (result.stderr or "")
    return result.returncode == 0, text.strip()


def service_running():
    ok, out = run(["busctl", "--user", "list", "--no-pager"])
    return ok and DBUS_NAME in out


def script_installed():
    return os.path.isdir(SCRIPT_TARGET_DIR)


def script_enabled():
    ok, out = run(["kreadconfig6", "--file", "kwinrc", "--group", "Plugins",
                   "--key", "%sEnabled" % KWIN_SCRIPT_ID])
    return ok and out.strip().lower() == "true"


def script_loaded():
    ok, out = run(["gdbus", "call", "--session", "--dest", "org.kde.KWin",
                   "--object-path", "/Scripting",
                   "--method", "org.kde.kwin.Scripting.isScriptLoaded",
                   KWIN_SCRIPT_ID])
    return ok and "true" in out


def fetch_report(backend):
    """Ask the service what it currently knows.

    Returns (report, problem), report being None when that is not possible and
    problem then saying why in the panel's own words: a service that is running
    but slow, or answering badly, is not the same thing as one that is absent,
    and the panel used to claim "not running" for all three.
    """
    if not backend:
        return None, "the service file is not installed"
    ok, out = run([sys.executable, backend, "json"], timeout=20)
    if not ok:
        return None, "asking the service failed: %s" % (out or "no output")
    start = out.find("{")
    if start < 0:
        return None, "the service answered without json: %s" % (out[:80] or "no output")
    try:
        return json.loads(out[start:]), None
    except json.JSONDecodeError as exc:
        return None, "the service answered unreadable json: %s" % exc


def make_tone(path, seconds=1.5, frequency=440):
    """Write a short sine tone so a speaker test can identify an output by ear."""
    import math
    import wave
    rate = 48000
    frames = bytearray()
    for index in range(int(rate * seconds)):
        # Fade in and out so the tone has no click at either end.
        position = index / rate
        fade = min(1.0, position / 0.03, (seconds - position) / 0.03)
        value = int(0.25 * 32767 * fade * math.sin(2 * math.pi * frequency * position))
        frames += value.to_bytes(2, sys.byteorder, signed=True) * 2
    with wave.open(path, "w") as handle:
        handle.setnchannels(2)
        handle.setsampwidth(2)
        handle.setframerate(rate)
        handle.writeframes(bytes(frames))


class Window(QWidget):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("Monitor Audio")
        self.resize(880, 720)
        self.tempdir = tempfile.mkdtemp(prefix="monitor-audio-ui-")
        self.status_text = ""
        # Last rows put into each table, and the last state of the displays table.
        # Compared before repainting so a refresh cannot disturb what the user is
        # pointing at.
        self.shown = {}
        self.displays_signature = None
        self._build()
        self.timer = QTimer(self)
        self.timer.timeout.connect(self.refresh)
        self.timer.start(3000)
        self.refresh()

    # ---------- layout ----------

    def _build(self):
        root = QVBoxLayout(self)

        self.headline = QLabel("Checking...")
        self.headline.setWordWrap(True)
        self.headline.setFont(QFont("", 11, QFont.Bold))
        root.addWidget(self.headline)
        self.subline = QLabel("")
        self.subline.setWordWrap(True)
        root.addWidget(self.subline)

        controls = QHBoxLayout()
        self.pause = QCheckBox("Routing switched on")
        self.pause.toggled.connect(lambda on: self.set_setting("enabled", on))
        controls.addWidget(self.pause)
        self.keep_awake = QCheckBox("Keep routed outputs awake")
        self.keep_awake.setToolTip(
            "Speakers that are not used go to sleep after a few seconds and take a\n"
            "moment to wake, which is the mute you hear when resuming after being\n"
            "away. Holding one silent stream on each output you route to prevents\n"
            "that. The silent stream does appear in the volume mixer.")
        self.keep_awake.toggled.connect(lambda on: self.set_setting("keep_awake", on))
        controls.addWidget(self.keep_awake)
        self.install_button = QPushButton("Install")
        self.install_button.clicked.connect(self.install_backend)
        controls.addWidget(self.install_button)
        self.uninstall_button = QPushButton("Remove")
        self.uninstall_button.clicked.connect(self.uninstall_backend)
        controls.addWidget(self.uninstall_button)
        self.reset_button = QPushButton("Forget current routes")
        self.reset_button.clicked.connect(self.reset_routes)
        controls.addWidget(self.reset_button)
        controls.addStretch(1)
        root.addLayout(controls)

        self.tabs = QTabWidget()
        root.addWidget(self.tabs, 1)

        monitors = QWidget()
        layout = QVBoxLayout(monitors)
        self.displays = self._make_table(
            "displays", ["Display", "Connector", "Audio output in use"],
            [220, 110, 520])
        layout.addWidget(self.displays)
        hint = QLabel("Automatic uses the output whose name contains the display model. "
                      "Pick another only when that guess is wrong - the choice is saved "
                      "per monitor and survives replugging cables.")
        hint.setWordWrap(True)
        layout.addWidget(hint)
        test = QHBoxLayout()
        self.test_output = QComboBox()
        test.addWidget(self.test_output, 1)
        test.addWidget(QPushButton("Play a tone", clicked=self.play_tone))
        layout.addLayout(test)
        self.tabs.addTab(monitors, "Monitors")

        self.routes = self._make_table(
            "routes", ["Application", "Display", "Output", "Why"],
            [200, 180, 300, 140])
        self.tabs.addTab(self.routes, "Applications")

        self.streams = self._make_table(
            "streams", ["Audio stream", "Title", "Output", "Placed by"],
            [180, 320, 300, 140])
        self.tabs.addTab(self.streams, "Audio streams")

        self.log_page = QWidget()
        layout = QVBoxLayout(self.log_page)
        self.follow = QCheckBox("Follow new lines")
        self.follow.setChecked(True)
        layout.addWidget(self.follow)
        self.log = QPlainTextEdit()
        self.log.setReadOnly(True)
        self.log.setMaximumBlockCount(2000)
        self.log.setFont(QFont("monospace", 9))
        layout.addWidget(self.log)
        self.tabs.addTab(self.log_page, "Log")
        # The log is only worth reading while it is on screen, so it is read from
        # the moment the tab is opened rather than on every timer tick.
        self.tabs.currentChanged.connect(self.tab_changed)

    def _make_table(self, name, headers, widths):
        """A read-only table whose column widths the user can drag.

        Every column is left in Interactive mode on purpose. A stretched column
        cannot be resized, and the columns worth widening - titles and output
        names - are exactly the ones that would otherwise be stretched.
        """
        table = QTableWidget(0, len(headers))
        table.setObjectName(name)
        table.setHorizontalHeaderLabels(headers)
        table.verticalHeader().setVisible(False)
        table.setEditTriggers(QAbstractItemView.NoEditTriggers)
        table.setSelectionBehavior(QAbstractItemView.SelectRows)
        header = table.horizontalHeader()
        header.setSectionResizeMode(QHeaderView.Interactive)
        header.setSectionsMovable(True)
        # Stretching the last section would make it the one column that cannot be
        # dragged, and that is the column people ask for room in.
        header.setStretchLastSection(False)
        for column, width in enumerate(widths):
            table.setColumnWidth(column, width)
        return table

    # ---------- actions ----------

    def set_setting(self, key, value):
        """Change one setting and let the service pick it up on its next pass."""
        config = self.read_config()
        config[key] = value
        self.write_config(config)
        self.refresh()

    def read_config(self):
        try:
            with open(CONFIG_PATH) as handle:
                data = json.load(handle)
            if isinstance(data, dict):
                return data
        except FileNotFoundError:
            return {}
        except (OSError, json.JSONDecodeError) as exc:
            self.status_text = "Could not read settings: %s" % exc
        return {}

    def write_config(self, config):
        os.makedirs(os.path.dirname(CONFIG_PATH), exist_ok=True)
        # Written to a temporary file and moved into place. The service re-reads
        # this file every few seconds, and a half-written one would quietly drop
        # it back to its defaults while the panel still looked saved.
        fd, tmp = tempfile.mkstemp(prefix="config.", dir=os.path.dirname(CONFIG_PATH))
        try:
            with os.fdopen(fd, "w") as handle:
                json.dump(config, handle, indent=2)
                handle.write("\n")
            os.replace(tmp, CONFIG_PATH)
        except OSError as exc:
            # The save is the failure being reported either way; a temp file left
            # behind is worth a line, not worth hiding.
            try:
                os.unlink(tmp)
                detail = ""
            except OSError as cleanup:
                detail = "\n\nA temporary file was left behind: %s" % cleanup
            QMessageBox.warning(self, "Could not save", "%s%s" % (exc, detail))

    def set_override(self, model, output_name):
        """Record the choice for one display; empty output_name means decide automatically."""
        config = self.read_config()
        overrides = config.get("manual_output")
        if not isinstance(overrides, dict):
            overrides = {}
        if output_name == AUTOMATIC:
            overrides.pop(model, None)
        else:
            overrides[model] = output_name
        config["manual_output"] = overrides
        self.write_config(config)
        self.refresh()

    def install_files(self):
        """Copy the service, KWin script and systemd unit into place.

        Returns what went wrong; an empty list means everything worked. Split from
        the button handler so the work can run without a dialog getting in the way.
        """
        backend_source = os.path.join(BUNDLE, "backend", "monitor-audio.py")
        script_source = os.path.join(BUNDLE, "kwin-script", KWIN_SCRIPT_ID)
        unit_source = os.path.join(BUNDLE, "packaging", SERVICE)

        problems = []
        if not os.path.isfile(backend_source):
            problems.append("bundled service file is missing (%s)" % backend_source)
        if not os.path.isdir(script_source):
            problems.append("bundled KWin script is missing (%s)" % script_source)
        if not os.path.isfile(unit_source):
            problems.append("bundled systemd unit is missing (%s)" % unit_source)
        if problems:
            return problems

        os.makedirs(os.path.dirname(BACKEND_TARGET), exist_ok=True)
        shutil.copy2(backend_source, BACKEND_TARGET)

        os.makedirs(os.path.dirname(SCRIPT_TARGET_DIR), exist_ok=True)
        if os.path.isdir(SCRIPT_TARGET_DIR):
            shutil.rmtree(SCRIPT_TARGET_DIR)
        shutil.copytree(script_source, SCRIPT_TARGET_DIR)

        os.makedirs(os.path.dirname(UNIT_TARGET), exist_ok=True)
        shutil.copy2(unit_source, UNIT_TARGET)

        for command in (["systemctl", "--user", "daemon-reload"],
                        ["systemctl", "--user", "enable", "--now", SERVICE],
                        ["kwriteconfig6", "--file", "kwinrc", "--group", "Plugins",
                         "--key", "%sEnabled" % KWIN_SCRIPT_ID, "true"],
                        ["kbuildsycoca6", "--noincremental"],
                        ["gdbus", "call", "--session", "--dest", "org.kde.KWin",
                         "--object-path", "/KWin", "--method", "org.kde.KWin.reconfigure"]):
            ok, out = run(command, timeout=120)
            if not ok:
                problems.append("%s: %s" % (command[0], out or "failed"))
        return problems

    def install_backend(self):
        problems = self.install_files()
        if problems:
            QMessageBox.critical(self, "Cannot install", "\n".join(problems))
        else:
            self.status_text = "Installed. Drag a window to another monitor to try it."
        self.refresh()

    def remove_files(self):
        """Stop the service and delete what install_files put in place.

        KDE is told to reload the script list either way, otherwise a removed
        script stays loaded until logout.
        """
        problems = []
        for command in (["systemctl", "--user", "disable", "--now", SERVICE],
                        ["kwriteconfig6", "--file", "kwinrc", "--group", "Plugins",
                         "--key", "%sEnabled" % KWIN_SCRIPT_ID, "--delete"],
                        ["gdbus", "call", "--session", "--dest", "org.kde.KWin",
                         "--object-path", "/KWin", "--method", "org.kde.KWin.reconfigure"]):
            ok, out = run(command, timeout=60)
            if not ok and out:
                problems.append("%s: %s" % (command[0], out))

        for path in (UNIT_TARGET, BACKEND_TARGET):
            try:
                os.remove(path)
            except FileNotFoundError:
                pass
            except OSError as exc:
                problems.append("could not remove %s: %s" % (path, exc))
        try:
            shutil.rmtree(SCRIPT_TARGET_DIR)
        except FileNotFoundError:
            pass
        except OSError as exc:
            problems.append("could not remove the KWin script: %s" % exc)

        run(["systemctl", "--user", "daemon-reload"])
        return problems

    def uninstall_backend(self):
        answer = QMessageBox.question(
            self, "Remove monitor-audio?",
            "This stops audio following your windows, removes the background "
            "service and the KWin script, and turns the script off in KDE.\n\n"
            "Your display choices file under ~/.config/monitor-audio is kept.",
            QMessageBox.Yes | QMessageBox.No, QMessageBox.No)
        if answer != QMessageBox.Yes:
            return
        problems = self.remove_files()
        if problems:
            QMessageBox.warning(self, "Removed with problems", "\n".join(problems))
        else:
            self.status_text = "Removed."
        self.refresh()

    def reset_routes(self):
        backend = self.backend_path()
        if not backend:
            QMessageBox.information(self, "Not installed",
                                    "The background service is not installed.")
            return
        ok, out = run([sys.executable, backend, "reset"], timeout=20)
        if not ok:
            QMessageBox.warning(self, "Could not reset", out)
        self.refresh()

    def play_tone(self):
        output = self.test_output.currentData()
        if not output:
            QMessageBox.information(self, "No output", "Pick an output to test first.")
            return
        path = os.path.join(self.tempdir, "tone.wav")
        try:
            make_tone(path)
        except OSError as exc:
            QMessageBox.warning(self, "Could not make the tone", str(exc))
            return
        ok, out = run(["paplay", "--device=%s" % output, path], timeout=30)
        if not ok:
            QMessageBox.warning(self, "Could not play", out or "paplay failed")

    # ---------- refresh ----------

    def backend_path(self):
        for candidate in (BACKEND_TARGET, os.path.join(BUNDLE, "backend", "monitor-audio.py")):
            if os.path.isfile(candidate):
                return candidate
        return None

    def refresh(self):
        if self.isMinimized():
            # Nothing on screen to update, and a refresh is not free: a python
            # process here, plus pactl and pw-dump over in the service.
            return
        backend = self.backend_path()
        # One call covers both cases: the command answers with a partial report of
        # its own when the service is unreachable, so asking busctl first was a
        # second process every few seconds to learn what this one already knows.
        report, problem = fetch_report(backend)
        if report is None:
            # Fall back to the settings file itself, so the switches and the
            # per-display choices still describe what is saved rather than
            # silently resetting to defaults while the service is unreachable.
            saved = self.read_config()
            report = {"service_running": False, "outputs": [], "routes": [], "streams": [],
                      "config": {"enabled": bool(saved.get("enabled", True)),
                                 "keep_awake": bool(saved.get("keep_awake", True)),
                                 "manual_output": saved.get("manual_output") or {}}}
            if problem:
                self.status_text = problem

        outputs = report["outputs"]
        output_names = [o["name"] for o in outputs]
        descriptions = {o["name"]: o["description"] for o in outputs}
        overrides = report["config"].get("manual_output") or {}

        self.headline.setText(
            "Running - audio follows the monitor a window sits on"
            if report["service_running"] else
            "Not running - install the background service to start routing")
        self.subline.setText(
            "KWin script: %s    Settings: %s%s"
            % ("loaded" if script_loaded() else ("installed, not loaded" if script_installed() else "not installed"),
               CONFIG_PATH,
               "    |    " + self.status_text if self.status_text else ""))

        self.pause.blockSignals(True)
        self.pause.setChecked(bool(report["config"].get("enabled", True)))
        self.pause.blockSignals(False)
        self.keep_awake.blockSignals(True)
        self.keep_awake.setChecked(bool(report["config"].get("keep_awake", True)))
        self.keep_awake.blockSignals(False)
        self.install_button.setEnabled(backend is not None)
        already = os.path.isfile(BACKEND_TARGET) or script_installed()
        self.install_button.setText("Reinstall" if already else "Install")
        self.uninstall_button.setEnabled(already)

        busy = self.popup_open()
        if not busy:
            self.fill_displays(overrides, output_names, descriptions, report)
            if self.test_output.count() != len(outputs):
                self.test_output.clear()
                for entry in outputs:
                    self.test_output.addItem(entry["description"], entry["name"])

        self.fill_table(self.routes, [
            (", ".join(r["apps"]) or "?", r["model"] or "?",
             descriptions.get(r["output"], r["output"] or "none"), r["how"])
            for r in report["routes"]])
        self.fill_table(self.streams, [
            (", ".join(s["apps"]) or "?", ", ".join(s["titles"]) or "?",
             descriptions.get(s["output"], s["output"] or "none"),
             "this app" if s["placed_by"] else "nobody yet")
            for s in report["streams"]])

        if self.tabs.currentWidget() is self.log_page and self.follow.isChecked():
            self.refresh_log()

    def popup_open(self):
        """True while a dropdown list is on screen.

        Rebuilding a row destroys its combo box, and a destroyed combo box takes
        its open popup along with it - which is how the override list used to
        close itself a few seconds after being opened.
        """
        for combo in self.findChildren(QComboBox):
            if combo.view() is not None and combo.view().isVisible():
                return True
        return False

    def tab_changed(self, index):
        if self.tabs.widget(index) is self.log_page:
            self.refresh_log()

    def refresh_log(self):
        ok, out = run(["journalctl", "--user", "-u", SERVICE, "-n", "200",
                       "--no-pager", "-o", "cat"], timeout=20)
        self.log.setPlainText(out if ok else out)

    def resolutions(self, models):
        """Ask the service which output each display would use, and why.

        Asked rather than worked out here, so this table cannot disagree with what
        the routing is actually doing - which it did when the rules were copied.
        """
        backend = self.backend_path()
        if not backend or not models:
            return {}
        ok, out = run([sys.executable, backend, "resolve", json.dumps(models)], timeout=20)
        if not ok:
            self.status_text = "Could not ask the service about your displays: %s" % (out or "failed")
            return {}
        start = out.find("{")
        if start < 0:
            self.status_text = "The service gave an unreadable answer about your displays"
            return {}
        try:
            return json.loads(out[start:])
        except json.JSONDecodeError as exc:
            self.status_text = "The service gave unreadable json about your displays: %s" % exc
            return {}

    def fill_displays(self, overrides, output_names, descriptions, report):
        """One row per connected display; the dropdown is both the answer and the control.

        There used to be a separate read-only "in use" column next to an override
        dropdown, which meant the column could - and did - describe a decision the
        override had already changed. One column cannot say two different things.

        Rebuilt only when something it shows has actually changed, so the timer
        cannot rebuild a row while it is being used.
        """
        rows = []
        for screen in QApplication.instance().screens():
            rows.append((screen.model() or screen.name() or "Unknown display",
                         screen.name()))
        # A display that is connected but has no window on it yet still needs a row.
        for route in report["routes"]:
            if all(route["model"] != model for model, _ in rows) and route["model"]:
                rows.append((route["model"], ""))

        # Everything a row can show is decided by these three, so the service is
        # only asked once one of them moves. Asking every few seconds about a
        # desktop that is not changing cost another python process plus two
        # PipeWire dumps, for an answer that was then thrown away.
        signature = (tuple(rows), tuple(output_names), tuple(sorted(overrides.items())))
        if signature == self.displays_signature:
            return
        self.displays_signature = signature
        resolved = self.resolutions([model for model, _ in rows])

        why_words = {
            "display": "Chosen because an output names this display.",
            "fallback": "This display carries no audio of its own, so it uses the "
                        "fallback output.",
            "override": "You chose this one.",
            "override-miss": "You chose an output that is not present right now, "
                             "so nothing is routed to this display.",
            "none": "No output matches this display and no fallback is available.",
        }

        self.displays.setRowCount(len(rows))
        for row, (model, connector) in enumerate(rows):
            info = resolved.get(model, {})
            self.displays.setItem(row, 0, QTableWidgetItem(model))
            self.displays.setItem(row, 1, QTableWidgetItem(connector))

            automatic = "Automatic - %s" % (info.get("description") or "no output found")
            combo = QComboBox()
            combo.setMinimumWidth(260)
            combo.addItem(automatic, AUTOMATIC)
            for name in output_names:
                combo.addItem(descriptions[name], name)
            chosen = overrides.get(model)
            index = combo.findData(chosen) if chosen else -1
            if chosen and index < 0:
                # Settings written before this panel stored output names hold the
                # output's description instead, which the service accepts too.
                # Matching only the name left such a display showing "Automatic"
                # while the routing was in fact following the saved choice.
                index = next((i for i in range(1, combo.count())
                              if combo.itemText(i) == chosen), -1)
            if index >= 0:
                combo.setCurrentIndex(index)
            # Driven by what matched, not by whether a choice exists: a saved
            # choice naming an output that is gone resolves to override-miss in
            # the service, and the row has to say that rather than claim a choice
            # that is not in force.
            reason = "override" if index >= 0 else info.get("how", "none")
            combo.setToolTip("%s\n%s" % (info.get("description") or "nothing",
                                         why_words.get(reason, "")))
            # Connected only after the saved choice is in place: setting the index
            # above would otherwise fire this signal and write the settings file
            # again, turning a hand-written value into the dropdown's own wording.
            combo.currentIndexChanged.connect(
                lambda _, box=combo, m=model: self.set_override(m, box.currentData()))
            self.displays.setCellWidget(row, 2, combo)

    def fill_table(self, table, rows):
        """Replace a table's rows, but only when they differ from what is shown.

        Repainting identical rows would drop the selected row every few seconds,
        which is as annoying as a dropdown that closes on its own.
        """
        if self.shown.get(table.objectName()) == rows:
            return
        self.shown[table.objectName()] = rows
        table.setRowCount(len(rows))
        for row, values in enumerate(rows):
            for column, value in enumerate(values):
                table.setItem(row, column, QTableWidgetItem(value))

    def showEvent(self, event):
        """Start polling again once the window is on screen.

        Polling stops while the window is hidden (see hideEvent): behind a closed
        window nobody reads the answer, and producing it was most of this
        program's idle cost.
        """
        super().showEvent(event)
        self.timer.start(3000)
        self.refresh()

    def hideEvent(self, event):
        self.timer.stop()
        super().hideEvent(event)

    def closeEvent(self, event):
        self.timer.stop()
        shutil.rmtree(self.tempdir, ignore_errors=True)
        super().closeEvent(event)


def main():
    app = QApplication(sys.argv)
    app.setApplicationName("Monitor Audio")
    window = Window()
    window.show()
    sys.exit(app.exec())


if __name__ == "__main__":
    main()
