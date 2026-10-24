/*
    Tell the monitor-audio service whenever a window is dragged onto a different
    monitor, or changes its title, so it can move that window's audio to that
    monitor's speakers.

    KWin cannot run programs, so the only way out of here is D-Bus. All the
    audio logic (and the monitor -> speaker table) lives in
    ~/.local/share/monitor-audio/monitor-audio.py

    The window id travels along because a browser is one process for several
    windows: without an id per window the service cannot tell the window on the
    TV from the window on the desk, and dragging one would move both. The caption
    travels because a browser window's caption is its active tab's title, which is
    the only thing tying a tab's audio stream to the window that plays it.

    Debug: journalctl -b | grep -i "monitor-audio: kwin"
*/

const SERVICE = "local.MonitorAudio";
const PATH = "/local/MonitorAudio";
const IFACE = "local.MonitorAudio";

function windowKey(c) {
    // internalId is KWin's per-window number; windowId is the older name for it.
    const id = (c.internalId !== undefined && c.internalId !== null)
        ? c.internalId : c.windowId;
    return String(id === undefined || id === null ? c.pid : id);
}

function routeWindow(c) {
    // Only real application windows: panels, wallpaper and popups have no audio story.
    if (!c || !c.normalWindow || c.specialWindow) {
        return;
    }
    const out = c.output;
    if (!out || !c.pid) {
        return;
    }
    // pid travels as a string: KWin's D-Bus bridge and the service must agree exactly.
    // The display model is what the service matches against PipeWire's output
    // description, so the mapping survives replugging and sound profile changes.
    callDBus(SERVICE, PATH, IFACE, "Route",
             String(c.pid), out.name, String(c.resourceClass),
             String(c.desktopFileName), String(c.caption), windowKey(c),
             String(out.model));
}

function watch(c) {
    if (!c) {
        return;
    }
    if (c.outputChanged) {
        // outputChanged fires only when the window's centre crosses onto another
        // monitor, which is exactly a drag across screens and nothing else.
        c.outputChanged.connect(() => routeWindow(c));
    }
    if (c.captionChanged) {
        // A browser switching tabs keeps the same window and the same monitor, yet
        // its audio belongs to a different tab from that moment on, so the service
        // has to hear about the new title. KWin only fires this on a real change.
        c.captionChanged.connect(() => routeWindow(c));
    }
}

// Every window is reported, not only the ones that later move. A browser is one
// process for several windows and the service tells them apart by caption, so a
// window it has never heard of leaves no rival for the one that did move - and the
// moved window then claims that window's audio too, silencing the monitor you are
// still using. Registering up front is what makes "no rivals" mean "only window".
workspace.windowAdded.connect(c => {
    watch(c);
    routeWindow(c);
});
workspace.stackingOrder.forEach(c => {
    watch(c);
    routeWindow(c);
});

// Window has no "windowClosed" signal to connect to per window; the workspace
// reports removals. Without this the service keeps routing to a window that no
// longer exists until the whole application exits.
workspace.windowRemoved.connect(c => callDBus(SERVICE, PATH, IFACE, "Forget", windowKey(c)));

// console.* only: KWin's JS sandbox has no print(), and an uncaught error here
// would tear down this whole script and every handler connected above.
console.log("monitor-audio: kwin script loaded");
