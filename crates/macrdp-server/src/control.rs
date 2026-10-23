//! `macrdp-server control`: the helper the macOS app runs. JSON Lines over stdin/stdout, see
//! docs/superpowers/specs/2026-10-17-swiftui-app-design.md ("Control protocol").
//!
//! Stdout carries protocol messages only. Nothing here runs a timer: with no session the
//! helper writes nothing until a command arrives.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use macrdp_core::{ConfigUpdate, ConnectionEvent, LogEntry, Metrics, PermissionStatus, ServerConfig, ServerEventHandler, ServerStatus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use zeroize::Zeroizing;

use crate::config_store::{self, Effect};

/// Largest protocol message, either direction.
const MAX_MESSAGE: usize = 64 * 1024;
/// Longest forwarded log message, so a log line always fits in a message.
const MAX_LOG_CHARS: usize = 4096;
/// At most two metrics messages per second.
const METRICS_INTERVAL: Duration = Duration::from_millis(500);
/// How long `test_standby` keeps the monitors asleep.
const STANDBY_TEST: Duration = Duration::from_secs(3);

/// What the server reports, funnelled into the control loop.
#[derive(Debug)]
pub enum Event {
    Status(ServerStatus),
    Metrics(Metrics),
    Connection(ConnectionEvent),
    Log(LogEntry),
    /// Physical displays the app covers for the curtain (empty: none).
    Shield(Vec<u32>),
}

/// The `ServerEventHandler` given to the server: forwards every event to the control loop.
pub struct EventSink(pub UnboundedSender<Event>);

impl ServerEventHandler for EventSink {
    fn on_status_change(&self, status: ServerStatus) {
        let _ = self.0.send(Event::Status(status));
    }
    fn on_metrics(&self, metrics: Metrics) {
        let _ = self.0.send(Event::Metrics(metrics));
    }
    fn on_connection(&self, event: ConnectionEvent) {
        let _ = self.0.send(Event::Connection(event));
    }
    fn on_log(&self, entry: LogEntry) {
        let _ = self.0.send(Event::Log(entry));
    }
    fn on_shield_displays(&self, displays: Vec<u32>) {
        let _ = self.0.send(Event::Shield(displays));
    }
}

/// Remote-login state. Never carries the password or hash.
#[derive(Debug, Clone, Serialize)]
pub struct AuthInfo {
    pub user: String,
    pub enrolled: bool,
    pub reenroll_needed: bool,
    /// SHA-256 of the TLS certificate (public).
    pub fingerprint: Option<String>,
}

/// A running server.
pub(crate) trait Server {
    fn port(&self) -> u16;
    fn update_config(&self, update: ConfigUpdate);
    fn disconnect(&self);
    async fn stop(&self) -> anyhow::Result<()>;
}

/// Everything the control loop does to the Mac, behind a trait so tests use a fake.
pub(crate) trait Backend {
    type Server: Server;
    async fn start(&self, config: ServerConfig, events: EventSink) -> anyhow::Result<Self::Server>;
    /// `config` names a custom TLS certificate, if any.
    fn auth(&self, config: &ServerConfig) -> AuthInfo;
    /// Verifies the Mac password and stores its NT hash; `Ok(false)` for a wrong password.
    fn enroll(&self, password: &str) -> anyhow::Result<bool>;
    fn permissions(&self) -> PermissionStatus;
    fn request_permission(&self, which: &str) -> anyhow::Result<()>;
    fn open_privacy_settings(&self, which: &str) -> anyhow::Result<()>;
    fn addresses(&self) -> Vec<String>;
    fn set_log_level(&self, level: &str);
    /// DDC standby (`on = false`) or wake for every external monitor; returns how many took it.
    fn monitor_power(&self, on: bool) -> usize;
}

/// A string zeroized on drop and redacted in Debug output.
struct Secret(Zeroizing<String>);

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(|s| Secret(Zeroizing::new(s)))
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// One command line. Unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct Command {
    #[serde(default)]
    id: Option<u64>,
    cmd: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    password: Option<Secret>,
    #[serde(default)]
    which: Option<String>,
}

/// The active session as the app sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Session {
    peer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    codec: Option<String>,
    display_mode: String,
    since: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Stopped,
    Starting,
    Listening,
    Connected,
    Error,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Stopped => "stopped",
            State::Starting => "starting",
            State::Listening => "listening",
            State::Connected => "connected",
            State::Error => "error",
        }
    }
}

pub(crate) struct Controller<B: Backend, W: Write> {
    backend: B,
    out: W,
    config_path: PathBuf,
    config: ServerConfig,
    /// Set when the config file exists but cannot be parsed: it is never overwritten.
    config_error: Option<String>,
    events: UnboundedSender<Event>,
    server: Option<B::Server>,
    state: State,
    error: Option<String>,
    session: Option<Session>,
    /// Physical displays the app keeps black for the curtain. Outlives the session it came
    /// with: it clears only when the display configuration is restored.
    shield: Vec<u32>,
    last_metrics: Option<Instant>,
}

