//! Putting the service in place, and taking it back out.
//!
//! Everything is parameterised by `Roots` and a `Runner` so the whole of this can
//! run against a temporary directory in a test instead of rearranging a real
//! desktop. The panel is the only part of this project that can damage a working
//! install, so it is the part that gets tested the hardest.

use std::path::{Path, PathBuf};

pub const SERVICE: &str = "monitor-audio.service";
pub const KWIN_SCRIPT_ID: &str = "monitor-audio";

/// The KWin script travels inside the binary. KWin only loads QML/JavaScript, so
/// this stays JavaScript whatever language the rest is written in.
pub const KWIN_METADATA: &str = include_str!("../../../../kwin-script/monitor-audio/metadata.json");
pub const KWIN_MAIN_JS: &str =
    include_str!("../../../../kwin-script/monitor-audio/contents/code/main.js");

/// Where things go. Split out so a test can point it at a temporary directory.
#[derive(Clone, Debug)]
pub struct Roots {
    pub backend_target: PathBuf,
    pub script_dir: PathBuf,
    pub unit_target: PathBuf,
}

impl Roots {
    /// Refuse a home directory that is empty or relative.
    ///
    /// Every target below is built by joining onto this path, so an empty `HOME`
    /// would turn `~/.local/share/...` into a relative path and the installer
    /// would happily write into whatever directory the panel happened to be
    /// started from - and `remove` would delete from there too.
    pub fn from_home_checked(home: &Path) -> Option<Roots> {
        let text = home.to_string_lossy();
        if text.is_empty() || !home.is_absolute() {
            return None;
        }
        Some(Roots::from_home(home))
    }

    pub fn from_env() -> Option<Roots> {
        match std::env::var("HOME") {
            Ok(value) if !value.is_empty() => Roots::from_home_checked(Path::new(&value)),
            _ => None,
        }
    }

    pub fn from_home(home: &Path) -> Roots {
        Roots {
            backend_target: home.join(".local/share/monitor-audio/monitor-audio"),
            script_dir: home.join(".local/share/kwin/scripts").join(KWIN_SCRIPT_ID),
            unit_target: home.join(".config/systemd/user").join(SERVICE),
        }
    }
}

/// Running commands is the other thing a test must not do for real.
pub trait Runner {
    fn run(&self, args: &[&str]) -> Result<String, String>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let output = std::process::Command::new(args[0])
            .args(&args[1..])
            .output()
            .map_err(|err| format!("{}: {}", args[0], err))?;
        if output.status.success() {
            Ok(String::new())
        } else {
            Err(format!(
                "{}: {}",
                args[0],
                String::from_utf8_lossy(&output.stderr).trim().chars().take(200).collect::<String>()
            ))
        }
    }
}

/// The systemd unit, written here rather than shipped as a file because the
/// ExecStart path is only known once the binary has been copied somewhere.
pub fn unit_text(backend: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Route application audio to the monitor its window is on\n\
         After=pipewire.service wireplumber.service\n\
         \n\
         [Service]\n\
         # Not Type=dbus: Fedora 44 SIGTERMs those units before they finish starting.\n\
         Type=simple\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         Nice=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        backend.display()
    )
}

fn write_file(path: &Path, bytes: &[u8], executable: bool) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {}", parent.display(), err))?;
    }
    std::fs::write(path, bytes).map_err(|err| format!("could not write {}: {}", path.display(), err))?;
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|err| format!("could not make {} runnable: {}", path.display(), err))?;
    }
    Ok(())
}

/// Copy the service, KWin script and systemd unit into place. The returned list
/// says what went wrong; empty means everything worked.
/// `backend_source` is the daemon binary to install. It is a parameter rather
/// than `current_exe` because the panel and the service are two programs: the
/// panel asking to be installed as the service would produce a unit that starts
/// a window and routes nothing.
pub fn install_files(roots: &Roots, backend_source: &Path, runner: &dyn Runner) -> Vec<String> {
    let mut problems = Vec::new();

    let binary = std::fs::read(backend_source)
        .map_err(|err| format!("could not read the service binary at {}: {}", backend_source.display(), err));
    match binary {
        Ok(bytes) => {
            if let Err(err) = write_file(&roots.backend_target, &bytes, true) {
                problems.push(err);
            }
        }
        Err(err) => {
            problems.push(err);
            // Without a binary there is nothing for the unit to start, so stop
            // here rather than installing a service that cannot run.
            problems.push("nothing was installed".to_string());
            return problems;
        }
    }

    // Replaced wholesale: a script left over from an older version would keep
    // running with the old behaviour and nothing would say so.
    if roots.script_dir.exists() {
        if let Err(err) = std::fs::remove_dir_all(&roots.script_dir) {
            problems.push(format!("could not clear the old KWin script: {}", err));
        }
    }
    for (name, body) in [
        ("metadata.json", KWIN_METADATA),
        ("contents/code/main.js", KWIN_MAIN_JS),
    ] {
        if let Err(err) = write_file(&roots.script_dir.join(name), body.as_bytes(), false) {
            problems.push(err);
        }
    }

    if let Err(err) = write_file(&roots.unit_target, unit_text(&roots.backend_target).as_bytes(), false) {
        problems.push(err);
    }

    for command in [
        vec!["systemctl", "--user", "daemon-reload"],
        vec!["systemctl", "--user", "enable", "--now", SERVICE],
        vec!["kwriteconfig6", "--file", "kwinrc", "--group", "Plugins",
             "--key", &format!("{}Enabled", KWIN_SCRIPT_ID), "true"],
        vec!["kbuildsycoca6", "--noincremental"],
        vec!["gdbus", "call", "--session", "--dest", "org.kde.KWin", "--object-path", "/KWin",
             "--method", "org.kde.KWin.reconfigure"],
    ] {
        if let Err(problem) = runner.run(&command) {
            problems.push(problem);
        }
    }
    problems
}

