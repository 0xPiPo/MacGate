//! RDP server lifecycle: start, stop, metrics, connections

/// Parse "WxH" resolution string, e.g. "3840x2160" → Some((3840, 2160))
fn parse_resolution(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Resolve resolution config value to (width, height).
///
/// Supports three formats:
/// - "auto": detect display scale and multiply logical resolution
/// - "WxH" (e.g. "3840x2160"): use directly
/// - legacy numeric scale (e.g. "2"): multiply logical resolution by scale factor
pub fn resolve_resolution(
    res_mode: &str,
    logical_w: u16,
    logical_h: u16,
) -> (u16, u16) {
    // u32 math, then clamped: a config value can never overflow u16 or exceed 8192.
    let (w, h) = if res_mode == "auto" {
        let scale = macrdp_capture::detect_display_scale().unwrap_or(1);
        tracing::info!(scale, logical_w, logical_h, "Resolution auto: display scale");
        (logical_w as u32 * scale, logical_h as u32 * scale)
    } else if let Some((w, h)) = parse_resolution(res_mode) {
        tracing::info!(w, h, "Resolution: explicit WxH");
        (w, h)
    } else if let Ok(scale) = res_mode.parse::<u32>() {
        // Legacy hidpi_scale: 1, 2, 3, 4 → multiply logical resolution
        let scale = scale.max(1).min(4);
        let (w, h) = (logical_w as u32 * scale, logical_h as u32 * scale);
        tracing::info!(scale, w, h, logical_w, logical_h, "Resolution: legacy hidpi_scale → {}x{}", w, h);
        (w, h)
    } else {
        tracing::warn!(res_mode, "Resolution: unrecognized value, using logical resolution");
        (logical_w as u32, logical_h as u32)
    };
    crate::resolution::clamp_desktop_size(w, h)
}

use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Instant;

use anyhow::{Context, Result};
use ironrdp_server::gfx::GfxState;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::callbacks::*;
use crate::config::ServerConfig;
use crate::permissions;
use crate::tls;

/// Handle to a running RDP server. Use to query state and stop the server.
pub struct ServerHandle {
    port: u16,
    started_at: Instant,
    gfx_state: Arc<Mutex<GfxState>>,
    shutdown_notify: Arc<Notify>,
    stopped: AtomicBool,
    /// The OS thread running the RDP server (RdpServer is !Send, needs dedicated thread)
    server_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    metrics_task: Mutex<Option<JoinHandle<()>>>,
    config_tx: tokio::sync::mpsc::UnboundedSender<ConfigUpdate>,
    session_events: SessionEvents,
}

impl ServerHandle {
    /// The TCP port the server is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Current server status.
    pub fn status(&self) -> ServerStatus {
        if self.stopped.load(Ordering::Relaxed) {
            ServerStatus {
                running: false,
                state: "stopped".to_string(),
                uptime_secs: 0,
            }
        } else {
            ServerStatus {
                running: true,
                state: "running".to_string(),
                uptime_secs: self.started_at.elapsed().as_secs(),
            }
        }
    }

    /// Current performance metrics.
    pub fn metrics(&self) -> Metrics {
        let gfx = self.gfx_state.lock().unwrap();
        let net_ms = (gfx.rtt_ewma_ms - gfx.last_encode_ms).max(0.0);
        Metrics {
            fps: 0, // Actual FPS comes from config/runtime
            bitrate_kbps: 0,
            rtt_ms: gfx.rtt_ewma_ms,
            bytes_sent: gfx.total_bytes_sent,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            encode_ms: gfx.last_encode_ms,
            net_ms,
            last_frame_bytes: gfx.last_frame_bytes,
            network_quality: gfx.network_quality,
            pending_acks: gfx.pending_acks,
            ..Default::default()
        }
    }

    /// Push a hot config update to the running server.
    pub fn update_config(&self, update: ConfigUpdate) {
        let _ = self.config_tx.send(update);
    }

    /// Ends the active session, if any; the listener keeps running.
    pub fn disconnect(&self) {
        if let Some(tx) = self.session_events.lock().unwrap().as_ref() {
            let _ = tx.send(ironrdp_server::ServerEvent::Disconnect);
        }
    }

    /// Gracefully stop the server.
    pub async fn stop(&self) -> Result<()> {
        if self.stopped.swap(true, Ordering::Relaxed) {
            return Ok(()); // Already stopped
        }
        self.shutdown_notify.notify_waiters();

        // Extract thread handle from mutex *before* the await point so that
        // the MutexGuard is dropped and the future remains Send.
        let thread = self.server_thread.lock().unwrap().take();
        if let Some(thread) = thread {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = thread.join();
            })
            .await;
        }
        // Cancel metrics task
        let task = self.metrics_task.lock().unwrap().take();
        if let Some(task) = task {
            task.abort();
        }
        Ok(())
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if !self.stopped.load(Ordering::Relaxed) {
            self.shutdown_notify.notify_waiters();
        }
    }
}

