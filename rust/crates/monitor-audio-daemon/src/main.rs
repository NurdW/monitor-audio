//! The service: a D-Bus name, one timer, and the same command line the Python
//! version answered, so the control panel and the KWin script do not have to know
//! which implementation is running.

mod state;
mod system;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use monitor_audio_core::{BUS_NAME, BURST_INTERVAL_MS, IFACE, OBJ_PATH, RESCAN_SECONDS};
use state::State;
use system::{Pactl, Snapshot};

fn log(message: &str) {
    // The journal adds its own stamp, but `monitor-audio status` in a terminal and
    // the panel's Log tab read this output directly, so it carries its own.
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();
    let local = seconds % 86400;
    println!("[{:02}:{:02}:{:02}] {}", local / 3600, (local % 3600) / 60, local % 60, message,);
}

struct Service {
    state: Arc<Mutex<State>>,
}

#[zbus::interface(name = "local.MonitorAudio")]
impl Service {
    /// KWin's `callDBus` passes every argument as a string, so the signature stays
    /// seven strings even though two of them are numbers.
    // Eight arguments because the D-Bus signature is dictated by KWin's callDBus,
    // which passes every one of them as a string and cannot be given a struct.
    #[allow(clippy::too_many_arguments)]
    #[zbus(name = "Route")]
    async fn route(
        &self,
        pid: &str,
        _output: &str,
        resource_class: &str,
        desktop_file: &str,
        caption: &str,
        window: &str,
        model: &str,
    ) -> String {
        let state = self.state.clone();
        let request = state::RouteRequest {
            pid: pid.to_string(),
            resource_class: resource_class.to_string(),
            desktop_file: desktop_file.to_string(),
            caption: caption.to_string(),
            window: window.to_string(),
            model: model.to_string(),
        };
        // Every path through this shells out to pactl or pw-dump, so it goes on a
        // blocking thread rather than holding up the D-Bus task.
        tokio::task::spawn_blocking(move || {
            let snap = Snapshot::gather(&log);
            state
                .lock()
                .expect("state lock poisoned")
                .handle_route(&request, &snap, &Pactl, &log)
        })
        .await
        .unwrap_or_else(|err| format!("route failed: {}", err))
    }

    #[zbus(name = "Status")]
    async fn status(&self) -> String {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || {
            let snap = Snapshot::gather(&log);
            state.lock().expect("state lock poisoned").status_text(&snap)
        })
        .await
        .unwrap_or_else(|err| format!("status failed: {}", err))
    }

    #[zbus(name = "JsonStatus")]
    async fn json_status(&self) -> String {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || {
            let snap = Snapshot::gather(&log);
            state.lock().expect("state lock poisoned").report(&snap, true).to_string()
        })
        .await
        .unwrap_or_else(|err| format!("{{\"error\": \"{}\"}}", err))
    }

    #[zbus(name = "Reset")]
    async fn reset(&self) -> String {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || {
            state.lock().expect("state lock poisoned").reset(&log)
        })
        .await
        .unwrap_or_else(|err| format!("reset failed: {}", err))
    }

    /// Answer "which output would each of these displays use?" with the same code
    /// the routing itself uses, so the control panel never has to guess. The panel
    /// used to keep its own copy of those rules and its table disagreed with what
    /// was happening.
    #[zbus(name = "Resolve")]
    async fn resolve(&self, models_json: &str) -> String {
        let models_json = models_json.to_string();
        tokio::task::spawn_blocking(move || cli_resolve(&models_json))
            .await
            .unwrap_or_else(|err| format!("resolve failed: {}", err))
    }

    #[zbus(name = "Forget")]
    async fn forget(&self, window: &str) -> String {
        let state = self.state.clone();
        let window = window.to_string();
        tokio::task::spawn_blocking(move || {
            state.lock().expect("state lock poisoned").forget(&window, &log)
        })
        .await
        .unwrap_or_else(|err| format!("forget failed: {}", err))
    }
}