/// Stop the service and delete what `install_files` put in place. KDE is told to
/// reload its script list either way, otherwise a removed script stays loaded
/// until logout.
pub fn remove_files(roots: &Roots, runner: &dyn Runner) -> Vec<String> {
    let mut problems = Vec::new();
    for command in [
        vec!["systemctl", "--user", "disable", "--now", SERVICE],
        vec!["kwriteconfig6", "--file", "kwinrc", "--group", "Plugins",
             "--key", &format!("{}Enabled", KWIN_SCRIPT_ID), "--delete"],
        vec!["gdbus", "call", "--session", "--dest", "org.kde.KWin", "--object-path", "/KWin",
             "--method", "org.kde.KWin.reconfigure"],
    ] {
        if let Err(problem) = runner.run(&command) {
            problems.push(problem);
        }
    }

    for path in [&roots.unit_target, &roots.backend_target] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => problems.push(format!("could not remove {}: {}", path.display(), err)),
        }
    }
    match std::fs::remove_dir_all(&roots.script_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => problems.push(format!("could not remove the KWin script: {}", err)),
    }

    if let Err(problem) = runner.run(&["systemctl", "--user", "daemon-reload"]) {
        problems.push(problem);
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Recorded {
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl Runner for Recorded {
        fn run(&self, args: &[&str]) -> Result<String, String> {
            self.calls.borrow_mut().push(args.iter().map(|a| a.to_string()).collect());
            Ok(String::new())
        }
    }

    fn temp_roots(name: &str) -> Roots {
        let home = std::env::temp_dir().join(format!("ma-panel-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&home).expect("temp home");
        Roots::from_home(&home)
    }

    fn fake_binary(roots: &Roots) -> PathBuf {
        let source = roots.script_dir.parent().unwrap().join("fake-binary");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"#!/bin/sh\necho hi\n").unwrap();
        source
    }

    #[test]
    fn install_puts_all_three_pieces_in_place() {
        let roots = temp_roots("install");
        let binary = fake_binary(&roots);
        let runner = Recorded::default();
        let problems = install_files(&roots, &binary, &runner);
        assert_eq!(problems, Vec::<String>::new(), "install reported: {:?}", problems);
        assert!(roots.backend_target.is_file());
        assert!(roots.script_dir.join("metadata.json").is_file());
        assert!(roots.script_dir.join("contents/code/main.js").is_file());
        assert!(roots.unit_target.is_file());
        std::fs::remove_dir_all(roots.script_dir.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn the_installed_binary_is_runnable() {
        // A copied file that lost its execute bit leaves systemd unable to start
        // the service, and the failure shows up as nothing happening at all.
        use std::os::unix::fs::PermissionsExt;
        let roots = temp_roots("mode");
        let binary = fake_binary(&roots);
        install_files(&roots, &binary, &Recorded::default());
        let mode = std::fs::metadata(&roots.backend_target).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "installed binary is not executable: {:o}", mode);
        std::fs::remove_dir_all(roots.script_dir.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn the_unit_starts_the_installed_binary_and_is_not_type_dbus() {
        let text = unit_text(Path::new("/home/someone/.local/share/monitor-audio/monitor-audio"));
        let settings: Vec<&str> = text.lines().map(str::trim).collect();
        assert!(settings.contains(&"Type=simple"), "Type=dbus is SIGTERMed on Fedora 44");
        assert!(!settings.contains(&"Type=dbus"),
                "the comment mentions Type=dbus to explain why it is not used");
        assert!(text.contains("ExecStart=/home/someone/.local/share/monitor-audio/monitor-audio"));
    }

    #[test]
    fn systemd_accepts_the_generated_unit() {
        // Checked with the tool that will actually load it, rather than by
        // eyeballing the string: a typo here means the service never starts and
        // the only symptom is that nothing happens.
        let path = std::env::temp_dir().join(format!("ma-unit-{}.service", std::process::id()));
        std::fs::write(&path, unit_text(Path::new("/bin/sleep"))).expect("write unit");
        let analysis = std::process::Command::new("systemd-analyze")
            .args(["verify", &path.to_string_lossy()])
            .output();
        match analysis {
            Ok(output) => {
                let complaints = String::from_utf8_lossy(&output.stderr).to_string();
                let about_this_unit: Vec<&str> = complaints
                    .lines()
                    .filter(|line| line.contains(&path.file_name().unwrap().to_string_lossy().to_string()))
                    .collect();
                assert!(about_this_unit.is_empty(), "systemd rejected the unit: {:?}", about_this_unit);
            }
            // A machine without the tool is a reason to say so, not to pass quietly.
            Err(err) => eprintln!("skipped: systemd-analyze is not available here ({})", err),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn install_tells_systemd_and_kwin_about_the_change() {
        let roots = temp_roots("commands");
        let binary = fake_binary(&roots);
        let runner = Recorded::default();
        install_files(&roots, &binary, &runner);
        let calls = runner.calls.borrow();
        let joined: Vec<String> = calls.iter().map(|c| c.join(" ")).collect();
        assert!(joined.iter().any(|c| c.contains("daemon-reload")), "{:?}", joined);
        assert!(joined.iter().any(|c| c.contains("enable --now monitor-audio.service")));
        assert!(joined.iter().any(|c| c.contains("monitor-audioEnabled true")));
        assert!(joined.iter().any(|c| c.contains("kbuildsycoca6")));
        assert!(joined.iter().any(|c| c.contains("org.kde.KWin.reconfigure")));
        std::fs::remove_dir_all(roots.script_dir.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn a_failing_command_is_reported_rather_than_swallowed() {
        struct AlwaysFails;
        impl Runner for AlwaysFails {
            fn run(&self, args: &[&str]) -> Result<String, String> {
                Err(format!("{}: refused", args[0]))
            }
        }
        let roots = temp_roots("fails");
        let binary = fake_binary(&roots);
        let problems = install_files(&roots, &binary, &AlwaysFails);
        assert!(!problems.is_empty(), "a command that failed must not pass silently");
        std::fs::remove_dir_all(roots.script_dir.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn an_unusable_home_directory_is_refused_rather_than_used() {
        assert!(Roots::from_home_checked(Path::new("")).is_none());
        assert!(Roots::from_home_checked(Path::new("relative/path")).is_none());
        assert!(Roots::from_home_checked(Path::new("/tmp/somewhere")).is_some());
    }

    #[test]
    fn remove_takes_everything_back_out() {
        let roots = temp_roots("remove");
        let binary = fake_binary(&roots);
        install_files(&roots, &binary, &Recorded::default());
        let problems = remove_files(&roots, &Recorded::default());
        assert_eq!(problems, Vec::<String>::new(), "remove reported: {:?}", problems);
        assert!(!roots.backend_target.exists());
        assert!(!roots.unit_target.exists());
        assert!(!roots.script_dir.exists());
        std::fs::remove_dir_all(roots.script_dir.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn removing_something_that_was_never_installed_is_not_an_error() {
        let roots = temp_roots("never");
        let problems = remove_files(&roots, &Recorded::default());
        assert_eq!(problems, Vec::<String>::new(), "reported: {:?}", problems);
        std::fs::remove_dir_all(roots.script_dir.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn the_kwin_script_registers_every_window_not_just_the_ones_that_move() {
        // A browser is one process for several windows and the service tells them
        // apart by caption. A window it never heard of leaves no rival, and the
        // window that did move then claims that window's audio too - which silences
        // the monitor you are still using. Registration is what prevents it.
        assert!(KWIN_MAIN_JS.contains("workspace.windowAdded.connect"), "no registration on add");
        assert!(KWIN_MAIN_JS.contains("workspace.stackingOrder.forEach"), "no registration at load");
        let registration = KWIN_MAIN_JS
            .split("workspace.stackingOrder.forEach")
            .nth(1)
            .unwrap_or("");
        assert!(registration.contains("routeWindow"), "existing windows are watched but never reported");
    }

    #[test]
    fn the_embedded_kwin_script_is_the_one_in_the_repository() {
        // The copy inside the binary is what gets installed; if it drifts from the
        // source the panel installs code nobody has read.
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../kwin-script/monitor-audio/contents/code/main.js");
        assert_eq!(std::fs::read_to_string(&source).expect("source script present"), KWIN_MAIN_JS);
    }
}