impl<B: Backend, W: Write> Controller<B, W> {
    /// Loads the config; a missing file is created with the defaults (mode 0600).
    pub fn new(backend: B, out: W, config_path: PathBuf, events: UnboundedSender<Event>) -> Self {
        let (config, config_error) = match config_store::load(&config_path) {
            Ok(config) => (config, None),
            Err(e) => {
                tracing::error!(path = %config_path.display(), "Invalid config file, using defaults and leaving it untouched: {e:#}");
                (ServerConfig::default(), Some(format!("{} is invalid: {e}", config_path.display())))
            }
        };
        if config_error.is_none() && !config_path.exists() {
            if let Err(e) = config_store::save(&config_path, &config) {
                tracing::warn!(path = %config_path.display(), "Cannot create the config file: {e}");
            }
        }
        Self {
            backend,
            out,
            config_path,
            config,
            config_error,
            events,
            server: None,
            state: State::Stopped,
            error: None,
            session: None,
            shield: Vec::new(),
            last_metrics: None,
        }
    }

    /// Writes one message line. Oversize messages are dropped (logged to the file only).
    fn emit(&mut self, message: Value) {
        let Ok(line) = serde_json::to_string(&message) else { return };
        if line.len() > MAX_MESSAGE {
            tracing::warn!(len = line.len(), "Dropping an oversize control message");
            return;
        }
        let written = writeln!(self.out, "{line}").and_then(|()| self.out.flush());
        if let Err(e) = written {
            tracing::warn!("Cannot write to the app: {e}");
        }
    }

    /// `message` with `"type": kind` added; `message` must serialize to an object.
    fn emit_typed(&mut self, kind: &str, message: impl Serialize) {
        let mut value = serde_json::to_value(message).unwrap_or_else(|_| json!({}));
        if let Value::Object(map) = &mut value {
            map.insert("type".into(), kind.into());
        }
        self.emit(value);
    }

    fn reply(&mut self, id: Option<u64>, result: Result<(), String>, restart_required: Option<bool>) {
        let mut reply = json!({ "type": "reply", "id": id, "ok": result.is_ok() });
        if let Err(error) = result {
            reply["error"] = error.into();
        }
        if let Some(restart) = restart_required {
            reply["restart_required"] = restart.into();
        }
        self.emit(reply);
    }

    fn emit_status(&mut self) {
        let port = self.server.as_ref().map_or(self.config.port, Server::port);
        let mut status = json!({
            "type": "status",
            "running": self.server.is_some(),
            "state": self.state.as_str(),
            "port": port,
            "addresses": self.backend.addresses(),
        });
        if let Some(error) = &self.error {
            status["error"] = error.clone().into();
        }
        self.emit(status);
    }

    fn emit_session(&mut self) {
        let mut message = match &self.session {
            Some(session) => {
                let mut value = serde_json::to_value(session).unwrap_or_else(|_| json!({}));
                value["active"] = true.into();
                value
            }
            None => json!({ "active": false }),
        };
        message["curtain"] = (!self.shield.is_empty()).into();
        message["shield_displays"] = self.shield.clone().into();
        self.emit_typed("session", message);
    }

    fn emit_config(&mut self) {
        let config = self.config.clone();
        self.emit_typed("config", config);
    }

    fn emit_auth(&mut self) {
        let auth = self.backend.auth(&self.config);
        self.emit_typed("auth", auth);
    }

    fn emit_permissions(&mut self) {
        let permissions = self.backend.permissions();
        self.emit_typed("permissions", permissions);
    }

    /// Handles one command line and writes exactly one reply.
    pub async fn handle_line(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        if line.len() > MAX_MESSAGE {
            return self.reply(None, Err("message too large".into()), None);
        }
        // The error never echoes the line: it may hold a password.
        let Ok(mut command) = serde_json::from_str::<Command>(line) else {
            return self.reply(None, Err("invalid command".into()), None);
        };
        let id = command.id;
        match command.cmd.as_str() {
            "start" => {
                let result = self.start().await;
                self.reply(id, result, None);
            }
            "stop" => {
                self.stop().await;
                self.reply(id, Ok(()), None);
            }
            "get_state" => {
                self.emit_status();
                self.emit_auth();
                self.emit_config();
                self.emit_permissions();
                self.emit_session();
                self.reply(id, Ok(()), None);
            }
            "set_config" => {
                let (Some(key), Some(value)) = (command.key.as_deref(), command.value.as_ref()) else {
                    return self.reply(id, Err("set_config needs key and value".into()), None);
                };
                match self.set_config(key, value) {
                    Ok(restart) => self.reply(id, Ok(()), Some(restart)),
                    Err(e) => self.reply(id, Err(e), Some(false)),
                }
            }
            "enroll" => {
                let password = command.password.take();
                let result = match password {
                    None => Err("enroll needs password".to_string()),
                    Some(password) => match self.backend.enroll(&password.0) {
                        Ok(true) => Ok(()),
                        Ok(false) => Err("wrong password".to_string()),
                        Err(e) => {
                            tracing::warn!("Enrollment failed: {e:#}");
                            Err("enrollment failed".to_string())
                        }
                    },
                };
                if result.is_ok() {
                    self.emit_auth();
                }
                self.reply(id, result, None);
            }
            "disconnect" => {
                if let Some(server) = &self.server {
                    server.disconnect();
                }
                self.reply(id, Ok(()), None);
            }
            "request_permissions" => {
                let result = self.permission(command.which.as_deref(), |b, w| b.request_permission(w));
                if result.is_ok() {
                    self.emit_permissions();
                }
                self.reply(id, result, None);
            }
            "open_privacy_settings" => {
                let result = self.permission(command.which.as_deref(), |b, w| b.open_privacy_settings(w));
                self.reply(id, result, None);
            }
            "test_standby" => {
                // A curtain session's monitors stay as the curtain left them.
                if self.session.is_some() {
                    return self.reply(id, Err("cannot test monitor standby during a session".into()), None);
                }
                let monitors = self.backend.monitor_power(false);
                tokio::time::sleep(STANDBY_TEST).await;
                self.backend.monitor_power(true);
                self.emit(json!({ "type": "reply", "id": id, "ok": true, "monitors": monitors }));
            }
            _ => self.reply(id, Err("unknown command".into()), None),
        }
    }