/// Arguments that are Send-safe, passed to the server thread.
/// RdpServer itself is !Send, so we pass everything needed to construct it.
struct ServerThreadArgs {
    config: ServerConfig,
    listener: TcpListener,
    authenticator: Arc<dyn ironrdp_server::RdpAuthenticator>,
    cert_path: std::path::PathBuf,
    key_path: std::path::PathBuf,
    width: u16,
    height: u16,
    quality: macrdp_encode::Quality,
    encoder_pref: macrdp_encode::EncoderPreference,
    mode_444: bool,
    coord_mapper: crate::handler::MouseCoordMapper,
    gfx_state: Arc<Mutex<GfxState>>,
    shutdown_notify: Arc<Notify>,
    handler: Arc<dyn ServerEventHandler>,
    config_rx: tokio::sync::mpsc::UnboundedReceiver<ConfigUpdate>,
    session_tx: tokio::sync::watch::Sender<bool>,
    session_events: SessionEvents,
}

/// The running server's event sender, set once the server is built.
type SessionEvents = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ironrdp_server::ServerEvent>>>>;

/// Start the RDP server with the given configuration and event handler.
/// Returns a handle to control the running server.
pub async fn start_server(
    config: ServerConfig,
    handler: impl ServerEventHandler,
) -> Result<Arc<ServerHandle>> {
    let handler: Arc<dyn ServerEventHandler> = Arc::new(handler);
    macrdp_capture::ddc::wake_if_left_asleep();

    // Notify handler: starting
    handler.on_status_change(ServerStatus {
        running: true,
        state: "starting".to_string(),
        uptime_secs: 0,
    });

    // Check only: prompting is the caller's call (the app asks during onboarding, the CLI
    // before starting), so starting never pops a system dialog.
    let perms = permissions::check_permissions();
    tracing::info!(?perms, "Permission status");

    let shield_handler = Arc::clone(&handler);
    macrdp_capture::virtual_display::set_shield_hook(Some(Box::new(move |displays: &[u32]| {
        shield_handler.on_shield_displays(displays.to_vec());
    })));

    // TLS
    let (cert_path, key_path) =
        tls::ensure_tls_files(config.cert_path.as_deref(), config.key_path.as_deref())?;
    match tls::cert_fingerprint(&cert_path) {
        Ok(fp) => tracing::info!(fingerprint = %fp, "TLS certificate SHA-256 — compare with the client's certificate prompt"),
        Err(e) => tracing::warn!("Cannot compute the TLS certificate fingerprint: {e:#}"),
    }

    // Display detection — SCK for capture sizing, CG for mouse mapping. Without Screen
    // Recording access SCK is skipped (it would prompt) and the CG size stands in.
    let cg_size = macrdp_capture::detect_cg_display_size();
    let (sck_w, sck_h) = match permissions::detect_display_size() {
        Ok((w, h)) => {
            tracing::info!(width = w, height = h, "SCK logical display size");
            (w as u16, h as u16)
        }
        Err(e) => {
            let (w, h) = cg_size.as_ref().map_or((1920, 1080), |&(w, h)| (w as u16, h as u16));
            tracing::warn!("Failed to detect SCK display size: {e}, using {w}x{h}");
            (w, h)
        }
    };
    let (cg_w, cg_h) = match cg_size {
        Ok((w, h)) => {
            tracing::info!(width = w, height = h, "CG logical display size (CGEvent coordinate space)");
            (w as u16, h as u16)
        }
        Err(e) => {
            tracing::warn!("Failed to detect CG display size: {e}, falling back to SCK size");
            (sck_w, sck_h)
        }
    };
    if sck_w != cg_w || sck_h != cg_h {
        tracing::warn!(sck_w, sck_h, cg_w, cg_h, "SCK and CG display sizes differ!");
    }

    // Resolution
    let (width, height) = if config.width > 0 && config.height > 0 {
        (config.width as u16, config.height as u16)
    } else {
        (sck_w, sck_h)
    };

    // Quality / encoder / chroma
    let quality = match config.quality.as_deref() {
        Some("low_latency") => macrdp_encode::Quality::LowLatency,
        Some("balanced") => macrdp_encode::Quality::Balanced,
        _ => macrdp_encode::Quality::HighQuality,
    };
    let encoder_pref = macrdp_encode::EncoderPreference::from_str_opt(config.encoder.as_deref());
    let mode_444 = config.chroma_mode.as_deref() == Some("avc444");

    // Resolution
    let res_mode = config.resolution.as_deref().unwrap_or("auto");
    let res_auto = res_mode == "auto";
    let (width, height) = resolve_resolution(res_mode, width, height);

    // Mouse coordinate mapping: mac = rdp × cg_logical ÷ rdp_desktop
    let coord_mapper = crate::handler::MouseCoordMapper::new(cg_w, cg_h, width, height);

    // GFX state (shared between server thread and metrics task)
    let mut gfx = GfxState::new(width, height, mode_444);
    gfx.set_rfx_quality(config.rfx_quality.as_deref());
    let gfx_state = Arc::new(Mutex::new(gfx));

    // A busy port is an error: clients are configured for one port, never a neighbour.
    let listener = TcpListener::bind((config.bind_address.as_str(), config.port))
        .with_context(|| format!("Cannot listen on {}:{}", config.bind_address, config.port))?;
    let port = listener.local_addr()?.port();

    let authenticator = mac_authenticator(config.auto_unlock)?;
    let shutdown_notify = Arc::new(Notify::new());
    let (config_tx, config_rx) = tokio::sync::mpsc::unbounded_channel::<ConfigUpdate>();
    let (session_tx, session_rx) = tokio::sync::watch::channel(false);
    let session_events = Arc::new(Mutex::new(None));

    // Pack everything the server thread needs
    let args = ServerThreadArgs {
        config: config.clone(),
        listener,
        authenticator,
        cert_path,
        key_path,
        width,
        height,
        quality,
        encoder_pref,
        mode_444,
        coord_mapper,
        gfx_state: Arc::clone(&gfx_state),
        shutdown_notify: Arc::clone(&shutdown_notify),
        handler: Arc::clone(&handler),
        config_rx,
        session_tx,
        session_events: Arc::clone(&session_events),
    };

    // Spawn RDP server on a dedicated OS thread.
    // RdpServer is !Send (contains dyn SoundServerFactory, dyn CliprdrServerFactory),
    // so we construct and run it entirely within this thread.
    let server_thread = std::thread::Builder::new()
        .name("rdp-server".into())
        .spawn(move || {
            run_server_thread(args);
        })
        .context("Failed to spawn RDP server thread")?;

    // Metrics ticker: 1 s, and only while a session runs (no timer at all when idle).
    let metrics_task = tokio::spawn(metrics_loop(Arc::clone(&gfx_state), Arc::clone(&handler), config.frame_rate, session_rx));

    let handle = Arc::new(ServerHandle {
        port,
        started_at: Instant::now(),
        gfx_state,
        shutdown_notify,
        stopped: AtomicBool::new(false),
        server_thread: Mutex::new(Some(server_thread)),
        metrics_task: Mutex::new(Some(metrics_task)),
        config_tx,
        session_events,
    });

    // Notify handler: running
    handler.on_status_change(ServerStatus {
        running: true,
        state: "running".to_string(),
        uptime_secs: 0,
    });

    Ok(handle)
}