/// One pass, then re-arm. The wait is not constant - fast while a stream is
/// expected, calm otherwise - so the interval is chosen every time round rather
/// than fixed by a repeating timer.
async fn tick_loop(state: Arc<Mutex<State>>) {
    loop {
        let burst = state.lock().expect("state lock poisoned").burst_is_running();
        let delay = if burst {
            Duration::from_millis(BURST_INTERVAL_MS)
        } else {
            Duration::from_secs_f64(RESCAN_SECONDS)
        };
        tokio::time::sleep(delay).await;
        let state = state.clone();
        if let Err(err) = tokio::task::spawn_blocking(move || {
            let mut guard = state.lock().expect("state lock poisoned");
            guard.prune_dead(&log);
            let snap = Snapshot::gather(&log);
            if guard.apply_pending(&snap, &Pactl, &log) > 0 {
                // A player that opens one stream often opens another a moment
                // later, so stay quick rather than going straight back to calm.
                guard.start_burst("a stream was placed", &log);
            }
            guard.keepalive_tick(&snap, &log);
        })
        .await
        {
            log(&format!("the polling pass failed and will be retried: {}", err));
        }
    }
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(Mutex::new(State::default()));

    let terminate = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        match signal_hook::flag::register(signal, terminate.clone()) {
            Ok(_) => {}
            Err(err) => log(&format!("could not watch for signal {}: {}", signal, err)),
        }
    }

    let connection = zbus::Connection::session().await?;
    connection.request_name(BUS_NAME).await?;
    connection.object_server().at(OBJ_PATH, Service { state: state.clone() }).await?;
    log(&format!("serving {} at {}", BUS_NAME, OBJ_PATH));

    tokio::spawn(tick_loop(state.clone()));

    // Polling the flag rather than awaiting a signal stream keeps this free of an
    // extra runtime dependency; half a second to shut down costs nothing and the
    // only thing it delays is releasing the silent streams.
    while !terminate.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    state.lock().expect("state lock poisoned").stop_keepalive("service stopping", &log);
    log("stopped");
    Ok(())
}

async fn call<B>(method: &str, body: &B) -> Result<String, zbus::Error>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(&connection, BUS_NAME, OBJ_PATH, IFACE).await?;
    let reply = proxy.call_method(method, body).await?;
    reply.body().deserialize::<String>()
}

async fn cli(command: &str, argument: Option<String>) -> i32 {
    match command {
        "status" => match call("Status", &()).await {
            Ok(text) => {
                println!("{}", text);
                0
            }
            Err(err) => {
                println!("monitor-audio service is not reachable: {}", err);
                1
            }
        },
        "json" => match call("JsonStatus", &()).await {
            Ok(text) => {
                println!("{}", text);
                0
            }
            Err(err) => {
                // The panel still needs something to draw when the service is down.
                log(&format!("service not reachable ({}); reporting what is visible without it", err));
                let snap = Snapshot::gather(&log);
                println!("{}", State::default().report(&snap, false));
                0
            }
        },
        "reset" => match call("Reset", &()).await {
            Ok(text) => {
                println!("{}", text);
                0
            }
            Err(err) => {
                println!("monitor-audio service is not reachable: {}", err);
                1
            }
        },
        "forget" => match argument {
            Some(value) => match call("Forget", &value).await {
                Ok(text) => {
                    println!("{}", text);
                    0
                }
                Err(err) => {
                    println!("monitor-audio service is not reachable: {}", err);
                    1
                }
            },
            None => {
                println!("usage: monitor-audio forget <window or pid>");
                2
            }
        },
        "resolve" => {
            let models = argument.unwrap_or_else(|| "[]".to_string());
            println!("{}", cli_resolve(&models));
            0
        }
        other => {
            println!("unknown command {:?}; try status, json, reset, forget or resolve", other);
            2
        }
    }
}

/// Answer "which output would each of these displays use?" with the same code the
/// routing itself uses, so the control panel never has to guess.
fn cli_resolve(models_json: &str) -> String {
    let models: Vec<String> = match serde_json::from_str(models_json) {
        Ok(models) => models,
        Err(err) => {
            log(&format!("could not read the list of displays ({}): {}", models_json, err));
            return "{}".to_string();
        }
    };
    let snap = Snapshot::gather(&log);
    let descriptions: std::collections::HashMap<&str, &str> =
        snap.sinks.iter().map(|s| (s.name.as_str(), s.description.as_str())).collect();
    let mut answer = serde_json::Map::new();
    for model in models {
        let (output, how) = monitor_audio_core::routing::resolve_output(&model, &snap.sinks, &snap.config, &log);
        answer.insert(
            model,
            serde_json::json!({
                "output": output.clone(),
                "description": output.as_deref().and_then(|name| descriptions.get(name)).copied(),
                "how": how,
            }),
        );
    }
    serde_json::Value::Object(answer).to_string()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("");
    let code = match command {
        "" => match serve().await {
            Ok(()) => 0,
            Err(err) => {
                log(&format!("could not start: {}", err));
                1
            }
        },
        other => cli(other, args.get(2).cloned()).await,
    };
    std::process::exit(code);
}