    fn permission(&self, which: Option<&str>, action: impl Fn(&B, &str) -> anyhow::Result<()>) -> Result<(), String> {
        match which {
            Some(which @ ("screen" | "accessibility")) => action(&self.backend, which).map_err(|e| e.to_string()),
            _ => Err("which must be \"screen\" or \"accessibility\"".into()),
        }
    }

    async fn start(&mut self) -> Result<(), String> {
        if self.server.is_some() {
            return Ok(());
        }
        self.state = State::Starting;
        self.error = None;
        self.emit_status();
        let result = self.backend.start(self.config.clone(), EventSink(self.events.clone())).await;
        match result {
            Ok(server) => {
                self.server = Some(server);
                self.state = State::Listening;
                self.emit_status();
                Ok(())
            }
            Err(e) => {
                let error = format!("{e:#}");
                tracing::error!("Cannot start the server: {error}");
                self.state = State::Error;
                self.error = Some(error.clone());
                self.emit_status();
                Err(error)
            }
        }
    }

    /// Stops the server; the session teardown restores the Mac's displays.
    async fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            if let Err(e) = server.stop().await {
                tracing::warn!("Server stop: {e:#}");
            }
        }
        self.state = State::Stopped;
        self.error = None;
        self.last_metrics = None;
        if self.session.take().is_some() {
            self.emit_session();
        }
        self.emit_status();
    }

    /// Returns whether the change needs a server restart.
    fn set_config(&mut self, key: &str, value: &Value) -> Result<bool, String> {
        if let Some(error) = &self.config_error {
            return Err(error.clone());
        }
        let mut config = self.config.clone();
        let effect = config_store::apply(&mut config, key, value)?;
        config_store::save(&self.config_path, &config).map_err(|e| format!("cannot save the config: {e}"))?;
        self.config = config;
        let restart = match effect {
            Effect::Hot(update) => {
                if let ConfigUpdate::LogLevel(level) = &update {
                    self.backend.set_log_level(level);
                }
                if let Some(server) = &self.server {
                    server.update_config(update);
                }
                false
            }
            Effect::Restart => self.server.is_some(),
        };
        self.emit_config();
        Ok(restart)
    }

    pub async fn handle_event(&mut self, event: Event) {
        match event {
            Event::Status(status) if status.state.starts_with("error") => {
                // The server thread died: drop it so `start` works again.
                let error = status.state.trim_start_matches("error:").trim().to_string();
                self.stop().await;
                self.state = State::Error;
                self.error = Some(error);
                self.emit_status();
            }
            Event::Status(_) => {}
            Event::Connection(ConnectionEvent::Connected(info)) => {
                if self.server.is_none() {
                    return;
                }
                self.session = Some(Session {
                    peer: info.client_ip,
                    client_name: None,
                    width: None,
                    height: None,
                    codec: None,
                    display_mode: self.config.display_mode.clone().unwrap_or_else(|| "curtain".into()),
                    since: info.connected_at,
                });
                self.last_metrics = None;
                self.state = State::Connected;
                self.emit_session();
                self.emit_status();
            }
            Event::Connection(ConnectionEvent::Disconnected(_)) => {
                if self.session.take().is_none() {
                    return;
                }
                if self.server.is_some() {
                    self.state = State::Listening;
                }
                self.emit_session();
                self.emit_status();
            }
            Event::Metrics(metrics) => {
                let Some(session) = self.session.as_mut() else { return };
                let before = session.clone();
                session.client_name = metrics.client_name.clone().filter(|n| !n.is_empty());
                session.width = Some(metrics.width).filter(|w| *w > 0);
                session.height = Some(metrics.height).filter(|h| *h > 0);
                session.codec = metrics.codec.clone();
                if *session != before {
                    self.emit_session();
                }
                if self.last_metrics.is_some_and(|t| t.elapsed() < METRICS_INTERVAL) {
                    return;
                }
                self.last_metrics = Some(Instant::now());
                self.emit_typed("metrics", metrics);
            }
            Event::Log(entry) => self.emit_typed("log", entry),
            Event::Shield(displays) => {
                if displays != self.shield {
                    self.shield = displays;
                    self.emit_session();
                }
            }
        }
    }

    pub async fn shutdown(&mut self) {
        self.stop().await;
    }
}