/// Runs the RDP server entirely on the current thread. Called from the dedicated
/// server thread. Constructs the !Send RdpServer here so it never crosses thread
/// boundaries.
fn run_server_thread(args: ServerThreadArgs) {
    use crate::display::MacDisplay;
    use crate::handler::MacInputHandler;
    use ironrdp_server::{RdpServer, TlsIdentityCtx};

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime for RDP server");

    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let mut config_rx = args.config_rx;

        // Load TLS identity (on this thread)
        let tls_identity = match TlsIdentityCtx::init_from_paths(&args.cert_path, &args.key_path) {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to load TLS certificate: {e}");
                args.handler.on_status_change(ServerStatus {
                    running: false,
                    state: format!("error: {e}"),
                    uptime_secs: 0,
                });
                return;
            }
        };
        let tls_acceptor = match tls_identity.make_acceptor() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("Failed to create TLS acceptor: {e}");
                args.handler.on_status_change(ServerStatus {
                    running: false,
                    state: format!("error: {e}"),
                    uptime_secs: 0,
                });
                return;
            }
        };

        // Create input handler (clone coord_mapper — shared with display for resize sync)
        let input_handler = MacInputHandler::new(args.coord_mapper.clone());

        // Audio: captured only while a client has RDPSND open (published through the tap).
        let audio_tap = args.config.audio.enabled.then(macrdp_audio::new_audio_tap);

        // Create audio factory
        let sound_factory: Option<Box<dyn ironrdp_server::SoundServerFactory>> =
            if let Some(ref tap) = audio_tap {
                Some(Box::new(macrdp_audio::MacAudioFactory::new(
                    Arc::clone(tap),
                    args.config.audio.sample_rate,
                    args.config.audio.channels,
                )))
            } else {
                None
            };

        // Clipboard factory
        let cliprdr_factory: Option<Box<dyn ironrdp_server::CliprdrServerFactory>> =
            if args.config.clipboard.enabled {
                let temp_dir = dirs::cache_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                    .join("macrdp")
                    .join("clipboard");
                Some(Box::new(macrdp_clipboard::MacClipboardFactory::new(temp_dir, args.config.clipboard.max_file_size_mb)))
            } else {
                None
            };

        // Create display
        let res_fixed = args.config.resolution.as_deref().unwrap_or("auto") != "auto";
        let fixed_resolution = (args.config.width > 0 && args.config.height > 0) || res_fixed;
        let bitrate_override = args.config.bitrate_mbps.map(|mbps| mbps * 1_000_000);
        let show_cursor = args.config.show_cursor.unwrap_or(true);
        macrdp_capture::virtual_display::set_curtain_standby_ok(args.config.curtain_standby_ok == Some(true));
        macrdp_capture::virtual_display::set_curtain_standby_keepalive(args.config.curtain_standby_keepalive);
        let display_mode = Arc::new(Mutex::new(macrdp_capture::virtual_display::DisplayMode::from_config(
            args.config.display_mode.as_deref(),
        )));
        let display = MacDisplay::new(
            args.width,
            args.height,
            fixed_resolution,
            // 0 would be an invalid ScreenCaptureKit frame interval.
            args.config.frame_rate.clamp(1, 120),
            args.quality,
            args.encoder_pref,
            args.mode_444,
            show_cursor,
            bitrate_override,
            Arc::clone(&args.gfx_state),
            args.coord_mapper,
            audio_tap,
            None, // perf_stats: not exposed via macrdp-core API yet
        )
        .with_display_mode(Arc::clone(&display_mode));

        // Build RDP server (this is the !Send type)
        let bind_addr = match args.listener.local_addr() {
            Ok(addr) => addr,
            Err(e) => {
                tracing::error!("Listener has no local address: {e}");
                return;
            }
        };
        let listener = match args
            .listener
            .set_nonblocking(true)
            .and_then(|()| tokio::net::TcpListener::from_std(args.listener))
        {
            Ok(listener) => listener,
            Err(e) => {
                tracing::error!("Failed to register listener: {e}");
                args.handler.on_status_change(ServerStatus {
                    running: false,
                    state: format!("error: {e}"),
                    uptime_secs: 0,
                });
                return;
            }
        };

        let mut server = RdpServer::builder()
            .with_addr(bind_addr)
            .with_hybrid(tls_acceptor, tls_identity.pub_key)
            .with_input_handler(input_handler)
            .with_display_handler(display)
            .with_sound_factory(sound_factory)
            .with_cliprdr_factory(cliprdr_factory)
            .build();

        server.set_gfx_state(Arc::clone(&args.gfx_state));

        server.set_authenticator(args.authenticator);
        server.set_session_lifecycle(Arc::new(ReportingLifecycle {
            inner: Arc::new(crate::lifecycle::MacSessionLifecycle::new(args.config.lock_on_disconnect, args.config.auto_unlock)),
            handler: Arc::clone(&args.handler),
            gfx_state: Arc::clone(&args.gfx_state),
            session: args.session_tx,
        }));
        *args.session_events.lock().unwrap() = Some(server.event_sender().clone());
        let idle = args.config.idle_timeout_secs;
        server.set_idle_timeout((idle > 0).then(|| std::time::Duration::from_secs(idle)));

        tracing::info!(%bind_addr, "RDP server listening (NLA only)");

        // Spawn config hot-update listener
        let gfx_for_config = Arc::clone(&args.gfx_state);
        let mode_for_config = Arc::clone(&display_mode);
        tokio::task::spawn_local(async move {
            while let Some(update) = config_rx.recv().await {
                match update {
                    ConfigUpdate::FrameRate(fps) => {
                        tracing::info!(fps, "Hot-update: frame_rate (next connection)");
                        gfx_for_config.lock().unwrap().next_frame_rate = Some(fps);
                    }
                    ConfigUpdate::BitrateKbps(kbps) => {
                        let bps = kbps * 1000;
                        tracing::info!(bitrate_kbps = kbps, "Hot-update: bitrate");
                        gfx_for_config.lock().unwrap().target_bitrate = Some(bps);
                    }
                    ConfigUpdate::LogLevel(level) => {
                        tracing::info!(%level, "Hot-update: log_level");
                    }
                    ConfigUpdate::ShowCursor(show) => {
                        tracing::info!(show_cursor = show, "Hot-update: show_cursor (next connection)");
                        gfx_for_config.lock().unwrap().show_cursor = Some(show);
                    }
                    ConfigUpdate::Resolution(res) => {
                        tracing::info!(%res, "Hot-update: resolution (next connection)");
                        gfx_for_config.lock().unwrap().resolution = Some(res);
                    }
                    ConfigUpdate::Encoder(enc) => {
                        tracing::info!(%enc, "Hot-update: encoder (next connection)");
                        gfx_for_config.lock().unwrap().encoder_pref = Some(enc);
                    }
                    ConfigUpdate::ChromaMode(mode) => {
                        let avc444 = mode == "avc444";
                        tracing::info!(%mode, avc444, "Hot-update: chroma_mode (next connection)");
                        gfx_for_config.lock().unwrap().chroma_mode = Some(mode);
                    }
                    ConfigUpdate::RfxQuality(q) => {
                        tracing::info!(rfx_quality = %q, "Hot-update: rfx_quality (next frame)");
                        gfx_for_config.lock().unwrap().set_rfx_quality(Some(q.as_str()));
                    }
                    ConfigUpdate::DisplayMode(mode) => {
                        let mode = macrdp_capture::virtual_display::DisplayMode::from_config(Some(mode.as_str()));
                        tracing::info!(?mode, "Hot-update: display_mode (next connection)");
                        *mode_for_config.lock().unwrap() = mode;
                    }
                    ConfigUpdate::CurtainStandbyOk(ok) => {
                        tracing::info!(ok, "Hot-update: curtain_standby_ok (next connection)");
                        macrdp_capture::virtual_display::set_curtain_standby_ok(ok);
                    }
                    ConfigUpdate::CurtainStandbyKeepalive(on) => {
                        tracing::info!(on, "Hot-update: curtain_standby_keepalive (next connection)");
                        macrdp_capture::virtual_display::set_curtain_standby_keepalive(on);
                    }
                }
            }
        });

        tokio::select! {
            result = server.run_on(listener) => {
                if let Err(e) = result {
                    tracing::error!("RDP server error: {e}");
                    args.handler.on_status_change(ServerStatus {
                        running: false,
                        state: format!("error: {e}"),
                        uptime_secs: 0,
                    });
                }
            }
            _ = args.shutdown_notify.notified() => {
                tracing::info!("Shutdown signal received, stopping server");
            }
        }
    });
}

