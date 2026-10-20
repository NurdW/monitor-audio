#!/usr/bin/python3
"""Throwaway interaction checks: tabs, dropdown stability, resizable columns."""
import os
import sys
import tempfile

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
sys.path.insert(0, os.path.expanduser("~/monitor-audio/ui"))

import monitor_audio_ui as ui
ui.CONFIG_PATH = os.path.join(tempfile.mkdtemp(prefix="ma-layout-test-"), "config.json")
from PySide6.QtWidgets import QApplication, QComboBox, QHeaderView

fails = []


def check(name, got, want=True):
    ok = got == want
    print("%-58s %s" % (name, "ok" if ok else "FAIL got=%r want=%r" % (got, want)))
    if not ok:
        fails.append(name)


app = QApplication(sys.argv)
window = ui.Window()
window.resize(980, 700)
window.show()
for _ in range(6):
    app.processEvents()

print("--- 2. the log lives on its own tab ---")
tabs = [window.tabs.tabText(i) for i in range(window.tabs.count())]
print("     tabs:", tabs)
check("a tab named Log exists", "Log" in tabs, True)
check("the Log tab is the log page",
      window.tabs.widget(tabs.index("Log")) is window.log_page, True)
check("the log is not also shown on the Monitors tab",
      window.log.parent() is not window.tabs.widget(0), True)
check("log is empty before its tab is opened", window.log.toPlainText(), "")
window.tabs.setCurrentWidget(window.log_page)
app.processEvents()
check("opening the Log tab fills it", len(window.log.toPlainText()) > 0, True)
check("Applications tab exists", "Applications" in tabs, True)
check("Audio streams tab exists", "Audio streams" in tabs, True)

print("\n--- 1. the override dropdown survives a refresh ---")
combo = window.displays.cellWidget(0, 2)
check("a display row has a dropdown", isinstance(combo, QComboBox), True)
window.refresh()
app.processEvents()
check("an unchanged refresh does not rebuild the dropdown",
      window.displays.cellWidget(0, 2) is combo, True)

combo.showPopup()
app.processEvents()
shown = combo.view().isVisible()
check("popup actually opened for this test to mean anything", shown, True)
for _ in range(4):
    window.refresh()
    app.processEvents()
check("dropdown still the same widget while its list is open",
      window.displays.cellWidget(0, 2) is combo, True)
check("its list is still open after four refreshes", combo.view().isVisible(), True)
combo.hidePopup()

print("\n--- 3 and 4. columns can be dragged to a new width ---")
for label, table, column in (("streams/Title", window.streams, 1),
                             ("streams/Output", window.streams, 2),
                             ("streams/Placed by", window.streams, 3),
                             ("routes/Display", window.routes, 1),
                             ("routes/Output", window.routes, 2),
                             ("routes/Why", window.routes, 3)):
    header = table.horizontalHeader()
    check("%s is user-resizable" % label,
          header.sectionResizeMode(column) == QHeaderView.Interactive, True)
    width = table.columnWidth(column)
    table.setColumnWidth(column, width + 60)
    check("%s actually takes a new width" % label,
          table.columnWidth(column), width + 60)
    check("%s is not force-stretched to the edge" % label,
          header.stretchLastSection(), False)

print("\n--- a width you set survives a refresh ---")
window.streams.setColumnWidth(1, 420)
window.routes.setColumnWidth(2, 333)
window.refresh()
app.processEvents()
check("Audio streams / Title keeps its width", window.streams.columnWidth(1), 420)
check("Applications / Output keeps its width", window.routes.columnWidth(2), 333)

print("\n--- rows are only repainted when they change ---")
window.refresh()
before = window.shown.get("streams")
window.refresh()
check("identical rows are not written twice", window.shown.get("streams") is before, True)

print("\n--- nothing is squeezed shut on any tab ---")
for name in tabs:
    page = window.tabs.widget(tabs.index(name))
    window.tabs.setCurrentWidget(page)
    app.processEvents()
    check("%s tab has real size" % name,
          page.width() > 200 and page.height() > 150, True)

window.close()
print("\n%s" % ("ALL PASS" if not fails else "FAILURES: %s" % fails))