/// Serves commands until stdin closes or `shutdown` resolves, then stops the server.
pub(crate) async fn run<B, W, R>(
    controller: &mut Controller<B, W>,
    input: R,
    mut events: UnboundedReceiver<Event>,
    shutdown: impl std::future::Future<Output = ()>,
) where
    B: Backend,
    W: Write,
    R: tokio::io::AsyncBufRead + Unpin,
{
    tokio::pin!(shutdown);
    let mut lines = input.lines();
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    let line = Zeroizing::new(line);
                    controller.handle_line(&line).await;
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("Cannot read commands: {e}");
                    break;
                }
            },
            Some(event) = events.recv() => controller.handle_event(event).await,
            () = &mut shutdown => break,
        }
    }
    controller.shutdown().await;
}

/// Forwards log events at or above a level to the app as `log` messages.
pub struct LogForwarder {
    pub events: UnboundedSender<Event>,
    /// Most verbose level forwarded, as [`level_rank`].
    pub threshold: Arc<AtomicU8>,
}

/// error 1, warn 2, info 3, debug 4, trace 5.
pub fn level_rank(level: &str) -> u8 {
    match level {
        "error" => 1,
        "warn" => 2,
        "info" => 3,
        "debug" => 4,
        _ => 5,
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogForwarder {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let level = event.metadata().level().as_str().to_ascii_lowercase();
        if level_rank(&level) > self.threshold.load(Ordering::Relaxed) {
            return;
        }
        let mut message = MessageText::default();
        event.record(&mut message);
        let _ = self.events.send(Event::Log(LogEntry {
            level,
            message: message.0.chars().take(MAX_LOG_CHARS).collect(),
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        }));
    }
}

/// The event's message followed by its fields.
#[derive(Default)]
struct MessageText(String);

impl tracing::field::Visit for MessageText {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

/// Applies a new log level to the log file and the forwarded `log` messages.
pub(crate) type SetLogLevel = Box<dyn Fn(&str)>;

/// The real Mac: macrdp-core, OpenDirectory, the Keychain and TCC.
pub(crate) struct MacBackend {
    pub set_log_level: SetLogLevel,
}

impl Server for Arc<macrdp_core::ServerHandle> {
    fn port(&self) -> u16 {
        macrdp_core::ServerHandle::port(self)
    }
    fn update_config(&self, update: ConfigUpdate) {
        macrdp_core::ServerHandle::update_config(self, update);
    }
    fn disconnect(&self) {
        macrdp_core::ServerHandle::disconnect(self);
    }
    async fn stop(&self) -> anyhow::Result<()> {
        macrdp_core::ServerHandle::stop(self).await
    }
}

impl Backend for MacBackend {
    type Server = Arc<macrdp_core::ServerHandle>;

    async fn start(&self, config: ServerConfig, events: EventSink) -> anyhow::Result<Self::Server> {
        macrdp_core::start_server(config, events).await
    }

    fn auth(&self, config: &ServerConfig) -> AuthInfo {
        use macrdp_core::auth::{self, AccountStore as _};
        let user = auth::current_user().unwrap_or_default();
        let enrolled = auth::SystemAccountStore
            .load_hash(&user)
            .map_err(|e| tracing::warn!("Keychain read: {e:#}"))
            .is_ok_and(|hash| hash.is_some());
        let fingerprint = macrdp_core::tls::ensure_tls_files(config.cert_path.as_deref(), config.key_path.as_deref())
            .and_then(|(cert, _)| macrdp_core::tls::cert_fingerprint(&cert))
            .map_err(|e| tracing::warn!("TLS fingerprint unavailable: {e:#}"))
            .ok();
        AuthInfo { user, enrolled, reenroll_needed: auth::REENROLL_NEEDED.load(Ordering::Relaxed), fingerprint }
    }

    fn enroll(&self, password: &str) -> anyhow::Result<bool> {
        use macrdp_core::auth;
        auth::enroll(&auth::SystemAccountStore, &auth::current_user()?, password)
    }

    fn permissions(&self) -> PermissionStatus {
        macrdp_core::permissions::check_permissions()
    }

    fn request_permission(&self, which: &str) -> anyhow::Result<()> {
        macrdp_core::permissions::request_permission(which)
    }

    fn open_privacy_settings(&self, which: &str) -> anyhow::Result<()> {
        macrdp_core::permissions::open_privacy_settings(which)
    }

    fn addresses(&self) -> Vec<String> {
        macrdp_core::tls::ipv4_addresses()
    }

    fn set_log_level(&self, level: &str) {
        (self.set_log_level)(level);
    }

    fn monitor_power(&self, on: bool) -> usize {
        macrdp_core::set_external_power(on)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Calls {
        started: usize,
        stopped: usize,
        disconnects: usize,
        updates: Vec<String>,
        passwords: Vec<String>,
        log_levels: Vec<String>,
        permission_calls: Vec<String>,
        /// Monitor power writes (true = wake).
        power: Vec<bool>,
    }

    #[derive(Clone, Default)]
    struct Fake {
        calls: Arc<Mutex<Calls>>,
        fail_start: bool,
        /// External monitors that accept a DDC write.
        monitors: usize,
    }

    struct FakeServer(Arc<Mutex<Calls>>);

    impl Server for FakeServer {
        fn port(&self) -> u16 {
            3389
        }
        fn update_config(&self, update: ConfigUpdate) {
            self.0.lock().unwrap().updates.push(format!("{update:?}"));
        }
        fn disconnect(&self) {
            self.0.lock().unwrap().disconnects += 1;
        }
        async fn stop(&self) -> anyhow::Result<()> {
            self.0.lock().unwrap().stopped += 1;
            Ok(())
        }
    }

    impl Backend for Fake {
        type Server = FakeServer;
        async fn start(&self, _config: ServerConfig, _events: EventSink) -> anyhow::Result<FakeServer> {
            if self.fail_start {
                anyhow::bail!("Cannot listen on 0.0.0.0:3389");
            }
            self.calls.lock().unwrap().started += 1;
            Ok(FakeServer(Arc::clone(&self.calls)))
        }
        fn auth(&self, _config: &ServerConfig) -> AuthInfo {
            AuthInfo { user: "pipo".into(), enrolled: true, reenroll_needed: false, fingerprint: Some("AB:CD".into()) }
        }
        fn enroll(&self, password: &str) -> anyhow::Result<bool> {
            self.calls.lock().unwrap().passwords.push(password.to_string());
            match password {
                "right horse battery" => Ok(true),
                "keychain breaks" => anyhow::bail!("Keychain write failed: -25308"),
                _ => Ok(false),
            }
        }
        fn permissions(&self) -> PermissionStatus {
            PermissionStatus { screen_capture: true, accessibility: false, microphone: false }
        }
        fn request_permission(&self, which: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().permission_calls.push(format!("request {which}"));
            Ok(())
        }
        fn open_privacy_settings(&self, which: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().permission_calls.push(format!("open {which}"));
            Ok(())
        }
        fn addresses(&self) -> Vec<String> {
            vec!["192.168.1.6".into(), "10.8.0.2".into()]
        }
        fn set_log_level(&self, level: &str) {
            self.calls.lock().unwrap().log_levels.push(level.into());
        }
        fn monitor_power(&self, on: bool) -> usize {
            self.calls.lock().unwrap().power.push(on);
            self.monitors
        }
    }

    /// Output shared with the test while `run` owns the controller.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    struct Rig {
        ctrl: Controller<Fake, Shared>,
        out: Shared,
        calls: Arc<Mutex<Calls>>,
        _events: UnboundedReceiver<Event>,
        dir: tempfile::TempDir,
    }

    fn rig_with(fake: Fake) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let out = Shared::default();
        let calls = Arc::clone(&fake.calls);
        let ctrl = Controller::new(fake, out.clone(), dir.path().join("config.toml"), tx);
        Rig { ctrl, out, calls, _events: rx, dir }
    }

    fn rig() -> Rig {
        rig_with(Fake::default())
    }

    fn messages(bytes: &[u8]) -> Vec<Value> {
        std::str::from_utf8(bytes).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    impl Rig {
        fn messages(&self) -> Vec<Value> {
            messages(&self.out.bytes())
        }
        fn of_type(&self, kind: &str) -> Vec<Value> {
            self.messages().into_iter().filter(|m| m["type"] == kind).collect()
        }
        fn last_reply(&self) -> Value {
            self.of_type("reply").pop().expect("a reply")
        }
        fn clear(&self) {
            self.out.0.lock().unwrap().clear();
        }
    }

    #[tokio::test]
    async fn missing_config_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let r = rig();
        let path = r.dir.path().join("config.toml");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn invalid_config_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = \"broken\"").unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let out = Shared::default();
        let mut ctrl = Controller::new(Fake::default(), out.clone(), path.clone(), tx);
        ctrl.handle_line(r#"{"id":1,"cmd":"set_config","key":"frame_rate","value":30}"#).await;
        let reply = messages(&out.bytes()).pop().unwrap();
        assert_eq!(reply["ok"], false);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "port = \"broken\"");
    }

    #[tokio::test]
    async fn unknown_command_gets_ok_false_with_its_id() {
        let mut r = rig();
        r.ctrl.handle_line(r#"{"id":7,"cmd":"fly","extra":1}"#).await;
        let reply = r.last_reply();
        assert_eq!((reply["id"].clone(), reply["ok"].clone()), (json!(7), json!(false)));
        assert!(reply["error"].is_string());
    }

    #[tokio::test]
    async fn malformed_line_gets_ok_false_without_echo() {
        let mut r = rig();
        r.ctrl.handle_line(r#"{"id":"x","cmd":"enroll","password":"hunter2-secret"}"#).await;
        r.ctrl.handle_line(r#"{"cmd":"enroll","password":"hunter2-secret""#).await;
        let replies = r.of_type("reply");
        assert_eq!(replies.len(), 2);
        assert!(replies.iter().all(|m| m["ok"] == false && m["id"].is_null()));
        assert!(!String::from_utf8(r.out.bytes()).unwrap().contains("hunter2"));
    }

    #[tokio::test]
    async fn every_command_gets_exactly_one_reply_with_its_id() {
        let mut r = rig();
        let commands = [
            r#"{"id":1,"cmd":"start"}"#,
            r#"{"id":2,"cmd":"start"}"#,
            r#"{"id":3,"cmd":"get_state"}"#,
            r#"{"id":4,"cmd":"disconnect"}"#,
            r#"{"id":5,"cmd":"request_permissions","which":"screen"}"#,
            r#"{"id":6,"cmd":"open_privacy_settings","which":"accessibility"}"#,
            r#"{"id":7,"cmd":"request_permissions","which":"camera"}"#,
            r#"{"id":8,"cmd":"set_config","key":"frame_rate"}"#,
            r#"{"id":9,"cmd":"enroll"}"#,
            r#"{"id":10,"cmd":"stop"}"#,
        ];
        for c in commands {
            r.ctrl.handle_line(c).await;
        }
        let replies = r.of_type("reply");
        let ids: Vec<_> = replies.iter().map(|m| m["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, (1..=10).collect::<Vec<_>>());
        let ok: Vec<_> = replies.iter().map(|m| m["ok"].as_bool().unwrap()).collect();
        assert_eq!(ok, [true, true, true, true, true, true, false, false, false, true]);
        let calls = r.calls.lock().unwrap();
        assert_eq!((calls.started, calls.stopped, calls.disconnects), (1, 1, 1), "start is a no-op when running");
        assert_eq!(calls.permission_calls, ["request screen", "open accessibility"]);
    }

    #[tokio::test]
    async fn get_state_emits_every_state_message() {
        let mut r = rig();
        r.ctrl.handle_line(r#"{"id":1,"cmd":"get_state"}"#).await;
        let kinds: Vec<_> = r.messages().iter().map(|m| m["type"].as_str().unwrap().to_string()).collect();
        assert_eq!(kinds, ["status", "auth", "config", "permissions", "session", "reply"]);
        let status = &r.of_type("status")[0];
        assert_eq!(status["state"], "stopped");
        assert_eq!(status["port"], 3389);
        assert_eq!(status["addresses"], json!(["192.168.1.6", "10.8.0.2"]));
        assert_eq!(r.of_type("auth")[0]["fingerprint"], "AB:CD");
        assert_eq!(r.of_type("config")[0]["frame_rate"], 60);
        assert_eq!(r.of_type("session")[0]["active"], false);
    }

    #[tokio::test]
    async fn failed_start_reports_error_state() {
        let mut r = rig_with(Fake { fail_start: true, ..Fake::default() });
        r.ctrl.handle_line(r#"{"id":1,"cmd":"start"}"#).await;
        let states: Vec<_> = r.of_type("status").iter().map(|m| m["state"].as_str().unwrap().to_string()).collect();
        assert_eq!(states, ["starting", "error"]);
        assert!(r.of_type("status")[1]["error"].as_str().unwrap().contains("3389"));
        assert_eq!(r.last_reply()["ok"], false);
    }

    #[tokio::test]
    async fn set_config_validates_persists_and_hot_applies() {
        let mut r = rig();
        let path = r.dir.path().join("config.toml");
        r.ctrl.handle_line(r#"{"id":1,"cmd":"start"}"#).await;

        r.ctrl.handle_line(r#"{"id":2,"cmd":"set_config","key":"frame_rate","value":30}"#).await;
        let reply = r.last_reply();
        assert_eq!((reply["ok"].clone(), reply["restart_required"].clone()), (json!(true), json!(false)));
        assert!(std::fs::read_to_string(&path).unwrap().contains("frame_rate = 30"));
        assert_eq!(r.of_type("config").pop().unwrap()["frame_rate"], 30);

        let before = std::fs::read_to_string(&path).unwrap();
        r.ctrl.handle_line(r#"{"id":3,"cmd":"set_config","key":"port","value":80}"#).await;
        assert_eq!(r.last_reply()["ok"], false);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "an invalid value is not persisted");

        r.ctrl.handle_line(r#"{"id":4,"cmd":"set_config","key":"port","value":3390}"#).await;
        assert_eq!(r.last_reply()["restart_required"], true);

        r.ctrl.handle_line(r#"{"id":5,"cmd":"set_config","key":"log_level","value":"debug"}"#).await;
        assert_eq!(r.last_reply()["restart_required"], false);

        r.ctrl.handle_line(r#"{"id":6,"cmd":"set_config","key":"nope","value":1}"#).await;
        assert_eq!(r.last_reply()["ok"], false);

        let calls = r.calls.lock().unwrap();
        assert_eq!(calls.updates, ["FrameRate(30)", "LogLevel(\"debug\")"]);
        assert_eq!(calls.log_levels, ["debug"]);
    }

    #[tokio::test]
    async fn restart_keys_need_no_restart_when_stopped() {
        let mut r = rig();
        r.ctrl.handle_line(r#"{"id":1,"cmd":"set_config","key":"port","value":3390}"#).await;
        assert_eq!(r.last_reply()["restart_required"], false);
        assert!(r.calls.lock().unwrap().updates.is_empty());
    }

    #[tokio::test]
    async fn enroll_never_echoes_the_password() {
        let mut r = rig();
        for (id, password) in [(1, "right horse battery"), (2, "wrong-p4ss\"word"), (3, "keychain breaks")] {
            let line = json!({ "id": id, "cmd": "enroll", "password": password }).to_string();
            r.ctrl.handle_line(&line).await;
        }
        let output = r.out.bytes();
        for secret in ["right horse battery", "wrong-p4ss", "keychain breaks"] {
            assert!(!output.windows(secret.len()).any(|w| w == secret.as_bytes()), "{secret} leaked");
        }
        let ok: Vec<_> = r.of_type("reply").iter().map(|m| m["ok"].as_bool().unwrap()).collect();
        assert_eq!(ok, [true, false, false]);
        assert_eq!(r.of_type("auth").len(), 1, "auth is re-sent after a successful enroll");
        assert_eq!(r.calls.lock().unwrap().passwords, ["right horse battery", "wrong-p4ss\"word", "keychain breaks"]);
    }

    #[tokio::test(start_paused = true)]
    async fn test_standby_sleeps_waits_three_seconds_then_wakes() {
        let mut r = rig_with(Fake { monitors: 2, ..Fake::default() });
        let started = tokio::time::Instant::now();
        r.ctrl.handle_line(r#"{"id":1,"cmd":"test_standby"}"#).await;
        assert_eq!(started.elapsed(), Duration::from_secs(3));
        let reply = r.last_reply();
        assert_eq!((reply["id"].clone(), reply["ok"].clone(), reply["monitors"].clone()), (json!(1), json!(true), json!(2)));
        assert_eq!(r.calls.lock().unwrap().power, [false, true], "sleep, then always wake");
    }

    #[tokio::test(start_paused = true)]
    async fn test_standby_reports_zero_monitors_and_still_wakes() {
        let mut r = rig();
        r.ctrl.handle_line(r#"{"id":1,"cmd":"test_standby"}"#).await;
        assert_eq!((r.last_reply()["ok"].clone(), r.last_reply()["monitors"].clone()), (json!(true), json!(0)));
        assert_eq!(r.calls.lock().unwrap().power.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn test_standby_is_refused_during_a_session() {
        let mut r = rig_with(Fake { monitors: 1, ..Fake::default() });
        r.ctrl.handle_line(r#"{"id":1,"cmd":"start"}"#).await;
        r.ctrl.handle_event(connected()).await;
        r.ctrl.handle_line(r#"{"id":2,"cmd":"test_standby"}"#).await;
        assert_eq!(r.last_reply()["ok"], false);
        assert!(r.calls.lock().unwrap().power.is_empty(), "a curtain's monitors are never touched");
    }

    fn connected() -> Event {
        Event::Connection(ConnectionEvent::Connected(macrdp_core::ConnectionInfo {
            client_ip: "10.8.0.3".into(),
            client_name: String::new(),
            connected_at: "2026-10-17T10:00:00Z".into(),
        }))
    }

    fn disconnected() -> Event {
        Event::Connection(ConnectionEvent::Disconnected(macrdp_core::DisconnectionInfo {
            client_ip: "10.8.0.3".into(),
            client_name: String::new(),
            duration_secs: 5,
            bytes_total: 0,
        }))
    }

    fn session_metrics() -> Event {
        Event::Metrics(Metrics {
            fps: 30,
            client_name: Some("PIXEL".into()),
            width: 2400,
            height: 1080,
            codec: Some("avc420".into()),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn session_lifecycle_is_reported() {
        let mut r = rig();
        r.ctrl.handle_line(r#"{"id":1,"cmd":"set_config","key":"display_mode","value":"mirror"}"#).await;
        r.ctrl.handle_line(r#"{"id":2,"cmd":"start"}"#).await;
        r.clear();

        r.ctrl.handle_event(connected()).await;
        let session = r.of_type("session").pop().unwrap();
        assert_eq!(session["active"], true);
        assert_eq!(session["peer"], "10.8.0.3");
        assert_eq!(session["display_mode"], "mirror");
        assert_eq!(session["since"], "2026-10-17T10:00:00Z");
        assert_eq!(r.of_type("status").pop().unwrap()["state"], "connected");

        r.ctrl.handle_event(session_metrics()).await;
        let session = r.of_type("session").pop().unwrap();
        assert_eq!((session["width"].clone(), session["height"].clone()), (json!(2400), json!(1080)));
        assert_eq!(session["codec"], "avc420");
        assert_eq!(session["client_name"], "PIXEL");
        let metrics = r.of_type("metrics");
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0]["fps"], 30);
        assert!(metrics[0].get("width").is_none(), "session details stay out of metrics");
        assert!(metrics[0].get("rfx_quality").is_none(), "H.264 sessions have no RemoteFX quality");

        r.ctrl.handle_event(session_metrics()).await;
        assert_eq!(r.of_type("metrics").len(), 1, "at most two metrics per second");
        assert_eq!(r.of_type("session").len(), 2, "unchanged details are not re-sent");

        r.ctrl.handle_event(disconnected()).await;
        assert_eq!(
            r.of_type("session").pop().unwrap(),
            json!({ "type": "session", "active": false, "curtain": false, "shield_displays": [] })
        );
        assert_eq!(r.of_type("status").pop().unwrap()["state"], "listening");
    }

    #[tokio::test]
    async fn the_curtain_shield_list_reaches_the_app_and_clears() {
        let mut r = rig();
        r.ctrl.handle_line(r#"{"id":1,"cmd":"start"}"#).await;
        r.ctrl.handle_event(connected()).await;
        let session = r.of_type("session").pop().unwrap();
        assert_eq!((session["curtain"].clone(), session["shield_displays"].clone()), (json!(false), json!([])));
        r.clear();

        r.ctrl.handle_event(Event::Shield(vec![1, 4])).await;
        let session = r.of_type("session").pop().unwrap();
        assert_eq!(session["active"], true);
        assert_eq!((session["curtain"].clone(), session["shield_displays"].clone()), (json!(true), json!([1, 4])));

        r.ctrl.handle_event(Event::Shield(vec![1, 4])).await;
        assert_eq!(r.of_type("session").len(), 1, "an unchanged list is not re-sent");

        // The session ends first; the shield goes when the display configuration is restored.
        r.ctrl.handle_event(disconnected()).await;
        assert_eq!(r.of_type("session").pop().unwrap()["shield_displays"], json!([1, 4]));
        r.ctrl.handle_event(Event::Shield(Vec::new())).await;
        assert_eq!(
            r.of_type("session").pop().unwrap(),
            json!({ "type": "session", "active": false, "curtain": false, "shield_displays": [] })
        );
    }

    #[tokio::test]
    async fn metrics_carry_the_remotefx_quality_in_effect() {
        let mut r = rig();
        r.ctrl.handle_line(r#"{"id":1,"cmd":"start"}"#).await;
        r.ctrl.handle_event(connected()).await;
        r.clear();
        let rfx = Metrics { fps: 30, codec: Some("rfx-progressive".into()), rfx_quality: Some("low".into()), ..Default::default() };
        r.ctrl.handle_event(Event::Metrics(rfx)).await;
        assert_eq!(r.of_type("metrics")[0]["rfx_quality"], "low");
    }

    #[tokio::test]
    async fn metrics_without_a_session_are_dropped() {
        let mut r = rig();
        r.ctrl.handle_line(r#"{"id":1,"cmd":"start"}"#).await;
        r.clear();
        r.ctrl.handle_event(session_metrics()).await;
        r.ctrl.handle_event(disconnected()).await;
        assert!(r.out.bytes().is_empty());
    }

    #[tokio::test]
    async fn server_error_drops_the_server_so_start_works_again() {
        let mut r = rig();
        r.ctrl.handle_line(r#"{"id":1,"cmd":"start"}"#).await;
        r.ctrl
            .handle_event(Event::Status(ServerStatus { running: false, state: "error: TLS".into(), uptime_secs: 0 }))
            .await;
        let status = r.of_type("status").pop().unwrap();
        assert_eq!((status["state"].clone(), status["running"].clone()), (json!("error"), json!(false)));
        r.ctrl.handle_line(r#"{"id":2,"cmd":"start"}"#).await;
        assert_eq!(r.calls.lock().unwrap().started, 2);
    }

    #[tokio::test]
    async fn quiet_when_idle_and_stops_on_eof() {
        let r = rig();
        let (out, calls, mut ctrl) = (r.out.clone(), Arc::clone(&r.calls), r.ctrl);
        let (mut app, helper) = tokio::io::duplex(4096);
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            run(&mut ctrl, tokio::io::BufReader::new(helper), rx, std::future::pending()).await;
        });
        use tokio::io::AsyncWriteExt;
        app.write_all(b"{\"id\":1,\"cmd\":\"start\"}\n").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let after_start = out.bytes().len();
        assert!(after_start > 0);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(out.bytes().len(), after_start, "no output with no session and no command");

        drop(app);
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
        assert_eq!(calls.lock().unwrap().stopped, 1, "stdin EOF stops the server");
    }

    #[tokio::test]
    async fn shutdown_signal_stops_the_server() {
        let r = rig();
        let (calls, mut ctrl) = (Arc::clone(&r.calls), r.ctrl);
        ctrl.handle_line(r#"{"id":1,"cmd":"start"}"#).await;
        let (_app, helper) = tokio::io::duplex(64);
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        run(&mut ctrl, tokio::io::BufReader::new(helper), rx, std::future::ready(())).await;
        assert_eq!(calls.lock().unwrap().stopped, 1);
    }

    #[test]
    fn log_forwarder_respects_the_threshold() {
        use tracing_subscriber::layer::SubscriberExt;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let threshold = Arc::new(AtomicU8::new(level_rank("warn")));
        let subscriber = tracing_subscriber::registry().with(LogForwarder { events: tx, threshold: Arc::clone(&threshold) });
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("quiet");
            tracing::warn!(port = 3389, "loud");
            threshold.store(level_rank("info"), Ordering::Relaxed);
            tracing::info!("now heard");
        });
        let mut got = Vec::new();
        while let Ok(Event::Log(entry)) = rx.try_recv() {
            got.push((entry.level, entry.message));
        }
        assert_eq!(got, [("warn".to_string(), "loud port=3389".to_string()), ("info".to_string(), "now heard".to_string())]);
    }
}