/// Reports metrics once a second while `session` is true; with no session it waits on the
/// watch channel only, so an idle server runs no timer.
async fn metrics_loop(
    gfx_state: Arc<Mutex<GfxState>>,
    handler: Arc<dyn ServerEventHandler>,
    fps: u32,
    mut session: tokio::sync::watch::Receiver<bool>,
) {
    let period = std::time::Duration::from_secs(1);
    loop {
        if session.wait_for(|active| *active).await.is_err() {
            return;
        }
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        let mut prev_bytes_sent = gfx_state.lock().unwrap().total_bytes_sent;
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                changed = session.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if !*session.borrow_and_update() {
                        break;
                    }
                    continue;
                }
            }
            let mut metrics = {
                let gfx = gfx_state.lock().unwrap();
                Metrics {
                    fps,
                    rtt_ms: gfx.rtt_ewma_ms,
                    bytes_sent: gfx.total_bytes_sent,
                    encode_ms: gfx.last_encode_ms,
                    net_ms: (gfx.rtt_ewma_ms - gfx.last_encode_ms).max(0.0),
                    last_frame_bytes: gfx.last_frame_bytes,
                    network_quality: gfx.network_quality,
                    pending_acks: gfx.pending_acks,
                    client_name: gfx.client_name.as_deref().map(|n| n.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()),
                    width: gfx.width,
                    height: gfx.height,
                    codec: session_codec(&gfx).map(str::to_string),
                    rfx_quality: (session_codec(&gfx) == Some("rfx-progressive")).then(|| gfx.rfx_quality.as_str().to_string()),
                    ..Default::default()
                }
            };
            // A new session resets the byte counter.
            metrics.bitrate_kbps = metrics.bytes_sent.saturating_sub(prev_bytes_sent) * 8 / 1000;
            prev_bytes_sent = metrics.bytes_sent;
            metrics.timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            handler.on_metrics(metrics);
        }
    }
}

/// The GFX codec the display pipeline uses once caps are confirmed (see `display::route`).
fn session_codec(gfx: &GfxState) -> Option<&'static str> {
    if gfx.avc_ready() {
        Some("avc420")
    } else if gfx.is_ready() {
        Some("rfx-progressive")
    } else {
        None
    }
}

/// Wraps the Mac lifecycle: reports connects and disconnects to the handler and drives the
/// session flag the metrics ticker waits on.
struct ReportingLifecycle {
    inner: Arc<dyn ironrdp_server::SessionLifecycle>,
    handler: Arc<dyn ServerEventHandler>,
    gfx_state: Arc<Mutex<GfxState>>,
    session: tokio::sync::watch::Sender<bool>,
}

impl ironrdp_server::SessionLifecycle for ReportingLifecycle {
    fn session_started(&self, peer: std::net::SocketAddr) -> Box<dyn ironrdp_server::SessionGuard> {
        let inner = self.inner.session_started(peer);
        self.session.send_replace(true);
        self.handler.on_connection(ConnectionEvent::Connected(ConnectionInfo {
            client_ip: peer.ip().to_string(),
            client_name: String::new(),
            connected_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }));
        Box::new(ReportingGuard {
            inner: Some(inner),
            peer,
            started: Instant::now(),
            handler: Arc::clone(&self.handler),
            gfx_state: Arc::clone(&self.gfx_state),
            session: self.session.clone(),
        })
    }
}

struct ReportingGuard {
    inner: Option<Box<dyn ironrdp_server::SessionGuard>>,
    peer: std::net::SocketAddr,
    started: Instant,
    handler: Arc<dyn ServerEventHandler>,
    gfx_state: Arc<Mutex<GfxState>>,
    session: tokio::sync::watch::Sender<bool>,
}

impl ironrdp_server::SessionGuard for ReportingGuard {
    fn replaced(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            inner.replaced();
        }
    }
}

impl Drop for ReportingGuard {
    fn drop(&mut self) {
        // The Mac side (screen restore, lock) finishes before the session is reported over.
        drop(self.inner.take());
        self.session.send_replace(false);
        let bytes_total = self.gfx_state.lock().map(|g| g.total_bytes_sent).unwrap_or(0);
        self.handler.on_connection(ConnectionEvent::Disconnected(DisconnectionInfo {
            client_ip: self.peer.ip().to_string(),
            client_name: String::new(),
            duration_secs: self.started.elapsed().as_secs(),
            bytes_total,
        }));
    }
}

/// Only the Mac user running macrdp may connect, with the enrolled Mac password.
fn mac_authenticator(auto_unlock: bool) -> Result<Arc<dyn ironrdp_server::RdpAuthenticator>> {
    use crate::auth;

    let user = auth::current_user()?;
    let authenticator = auth::MacAuthenticator::new(user.clone(), auth::host_names(), Arc::new(auth::SystemAccountStore))
        .with_auto_unlock(auto_unlock);
    // The only Keychain read until an enroll: TCP connects use the cached hash.
    if authenticator.preload() {
        tracing::info!(user = %user, "Remote login enabled for Mac account");
    } else {
        tracing::warn!(
            user = %user,
            "No Mac password enrolled — remote login is refused until you run `macrdp-server enroll` or enroll in the app"
        );
    }
    Ok(Arc::new(authenticator))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[derive(Default)]
    struct Recorder {
        metrics: Mutex<Vec<Metrics>>,
        connections: Mutex<Vec<ConnectionEvent>>,
    }

    impl ServerEventHandler for Arc<Recorder> {
        fn on_metrics(&self, metrics: Metrics) {
            self.metrics.lock().unwrap().push(metrics);
        }
        fn on_connection(&self, event: ConnectionEvent) {
            self.connections.lock().unwrap().push(event);
        }
    }

    fn gfx() -> Arc<Mutex<GfxState>> {
        Arc::new(Mutex::new(GfxState::new(1920, 1080, false)))
    }

    #[tokio::test(start_paused = true)]
    async fn metrics_tick_only_during_a_session() {
        let rec = Arc::new(Recorder::default());
        let (session_tx, session_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(metrics_loop(gfx(), Arc::new(Arc::clone(&rec)), 30, session_rx));

        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(rec.metrics.lock().unwrap().len(), 0, "no metrics while idle");

        session_tx.send_replace(true);
        tokio::time::sleep(Duration::from_millis(3500)).await;
        let during = rec.metrics.lock().unwrap().len();
        assert!((2..=4).contains(&during), "about one per second in a session, got {during}");

        session_tx.send_replace(false);
        tokio::time::sleep(Duration::from_millis(10)).await;
        let after_end = rec.metrics.lock().unwrap().len();
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(rec.metrics.lock().unwrap().len(), after_end, "metrics stop when the session ends");
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn metrics_carry_session_details() {
        let rec = Arc::new(Recorder::default());
        let state = gfx();
        {
            let mut st = state.lock().unwrap();
            st.channel_id = Some(1);
            st.caps_confirmed = true;
            st.avc420_supported = true;
            st.client_name = Some("PIXEL\u{0}\n".to_string());
        }
        let (_session_tx, session_rx) = tokio::sync::watch::channel(true);
        let task = tokio::spawn(metrics_loop(Arc::clone(&state), Arc::new(Arc::clone(&rec)), 30, session_rx));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let m = rec.metrics.lock().unwrap()[0].clone();
        assert_eq!((m.width, m.height), (1920, 1080));
        assert_eq!(m.codec.as_deref(), Some("avc420"));
        assert_eq!(m.client_name.as_deref(), Some("PIXEL"), "control characters are stripped");
        task.abort();
    }

    #[test]
    fn codec_follows_the_negotiated_gfx_path() {
        let mut st = GfxState::new(800, 600, false);
        assert_eq!(session_codec(&st), None, "nothing before GFX caps are confirmed");
        st.channel_id = Some(1);
        st.caps_confirmed = true;
        assert_eq!(session_codec(&st), Some("rfx-progressive"));
        st.avc420_supported = true;
        assert_eq!(session_codec(&st), Some("avc420"));
    }

    struct NoopLifecycle;
    struct NoopGuard;
    impl ironrdp_server::SessionGuard for NoopGuard {
        fn replaced(&mut self) {}
    }
    impl ironrdp_server::SessionLifecycle for NoopLifecycle {
        fn session_started(&self, _peer: std::net::SocketAddr) -> Box<dyn ironrdp_server::SessionGuard> {
            Box::new(NoopGuard)
        }
    }

    #[test]
    fn sessions_are_reported_and_tracked() {
        use ironrdp_server::SessionLifecycle as _;
        let rec = Arc::new(Recorder::default());
        let (session_tx, session_rx) = tokio::sync::watch::channel(false);
        let lifecycle = ReportingLifecycle {
            inner: Arc::new(NoopLifecycle),
            handler: Arc::new(Arc::clone(&rec)),
            gfx_state: gfx(),
            session: session_tx,
        };
        let guard = lifecycle.session_started("192.0.2.7:50000".parse().unwrap());
        assert!(*session_rx.borrow());
        drop(guard);
        assert!(!*session_rx.borrow());
        let events = rec.connections.lock().unwrap();
        assert!(matches!(&events[0], ConnectionEvent::Connected(i) if i.client_ip == "192.0.2.7"));
        assert!(matches!(&events[1], ConnectionEvent::Disconnected(i) if i.client_ip == "192.0.2.7"));
    }
}
