use core::cell::Cell;
use core::net::{IpAddr, SocketAddr};
use core::pin::Pin;
use core::time::Duration;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use ironrdp_acceptor::{Acceptor, AcceptorResult, BeginResult, DesktopSize};
use ironrdp_async::Framed;
use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::pdu::{ClipboardPdu, FileContentsRequest, OwnedFileContentsResponse};
use ironrdp_cliprdr::CliprdrServer;
use ironrdp_core::{decode, encode_vec, impl_as_any};
use ironrdp_displaycontrol::pdu::{DisplayControlCapabilities, DisplayControlMonitorLayout, DisplayControlPdu};
use ironrdp_displaycontrol::server::DisplayControlHandler;
use ironrdp_pdu::input::fast_path::{FastPathInput, FastPathInputEvent};
use ironrdp_pdu::input::InputEventPdu;
use ironrdp_pdu::mcs::{SendDataIndication, SendDataRequest};
use ironrdp_pdu::rdp::capability_sets::{BitmapCodecs, CapabilitySet, CmdFlags, CodecProperty, GeneralExtraFlags};
use ironrdp_pdu::rdp::headers::{ServerDeactivateAll, ShareControlPdu};
use ironrdp_pdu::x224::X224;
use ironrdp_pdu::{decode_err, mcs, nego, rdp, Action, PduResult};
use ironrdp_svc::{server_encode_svc_messages, ChannelFlags, StaticChannelId, StaticChannelSet, SvcMessage, SvcProcessor};
use ironrdp_tokio::{split_tokio_framed, unsplit_tokio_framed, FramedRead, FramedWrite, TokioFramed};
use rdpsnd::server::{RdpsndServer, RdpsndServerMessage};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task;
use futures_util::future::{abortable, AbortHandle};
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, trace, warn};
use {ironrdp_dvc as dvc, ironrdp_rdpsnd as rdpsnd};

use crate::auth::RdpAuthenticator;
use crate::clipboard::CliprdrServerFactory;
use crate::display::{DisplayUpdate, RdpServerDisplay};
use crate::encoder::{UpdateEncoder, UpdateEncoderCodecs};
use crate::gfx::{GfxHandler, GfxState};
use crate::handler::{MouseEvent, RdpServerInputHandler};
use crate::lifecycle::SessionLifecycle;
use crate::{builder, capabilities, SoundServerFactory};

#[derive(Clone)]
pub struct RdpServerOptions {
    pub addr: SocketAddr,
    pub security: RdpServerSecurity,
    pub codecs: BitmapCodecs,
}

impl RdpServerOptions {
    fn has_image_remote_fx(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::ImageRemoteFx(_)))
    }

    fn has_remote_fx(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::RemoteFx(_)))
    }

    #[cfg(feature = "qoi")]
    fn has_qoi(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::Qoi))
    }

    #[cfg(feature = "qoiz")]
    fn has_qoiz(&self) -> bool {
        self.codecs
            .0
            .iter()
            .any(|codec| matches!(codec.property, CodecProperty::QoiZ))
    }
}

#[derive(Clone)]
pub enum RdpServerSecurity {
    None,
    Tls(TlsAcceptor),
    /// Used for both hybrid + hybrid-ex.
    Hybrid((TlsAcceptor, Vec<u8>)),
}

impl RdpServerSecurity {
    pub fn flag(&self) -> nego::SecurityProtocol {
        match self {
            RdpServerSecurity::None => nego::SecurityProtocol::empty(),
            RdpServerSecurity::Tls(_) => nego::SecurityProtocol::SSL,
            // NLA only: a TLS-only or standard-security client gets a negotiation failure.
            RdpServerSecurity::Hybrid(_) => nego::SecurityProtocol::HYBRID | nego::SecurityProtocol::HYBRID_EX,
        }
    }
}

struct AInputHandler {
    /// Feeds one blocking worker, so mouse events apply in arrival order.
    mouse_tx: mpsc::UnboundedSender<MouseEvent>,
}

/// Start the ainput mouse worker; it exits when the returned sender drops.
fn spawn_mouse_worker(handler: Arc<Mutex<Box<dyn RdpServerInputHandler>>>) -> mpsc::UnboundedSender<MouseEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    task::spawn_blocking(move || {
        while let Some(event) = rx.blocking_recv() {
            handler.blocking_lock().mouse(event);
        }
    });
    tx
}

impl_as_any!(AInputHandler);

impl dvc::DvcProcessor for AInputHandler {
    fn channel_name(&self) -> &str {
        ironrdp_ainput::CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<dvc::DvcMessage>> {
        use ironrdp_ainput::{ServerPdu, VersionPdu};

        let pdu = ServerPdu::Version(VersionPdu::default());

        Ok(vec![Box::new(pdu)])
    }

    fn close(&mut self, _channel_id: u32) {}

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<dvc::DvcMessage>> {
        use ironrdp_ainput::ClientPdu;

        match decode(payload).map_err(|e| decode_err!(e))? {
            ClientPdu::Mouse(pdu) => {
                if self.mouse_tx.send(pdu.into()).is_err() {
                    debug!("ainput mouse worker stopped; dropping event");
                }
            }
        }

        Ok(Vec::new())
    }
}

impl dvc::DvcServerProcessor for AInputHandler {}

struct DisplayControlBackend {
    /// Feeds one blocking worker, so monitor layouts apply in arrival order.
    layout_tx: mpsc::UnboundedSender<DisplayControlMonitorLayout>,
}

impl DisplayControlBackend {
    /// Starts the layout worker; it exits when the backend drops.
    fn new(display: Arc<Mutex<Box<dyn RdpServerDisplay>>>) -> Self {
        let (layout_tx, mut rx) = mpsc::unbounded_channel();
        task::spawn_blocking(move || {
            while let Some(layout) = rx.blocking_recv() {
                display.blocking_lock().request_layout(layout);
            }
        });
        Self { layout_tx }
    }
}

impl DisplayControlHandler for DisplayControlBackend {
    fn monitor_layout(&self, layout: DisplayControlMonitorLayout) {
        if self.layout_tx.send(layout).is_err() {
            debug!("display layout worker stopped; dropping layout");
        }
    }
}

/// Display Control capabilities (MS-RDPEDISP 2.2.2.1): one monitor, and a maximum area of
/// MaxNumMonitors x FactorA x FactorB square pixels. 8192 x 8192 covers every size the display
/// accepts (sides 200..=8192); ironrdp's default (3840 x 2400) refused layouts above 9.2 MP.
const DISPLAY_CONTROL_CAPS: (u32, u32, u32) = (1, 8192, 8192);

/// The Display Control channel: capabilities on open, client monitor layouts to the display.
/// Logged at info so a client that never opens it (and stretches instead) is visible.
struct DisplayControlChannel {
    backend: DisplayControlBackend,
}

impl_as_any!(DisplayControlChannel);

impl dvc::DvcProcessor for DisplayControlChannel {
    fn channel_name(&self) -> &str {
        ironrdp_displaycontrol::CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<dvc::DvcMessage>> {
        let (monitors, a, b) = DISPLAY_CONTROL_CAPS;
        let caps = DisplayControlCapabilities::new(monitors, a, b).map_err(|e| decode_err!(e))?;
        info!(
            channel_id,
            max_monitors = monitors,
            max_area_factor_a = a,
            max_area_factor_b = b,
            "Display Control channel opened, capabilities sent"
        );
        Ok(vec![Box::new(DisplayControlPdu::from(caps))])
    }

    fn close(&mut self, channel_id: u32) {
        info!(channel_id, "Display Control channel closed");
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<dvc::DvcMessage>> {
        match decode(payload).map_err(|e| decode_err!(e))? {
            DisplayControlPdu::MonitorLayout(layout) => {
                for m in layout.monitors() {
                    let (width, height) = m.dimensions();
                    info!(
                        width,
                        height,
                        primary = m.is_primary(),
                        orientation = m.orientation().map(|o| o.angle()),
                        desktop_scale = m.desktop_scale_factor(),
                        device_scale = m.device_scale_factor().map(|s| s.value()),
                        physical_mm = ?m.physical_dimensions(),
                        "Display Control monitor layout received"
                    );
                }
                self.backend.monitor_layout(layout);
            }
            DisplayControlPdu::Caps(caps) => debug!(?caps, "Display Control capabilities from client"),
        }
        Ok(Vec::new())
    }
}

impl dvc::DvcServerProcessor for DisplayControlChannel {}

/// RDP Server
///
/// A server is created to listen for connections.
/// After the connection sequence is finalized using the provided security mechanism, the server can:
///  - receive display updates from a [`RdpServerDisplay`] and forward them to the client
///  - receive input events from a client and forward them to an [`RdpServerInputHandler`]
///
/// # Example
///
/// ```
/// use ironrdp_server::{RdpServer, RdpServerInputHandler, RdpServerDisplay, RdpServerDisplayUpdates};
///
///# use anyhow::Result;
///# use ironrdp_server::{DisplayUpdate, DesktopSize, KeyboardEvent, MouseEvent};
///# use tokio_rustls::TlsAcceptor;
///# struct NoopInputHandler;
///# impl RdpServerInputHandler for NoopInputHandler {
///#     fn keyboard(&mut self, _: KeyboardEvent) {}
///#     fn mouse(&mut self, _: MouseEvent) {}
///# }
///# struct NoopDisplay;
///# #[async_trait::async_trait]
///# impl RdpServerDisplay for NoopDisplay {
///#     async fn size(&mut self) -> DesktopSize {
///#         todo!()
///#     }
///#     async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
///#         todo!()
///#     }
///# }
///# async fn stub() -> Result<()> {
/// fn make_tls_acceptor() -> TlsAcceptor {
///    /* snip */
///#    todo!()
/// }
///
/// fn make_input_handler() -> impl RdpServerInputHandler {
///    /* snip */
///#    NoopInputHandler
/// }
///
/// fn make_display_handler() -> impl RdpServerDisplay {
///    /* snip */
///#    NoopDisplay
/// }
///
/// let tls_acceptor = make_tls_acceptor();
/// let input_handler = make_input_handler();
/// let display_handler = make_display_handler();
///
/// let mut server = RdpServer::builder()
///     .with_addr(([127, 0, 0, 1], 3389))
///     .with_tls(tls_acceptor)
///     .with_input_handler(input_handler)
///     .with_display_handler(display_handler)
///     .build();
///
/// server.run().await;
/// Ok(())
///# }
/// ```
pub struct RdpServer {
    opts: RdpServerOptions,
    // FIXME: replace with a channel and poll/process the handler?
    handler: Arc<Mutex<Box<dyn RdpServerInputHandler>>>,
    display: Arc<Mutex<Box<dyn RdpServerDisplay>>>,
    static_channels: StaticChannelSet,
    sound_factory: Option<Box<dyn SoundServerFactory>>,
    cliprdr_factory: Option<Box<dyn CliprdrServerFactory>>,
    ev_sender: mpsc::UnboundedSender<ServerEvent>,
    ev_receiver: Arc<Mutex<mpsc::UnboundedReceiver<ServerEvent>>>,
    auth: Option<Arc<dyn RdpAuthenticator>>,
    session_lifecycle: Option<Arc<dyn SessionLifecycle>>,
    handshake_timeout: Duration,
    idle_timeout: Option<Duration>,
    local_addr: Option<SocketAddr>,
    gfx_state: Arc<std::sync::Mutex<GfxState>>,
    gfx_enabled: bool,
}

#[derive(Debug)]
pub enum ServerEvent {
    Quit(String),
    Clipboard(ClipboardMessage),
    ClipboardFileContents(OwnedFileContentsResponse),
    ClipboardFileContentsRequest(FileContentsRequest),
    Rdpsnd(RdpsndServerMessage),
    GetLocalAddr(oneshot::Sender<Option<SocketAddr>>),
    /// Ends the active session; the listener keeps running. Ignored when idle.
    Disconnect,
}

pub trait ServerEventSender {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>);
}

impl ServerEvent {
    pub fn create_channel() -> (mpsc::UnboundedSender<Self>, mpsc::UnboundedReceiver<Self>) {
        mpsc::unbounded_channel()
    }
}

#[derive(Debug, PartialEq)]
enum RunState {
    Continue,
    Disconnect,
    DeactivationReactivation { desktop_size: DesktopSize },
}

impl RdpServer {
    pub fn new(
        opts: RdpServerOptions,
        handler: Box<dyn RdpServerInputHandler>,
        display: Box<dyn RdpServerDisplay>,
        mut sound_factory: Option<Box<dyn SoundServerFactory>>,
        mut cliprdr_factory: Option<Box<dyn CliprdrServerFactory>>,
    ) -> Self {
        let (ev_sender, ev_receiver) = ServerEvent::create_channel();
        if let Some(cliprdr) = cliprdr_factory.as_mut() {
            cliprdr.set_sender(ev_sender.clone());
        }
        if let Some(snd) = sound_factory.as_mut() {
            snd.set_sender(ev_sender.clone());
        }
        Self {
            opts,
            handler: Arc::new(Mutex::new(handler)),
            display: Arc::new(Mutex::new(display)),
            static_channels: StaticChannelSet::new(),
            sound_factory,
            cliprdr_factory,
            ev_sender,
            ev_receiver: Arc::new(Mutex::new(ev_receiver)),
            auth: None,
            session_lifecycle: None,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            idle_timeout: None,
            local_addr: None,
            gfx_state: Arc::new(std::sync::Mutex::new(GfxState::new(0, 0, false))),
            gfx_enabled: true,
        }
    }

    /// Set the GFX state (call before run() to share state with display)
    pub fn set_gfx_state(&mut self, state: Arc<std::sync::Mutex<GfxState>>) {
        self.gfx_state = state;
    }

    /// Get a reference to the GFX state
    pub fn gfx_state(&self) -> &Arc<std::sync::Mutex<GfxState>> {
        &self.gfx_state
    }

    /// Enable or disable GFX H.264 channel
    pub fn set_gfx_enabled(&mut self, enabled: bool) {
        self.gfx_enabled = enabled;
    }

    pub fn builder() -> builder::RdpServerBuilder<builder::WantsAddr> {
        builder::RdpServerBuilder::new()
    }

    pub fn event_sender(&self) -> &mpsc::UnboundedSender<ServerEvent> {
        &self.ev_sender
    }

    fn attach_channels(&mut self, acceptor: &mut Acceptor) {
        if let Some(cliprdr_factory) = self.cliprdr_factory.as_deref() {
            let backend = cliprdr_factory.build_cliprdr_backend();

            let cliprdr = CliprdrServer::new(backend);

            acceptor.attach_static_channel(cliprdr);
        }

        if let Some(factory) = self.sound_factory.as_deref() {
            let backend = factory.build_backend();

            acceptor.attach_static_channel(RdpsndServer::new(backend));
        }

        let dcs_backend = DisplayControlBackend::new(Arc::clone(&self.display));
        let mut dvc = dvc::DrdynvcServer::new()
            .with_dynamic_channel(AInputHandler {
                mouse_tx: spawn_mouse_worker(Arc::clone(&self.handler)),
            })
            .with_dynamic_channel(DisplayControlChannel { backend: dcs_backend });

        // Only register GFX channel if GFX frame sending is enabled
        if self.gfx_enabled {
            let gfx_handler = GfxHandler::new(Arc::clone(&self.gfx_state));
            dvc = dvc.with_dynamic_channel(gfx_handler);
            debug!("GFX channel registered");
        }

        acceptor.attach_static_channel(dvc);
    }

    /// Makes an authenticated handshake the active session.
    async fn run_session(&mut self, hs: Handshaked) -> Result<()> {
        let Handshaked {
            framed,
            mut acceptor,
            peer,
        } = hs;

        // Reset GFX state for new connection, preserving peer IP and hot-updated resolution
        let pending_resolution = {
            let mut gs = self.gfx_state.lock().unwrap();
            let w = gs.width;
            let h = gs.height;
            let avc444_enabled = gs.avc444_enabled;
            let res = gs.resolution.take();
            *gs = GfxState::new(w, h, avc444_enabled);
            gs.peer_addr = Some(peer.ip());
            res
        };

        // Apply hot-updated resolution for this new connection
        if let Some(ref res) = pending_resolution {
            if let Some((w, h)) = res.split_once('x').and_then(|(w, h)| {
                Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?))
            }) {
                info!(w, h, "Applying hot-updated resolution for new connection");
                // Use set_size (bypasses fixed_resolution check, since this is server-initiated)
                self.display.lock().await.set_size(w, h);
                let mut gs = self.gfx_state.lock().unwrap();
                gs.width = w;
                gs.height = h;
            }
        }

        let size = self.display.lock().await.size().await;
        acceptor.set_desktop(size, capabilities::capabilities(&self.opts, size));
        self.attach_channels(&mut acceptor);

        let framed = self.accept_finalize(framed, acceptor).await?;
        debug!("Shutting down TLS connection");
        let (mut tls_stream, _) = framed.into_inner();
        if let Err(e) = tls_stream.shutdown().await {
            debug!(?e, "TLS shutdown error");
        }
        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        let listener = TcpListener::bind(self.opts.addr).await?;
        self.run_on(listener).await
    }

    /// Serves connections from an already bound listener.
    ///
    /// Each socket handshakes (negotiation, TLS, CredSSP) in its own future with a
    /// timeout per stage, so no peer can block the server. At most two handshakes per
    /// source IP and eight in total run at once; when full, the oldest unauthenticated
    /// one is evicted. Only an authenticated connection becomes the active session, and
    /// it replaces the previous one.
    pub async fn run_on(&mut self, listener: TcpListener) -> Result<()> {
        let RdpServerSecurity::Hybrid((tls, pub_key)) = self.opts.security.clone() else {
            bail!("only NLA (hybrid) security is supported");
        };
        let auth = self.auth.clone().context("no authenticator configured")?;
        let ctx = Rc::new(HandshakeCtx {
            tls,
            pub_key,
            security: self.opts.security.flag(),
            auth,
        });

        let local_addr = listener.local_addr()?;
        debug!("Listening for connections on {local_addr}");
        self.local_addr = Some(local_addr);

        let mut handshakes: FuturesUnordered<HandshakeFuture> = FuturesUnordered::new();
        let mut slots = HandshakeSlots::default();
        let mut next: Option<Handshaked> = None;

        loop {
            if let Some(hs) = next.take() {
                let peer = hs.peer;
                // Dropped when this session ends, including when this future is cancelled.
                let mut guard = self.session_lifecycle.as_ref().map(|l| l.session_started(peer));
                {
                    let session = self.run_session(hs);
                    tokio::pin!(session);
                    loop {
                        tokio::select! {
                            result = &mut session => {
                                log_session_end(peer, result);
                                break;
                            }
                            accepted = listener.accept() => start_handshake(&ctx, &mut handshakes, &mut slots, accepted),
                            Some((id, done)) = handshakes.next() => {
                                slots.remove(id);
                                if let Some(hs) = done {
                                    info!(old = %peer, new = %hs.peer, "New authenticated connection replaces the active session");
                                    if let Some(guard) = guard.as_mut() {
                                        guard.replaced();
                                    }
                                    next = Some(hs);
                                    break;
                                }
                            }
                        }
                    }
                }
                drop(guard);
                info!("Ready for next connection");
                self.static_channels = StaticChannelSet::new();
                continue;
            }

            let ev_receiver = Arc::clone(&self.ev_receiver);
            let mut ev_receiver = ev_receiver.lock().await;
            tokio::select! {
                Some(event) = ev_receiver.recv() => {
                    match event {
                        ServerEvent::Quit(reason) => {
                            debug!("Got quit event {reason}");
                            break;
                        }
                        ServerEvent::GetLocalAddr(tx) => {
                            let _ = tx.send(self.local_addr);
                        }
                        ev => {
                            debug!("Unexpected event {:?}", ev);
                        }
                    }
                },
                accepted = listener.accept() => start_handshake(&ctx, &mut handshakes, &mut slots, accepted),
                Some((id, done)) = handshakes.next() => {
                    slots.remove(id);
                    next = done;
                }
            }
        }

        Ok(())
    }

    pub fn get_svc_processor<T: SvcProcessor + 'static>(&mut self) -> Option<&mut T> {
        self.static_channels
            .get_by_type_mut::<T>()
            .and_then(|svc| svc.channel_processor_downcast_mut())
    }

    pub fn get_channel_id_by_type<T: SvcProcessor + 'static>(&self) -> Option<StaticChannelId> {
        self.static_channels.get_channel_id_by_type::<T>()
    }

    async fn dispatch_pdu(
        &mut self,
        action: Action,
        bytes: bytes::BytesMut,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
    ) -> Result<RunState> {
        match action {
            Action::FastPath => {
                let input = decode(&bytes)?;
                self.handle_fastpath(input).await;
            }

            Action::X224 => {
                if self
                    .handle_x224(writer, io_channel_id, user_channel_id, &bytes)
                    .await
                    .context("X224 input error")?
                {
                    debug!("Got disconnect request");
                    return Ok(RunState::Disconnect);
                }
            }
        }

        Ok(RunState::Continue)
    }

    async fn dispatch_display_update(
        update: DisplayUpdate,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
        io_channel_id: u16,
        buffer: &mut Vec<u8>,
        mut encoder: UpdateEncoder,
        gfx_state: &Arc<std::sync::Mutex<GfxState>>,
        drdynvc_channel_id: Option<StaticChannelId>,
    ) -> Result<(RunState, UpdateEncoder)> {
        if let DisplayUpdate::Resize(desktop_size) = update {
            debug!(?desktop_size, "Display resize");
            encoder.set_desktop_size(desktop_size);
            deactivate_all(io_channel_id, user_channel_id, writer).await?;
            return Ok((RunState::DeactivationReactivation { desktop_size }, encoder));
        }

        // Handle GFX frames through the DVC channel
        if let DisplayUpdate::GfxFrame(ref gfx_frame) = update {
            let mut state = gfx_state.lock().unwrap();
            if state.avc_ready() {
                if let Some(drdynvc_id) = drdynvc_channel_id {
                    let channel_id = state.channel_id.unwrap();
                    // All frame PDUs combined into a single ZGFX-wrapped buffer
                    let pdu_data = GfxHandler::create_frame_pdu(&mut state, gfx_frame);
                    drop(state); // release lock before async

                    // Send as a single DVC message (auto-fragmented by encode_dvc_messages)
                    let dvc_messages: Vec<dvc::DvcMessage> = vec![Box::new(crate::gfx::RawGfxPdu(pdu_data))];
                    let svc_messages = dvc::encode_dvc_messages(
                        channel_id,
                        dvc_messages,
                        ChannelFlags::SHOW_PROTOCOL,
                    ).context("Failed to encode DVC messages")?;

                    let data = server_encode_svc_messages(
                        svc_messages.into(),
                        drdynvc_id,
                        user_channel_id,
                    )?;
                    writer.write_all(&data).await
                        .context("failed to write GFX frame")?;

                    return Ok((RunState::Continue, encoder));
                }
            } else {
                drop(state);
                // GFX not ready yet, fall through to bitmap encoding
                trace!("GFX not ready, falling back to bitmap path");
            }
        }

        // Handle GFX uncompressed / RemoteFX dirty-region updates through the DVC channel
        if matches!(update, DisplayUpdate::GfxUncompressed(_) | DisplayUpdate::GfxRemoteFx(_)) {
            let mut state = gfx_state.lock().unwrap();
            if state.is_ready() {
                if let Some(drdynvc_id) = drdynvc_channel_id {
                    let channel_id = state.channel_id.unwrap();
                    let pdu_data = match &update {
                        DisplayUpdate::GfxRemoteFx(rfx) => GfxHandler::create_remotefx_pdu(&mut state, rfx),
                        DisplayUpdate::GfxUncompressed(raw) => GfxHandler::create_uncompressed_pdu(&mut state, raw),
                        _ => unreachable!("matched above"),
                    };
                    drop(state);

                    let dvc_messages: Vec<dvc::DvcMessage> = vec![Box::new(crate::gfx::RawGfxPdu(pdu_data))];
                    let svc_messages = dvc::encode_dvc_messages(
                        channel_id,
                        dvc_messages,
                        ChannelFlags::SHOW_PROTOCOL,
                    ).context("Failed to encode DVC messages")?;

                    let data = server_encode_svc_messages(
                        svc_messages.into(),
                        drdynvc_id,
                        user_channel_id,
                    )?;
                    writer.write_all(&data).await
                        .context("failed to write GFX uncompressed frame")?;

                    return Ok((RunState::Continue, encoder));
                }
            } else {
                drop(state);
                trace!("GFX not ready for uncompressed, falling back to bitmap path");
            }
        }

        let mut encoder_iter = encoder.update(update);
        loop {
            let Some(fragmenter) = encoder_iter.next().await else {
                break;
            };

            let mut fragmenter = fragmenter.context("error while encoding")?;
            if fragmenter.size_hint() > buffer.len() {
                buffer.resize(fragmenter.size_hint(), 0);
            }

            while let Some(len) = fragmenter.next(buffer) {
                writer
                    .write_all(&buffer[..len])
                    .await
                    .context("failed to write display update")?;
            }
        }

        Ok((RunState::Continue, encoder))
    }

    async fn dispatch_server_events(
        &mut self,
        events: &mut Vec<ServerEvent>,
        writer: &mut impl FramedWrite,
        user_channel_id: u16,
    ) -> Result<RunState> {
        // Avoid wave message queuing up and causing extra delays.
        // This is a naive solution, better solutions should compute the actual delay, add IO priority, encode audio, use UDP etc.
        // 4 frames should roughly corresponds to hundreds of ms in regular setups.
        let mut wave_limit = 4;
        for event in events.drain(..) {
            trace!(?event, "Dispatching");
            match event {
                ServerEvent::Quit(reason) => {
                    debug!("Got quit event: {reason}");
                    return Ok(RunState::Disconnect);
                }
                ServerEvent::Disconnect => {
                    info!("Disconnect requested by the host");
                    return Ok(RunState::Disconnect);
                }
                ServerEvent::GetLocalAddr(tx) => {
                    let _ = tx.send(self.local_addr);
                }
                ServerEvent::Rdpsnd(s) => {
                    let Some(rdpsnd) = self.get_svc_processor::<RdpsndServer>() else {
                        warn!("No rdpsnd channel, dropping event");
                        continue;
                    };
                    let msgs = match s {
                        RdpsndServerMessage::Wave(data, ts) => {
                            if wave_limit == 0 {
                                debug!("Dropping wave");
                                continue;
                            }
                            wave_limit -= 1;
                            match rdpsnd.wave(data, ts) {
                                Ok(msgs) => msgs,
                                Err(e) => {
                                    // Wave before handshake complete — drop silently
                                    debug!("Dropping wave: {e}");
                                    continue;
                                }
                            }
                        }
                        RdpsndServerMessage::SetVolume { left, right } => {
                            rdpsnd.set_volume(left, right).context("failed to send rdpsnd event")?
                        }
                        RdpsndServerMessage::Close => {
                            rdpsnd.close().context("failed to send rdpsnd event")?
                        }
                        RdpsndServerMessage::Error(error) => {
                            error!(?error, "Handling rdpsnd event");
                            continue;
                        }
                    };
                    let channel_id = self
                        .get_channel_id_by_type::<RdpsndServer>()
                        .ok_or_else(|| anyhow!("SVC channel not found"))?;
                    let data = server_encode_svc_messages(msgs.into(), channel_id, user_channel_id)?;
                    writer.write_all(&data).await?;
                }
                ServerEvent::Clipboard(c) => {
                    let Some(cliprdr) = self.get_svc_processor::<CliprdrServer>() else {
                        warn!("No clipboard channel, dropping event");
                        continue;
                    };
                    let msgs = match c {
                        ClipboardMessage::SendInitiateCopy(formats) => cliprdr.initiate_copy(&formats),
                        ClipboardMessage::SendFormatData(data) => cliprdr.submit_format_data(data),
                        ClipboardMessage::SendInitiatePaste(format) => cliprdr.initiate_paste(format),
                        ClipboardMessage::Error(error) => {
                            error!(?error, "Handling clipboard event");
                            continue;
                        }
                    }
                    .context("failed to send clipboard event")?;
                    let channel_id = self
                        .get_channel_id_by_type::<CliprdrServer>()
                        .ok_or_else(|| anyhow!("SVC channel not found"))?;
                    let data = server_encode_svc_messages(msgs.into(), channel_id, user_channel_id)?;
                    writer.write_all(&data).await?;
                }
                ServerEvent::ClipboardFileContents(response) => {
                    let Some(cliprdr) = self.get_svc_processor::<CliprdrServer>() else {
                        warn!("No clipboard channel, dropping file contents response");
                        continue;
                    };
                    let msgs = cliprdr
                        .submit_file_contents(response)
                        .context("failed to submit file contents")?;
                    let channel_id = self
                        .get_channel_id_by_type::<CliprdrServer>()
                        .ok_or_else(|| anyhow!("SVC channel not found"))?;
                    let data = server_encode_svc_messages(msgs.into(), channel_id, user_channel_id)?;
                    writer.write_all(&data).await?;
                }
                ServerEvent::ClipboardFileContentsRequest(request) => {
                    let channel_id = self
                        .get_channel_id_by_type::<CliprdrServer>()
                        .ok_or_else(|| anyhow!("SVC channel not found"))?;
                    let pdu = ClipboardPdu::FileContentsRequest(request);
                    let svc_msg = encode_cliprdr_pdu(pdu);
                    let data = server_encode_svc_messages(
                        vec![svc_msg].into(),
                        channel_id,
                        user_channel_id,
                    )?;
                    writer.write_all(&data).await?;
                }
            }
        }

        Ok(RunState::Continue)
    }

    async fn client_loop<R, W>(
        &mut self,
        reader: &mut Framed<R>,
        writer: &mut Framed<W>,
        io_channel_id: u16,
        user_channel_id: u16,
        mut encoder: UpdateEncoder,
    ) -> Result<RunState>
    where
        R: FramedRead,
        W: FramedWrite,
    {
        debug!("Starting client loop");
        let mut display_updates = self.display.lock().await.updates().await?;
        let mut writer = SharedWriter::new(writer);
        let mut display_writer = writer.clone();
        let mut event_writer = writer.clone();
        let ev_receiver = Arc::clone(&self.ev_receiver);
        let gfx_state = Arc::clone(&self.gfx_state);

        // Update GFX state with current desktop size
        {
            let size = self.display.lock().await.size().await;
            let mut gs = self.gfx_state.lock().unwrap();
            gs.width = size.width;
            gs.height = size.height;
        }

        // Get DRDYNVC channel ID for sending GFX frames
        let drdynvc_channel_id = self.get_channel_id_by_type::<dvc::DrdynvcServer>();
        let idle_timeout = self.idle_timeout;

        let s = Rc::new(Mutex::new(self));

        let this = Rc::clone(&s);
        let dispatch_pdu = async move {
            loop {
                let (action, bytes) = match idle_timeout {
                    Some(limit) => tokio::time::timeout(limit, reader.read_pdu())
                        .await
                        .map_err(|_| anyhow!("idle timeout: no client activity for {limit:?}"))??,
                    None => reader.read_pdu().await?,
                };
                let mut this = this.lock().await;
                match this
                    .dispatch_pdu(action, bytes, &mut writer, io_channel_id, user_channel_id)
                    .await?
                {
                    RunState::Continue => continue,
                    state => break Ok(state),
                }
            }
        };

        let dispatch_display = async move {
            let mut buffer = vec![0u8; 4096];

            loop {
                match display_updates.next_update().await {
                    Ok(Some(update)) => {
                        match Self::dispatch_display_update(
                            update,
                            &mut display_writer,
                            user_channel_id,
                            io_channel_id,
                            &mut buffer,
                            encoder,
                            &gfx_state,
                            drdynvc_channel_id,
                        )
                        .await?
                        {
                            (RunState::Continue, enc) => {
                                encoder = enc;
                                continue;
                            }
                            (state, _) => {
                                break Ok(state);
                            }
                        }
                    }
                    Ok(None) => {
                        break Ok(RunState::Disconnect);
                    }
                    Err(error) => {
                        warn!(error = format!("{error:#}"), "next_updated failed");
                    }
                }
            }
        };

        let this = Rc::clone(&s);
        let mut ev_receiver = ev_receiver.lock().await;
        let dispatch_events = async move {
            let mut events = Vec::with_capacity(100);
            loop {
                let nevents = ev_receiver.recv_many(&mut events, 100).await;
                if nevents == 0 {
                    debug!("No sever events.. stopping");
                    break Ok(RunState::Disconnect);
                }
                while let Ok(ev) = ev_receiver.try_recv() {
                    events.push(ev);
                }
                let mut this = this.lock().await;
                match this
                    .dispatch_server_events(&mut events, &mut event_writer, user_channel_id)
                    .await?
                {
                    RunState::Continue => continue,
                    state => break Ok(state),
                }
            }
        };

        let state = tokio::select!(
            state = dispatch_pdu => state,
            state = dispatch_display => state,
            state = dispatch_events => state,
        );

        debug!("End of client loop: {state:?}");
        state
    }

    async fn client_accepted<R, W>(
        &mut self,
        reader: &mut Framed<R>,
        writer: &mut Framed<W>,
        result: AcceptorResult,
    ) -> Result<RunState>
    where
        R: FramedRead,
        W: FramedWrite,
    {
        debug!("Client accepted");

        if !result.input_events.is_empty() {
            debug!("Handling input event backlog from acceptor sequence");
            self.handle_input_backlog(
                writer,
                result.io_channel_id,
                result.user_channel_id,
                result.input_events,
            )
            .await?;
        }

        self.static_channels = result.static_channels;
        if !result.reactivation {
            // Before the capabilities below size the session.
            self.display.lock().await.begin_session(result.desktop_scale_factor);
            self.gfx_state.lock().unwrap().client_name = result.client_name;
            for (_type_id, channel, channel_id) in self.static_channels.iter_mut() {
                debug!(?channel, ?channel_id, "Start");
                let Some(channel_id) = channel_id else {
                    continue;
                };
                let svc_responses = channel.start()?;
                let response = server_encode_svc_messages(svc_responses, channel_id, result.user_channel_id)?;
                writer.write_all(&response).await?;
            }
        }

        let mut update_codecs = UpdateEncoderCodecs::new();
        let mut surface_flags = CmdFlags::empty();
        for c in result.capabilities {
            match c {
                CapabilitySet::General(c) => {
                    let fastpath = c.extra_flags.contains(GeneralExtraFlags::FASTPATH_OUTPUT_SUPPORTED);
                    if !fastpath {
                        bail!("Fastpath output not supported!");
                    }
                }
                CapabilitySet::Bitmap(b) => {
                    let client_size = DesktopSize {
                        width: b.desktop_width,
                        height: b.desktop_height,
                    };
                    let display_size = self.display.lock().await.size().await;

                    if client_size.width != display_size.width || client_size.height != display_size.height {
                        info!(
                            client_w = client_size.width, client_h = client_size.height,
                            server_w = display_size.width, server_h = display_size.height,
                            "Client requested different resolution"
                        );
                        // Adopt client resolution via request_resize
                        self.display.lock().await.request_resize(
                            client_size.width, client_size.height,
                        );
                    }
                }
                CapabilitySet::SurfaceCommands(c) => {
                    surface_flags = c.flags;
                }
                CapabilitySet::BitmapCodecs(BitmapCodecs(codecs)) => {
                    for codec in codecs {
                        match codec.property {
                            // FIXME: The encoder operates in image mode only.
                            //
                            // See [MS-RDPRFX] 3.1.1.1 "State Machine" for
                            // implementation of the video mode. which allows to
                            // skip sending Header for each image.
                            //
                            // We should distinguish parameters for both modes,
                            // and somehow choose the "best", instead of picking
                            // the last parsed here.
                            CodecProperty::RemoteFx(rdp::capability_sets::RemoteFxContainer::ClientContainer(c))
                                if self.opts.has_remote_fx() =>
                            {
                                for caps in c.caps_data.0 .0 {
                                    update_codecs.set_remotefx(Some((caps.entropy_bits, codec.id)));
                                }
                            }
                            CodecProperty::ImageRemoteFx(rdp::capability_sets::RemoteFxContainer::ClientContainer(
                                c,
                            )) if self.opts.has_image_remote_fx() => {
                                for caps in c.caps_data.0 .0 {
                                    update_codecs.set_remotefx(Some((caps.entropy_bits, codec.id)));
                                }
                            }
                            CodecProperty::NsCodec(_) => (),
                            #[cfg(feature = "qoi")]
                            CodecProperty::Qoi if self.opts.has_qoi() => {
                                update_codecs.set_qoi(Some(codec.id));
                            }
                            #[cfg(feature = "qoiz")]
                            CodecProperty::QoiZ if self.opts.has_qoiz() => {
                                update_codecs.set_qoiz(Some(codec.id));
                            }
                            _ => (),
                        }
                    }
                }
                _ => {}
            }
        }

        let desktop_size = self.display.lock().await.size().await;
        let encoder = UpdateEncoder::new(desktop_size, surface_flags, update_codecs)
            .context("failed to initialize update encoder")?;

        let state = self
            .client_loop(reader, writer, result.io_channel_id, result.user_channel_id, encoder)
            .await
            .context("client loop failure")?;

        Ok(state)
    }

    async fn handle_input_backlog(
        &mut self,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
        frames: Vec<Vec<u8>>,
    ) -> Result<()> {
        for frame in frames {
            match Action::from_fp_output_header(frame[0]) {
                Ok(Action::FastPath) => {
                    let input = decode(&frame)?;
                    self.handle_fastpath(input).await;
                }

                Ok(Action::X224) => {
                    let _ = self.handle_x224(writer, io_channel_id, user_channel_id, &frame).await;
                }

                // the frame here is always valid, because otherwise it would
                // have failed during the acceptor loop
                Err(_) => unreachable!(),
            }
        }

        Ok(())
    }

    async fn handle_fastpath(&mut self, input: FastPathInput) {
        for event in input.input_events().iter().copied() {
            let mut handler = self.handler.lock().await;
            match event {
                FastPathInputEvent::KeyboardEvent(flags, key) => {
                    handler.keyboard((key, flags).into());
                }

                FastPathInputEvent::UnicodeKeyboardEvent(flags, key) => {
                    handler.keyboard((key, flags).into());
                }

                FastPathInputEvent::SyncEvent(flags) => {
                    handler.keyboard(flags.into());
                }

                FastPathInputEvent::MouseEvent(mouse) => {
                    handler.mouse(mouse.into());
                }

                FastPathInputEvent::MouseEventEx(mouse) => {
                    handler.mouse(mouse.into());
                }

                FastPathInputEvent::MouseEventRel(mouse) => {
                    handler.mouse(mouse.into());
                }

                FastPathInputEvent::QoeEvent(quality) => {
                    warn!("Received QoE: {}", quality);
                }
            }
        }
    }

    async fn handle_io_channel_data(&mut self, data: SendDataRequest<'_>) -> Result<bool> {
        let control: rdp::headers::ShareControlHeader = decode(data.user_data.as_ref())?;

        match control.share_control_pdu {
            ShareControlPdu::Data(header) => match header.share_data_pdu {
                rdp::headers::ShareDataPdu::Input(pdu) => {
                    self.handle_input_event(pdu).await;
                }

                rdp::headers::ShareDataPdu::ShutdownRequest => {
                    return Ok(true);
                }

                rdp::headers::ShareDataPdu::RefreshRectangle(_) => {
                    debug!("Client RefreshRect: requesting IDR");
                    self.gfx_state.lock().unwrap().request_idr();
                }

                rdp::headers::ShareDataPdu::SuppressOutput(pdu) => {
                    if pdu.desktop_rect.is_some() {
                        debug!("Client resumed display output: requesting IDR");
                        self.gfx_state.lock().unwrap().request_idr();
                    } else {
                        // Pausing output is not implemented; frames keep flowing.
                        debug!("Client suppressed display output");
                    }
                }

                unexpected => {
                    warn!(?unexpected, "Unexpected share data pdu");
                }
            },

            unexpected => {
                warn!(?unexpected, "Unexpected share control");
            }
        }

        Ok(false)
    }

    async fn handle_x224(
        &mut self,
        writer: &mut impl FramedWrite,
        io_channel_id: u16,
        user_channel_id: u16,
        frame: &[u8],
    ) -> Result<bool> {
        let message = decode::<X224<mcs::McsMessage<'_>>>(frame)?;
        match message.0 {
            mcs::McsMessage::SendDataRequest(data) => {
                debug!(?data, "McsMessage::SendDataRequest");
                if data.channel_id == io_channel_id {
                    return self.handle_io_channel_data(data).await;
                }

                if let Some(svc) = self.static_channels.get_by_channel_id_mut(data.channel_id) {
                    let response_pdus = svc.process(&data.user_data)?;
                    let response = server_encode_svc_messages(response_pdus, data.channel_id, user_channel_id)?;
                    writer.write_all(&response).await?;
                } else {
                    warn!(channel_id = data.channel_id, "Unexpected channel received: ID",);
                }
            }

            mcs::McsMessage::DisconnectProviderUltimatum(disconnect) => {
                if disconnect.reason == mcs::DisconnectReason::UserRequested {
                    return Ok(true);
                }
            }

            _ => {
                warn!(name = ironrdp_core::name(&message), "Unexpected mcs message");
            }
        }

        Ok(false)
    }

    async fn handle_input_event(&mut self, input: InputEventPdu) {
        for event in input.0 {
            let mut handler = self.handler.lock().await;
            match event {
                ironrdp_pdu::input::InputEvent::ScanCode(key) => {
                    handler.keyboard((key.key_code, key.flags).into());
                }

                ironrdp_pdu::input::InputEvent::Unicode(key) => {
                    handler.keyboard((key.unicode_code, key.flags).into());
                }

                ironrdp_pdu::input::InputEvent::Sync(sync) => {
                    handler.keyboard(sync.flags.into());
                }

                ironrdp_pdu::input::InputEvent::Mouse(mouse) => {
                    handler.mouse(mouse.into());
                }

                ironrdp_pdu::input::InputEvent::MouseX(mouse) => {
                    handler.mouse(mouse.into());
                }

                ironrdp_pdu::input::InputEvent::MouseRel(mouse) => {
                    handler.mouse(mouse.into());
                }

                ironrdp_pdu::input::InputEvent::Unused(_) => {}
            }
        }
    }

    async fn accept_finalize<S>(&mut self, mut framed: TokioFramed<S>, mut acceptor: Acceptor) -> Result<TokioFramed<S>>
    where
        S: AsyncRead + AsyncWrite + Sync + Send + Unpin,
    {
        loop {
            let (new_framed, result) =
                tokio::time::timeout(self.handshake_timeout, ironrdp_acceptor::accept_finalize(framed, &mut acceptor))
                    .await
                    .context("connect sequence timed out")?
                    .context("failed to accept client during finalize")?;

            let (mut reader, mut writer) = split_tokio_framed(new_framed);

            match self.client_accepted(&mut reader, &mut writer, result).await? {
                RunState::Continue => {
                    unreachable!();
                }
                RunState::DeactivationReactivation { desktop_size } => {
                    // No description of such behavior was found in the
                    // specification, but apparently, we must keep the channel
                    // state as they were during reactivation. This fixes
                    // various state issues during client resize.
                    acceptor = Acceptor::new_deactivation_reactivation(
                        acceptor,
                        core::mem::take(&mut self.static_channels),
                        desktop_size,
                    )?;
                    framed = unsplit_tokio_framed(reader, writer);
                    continue;
                }
                RunState::Disconnect => {
                    let final_framed = unsplit_tokio_framed(reader, writer);
                    return Ok(final_framed);
                }
            }
        }
    }

    /// Hooks the start and end of each active session (keep awake, lock on end).
    pub fn set_session_lifecycle(&mut self, lifecycle: Arc<dyn SessionLifecycle>) {
        self.session_lifecycle = Some(lifecycle);
    }

    /// Sets who may connect. Without an authenticator [`RdpServer::run`] refuses to start.
    pub fn set_authenticator(&mut self, auth: Arc<dyn RdpAuthenticator>) {
        self.auth = Some(auth);
    }

    /// Time allowed for the connect sequence after authentication, and for each resize
    /// reactivation. Negotiation (3 s), TLS (3 s) and CredSSP (30 s) have fixed budgets.
    pub fn set_handshake_timeout(&mut self, timeout: Duration) {
        self.handshake_timeout = timeout;
    }

    /// Disconnects a session that sends nothing for this long. `None` disables it.
    pub fn set_idle_timeout(&mut self, timeout: Option<Duration>) {
        self.idle_timeout = timeout;
    }
}

async fn deactivate_all(
    io_channel_id: u16,
    user_channel_id: u16,
    writer: &mut impl FramedWrite,
) -> Result<(), anyhow::Error> {
    let pdu = ShareControlPdu::ServerDeactivateAll(ServerDeactivateAll);
    let pdu = rdp::headers::ShareControlHeader {
        share_id: 0,
        pdu_source: io_channel_id,
        share_control_pdu: pdu,
    };
    let user_data = encode_vec(&pdu)?.into();
    let pdu = SendDataIndication {
        initiator_id: user_channel_id,
        channel_id: io_channel_id,
        user_data,
    };
    let msg = encode_vec(&X224(pdu))?;
    writer.write_all(&msg).await?;
    Ok(())
}

fn encode_cliprdr_pdu(pdu: ClipboardPdu<'static>) -> SvcMessage {
    SvcMessage::from(pdu).with_flags(ChannelFlags::SHOW_PROTOCOL)
}

struct SharedWriter<'w, W: FramedWrite> {
    writer: Rc<Mutex<&'w mut W>>,
}

impl<W: FramedWrite> Clone for SharedWriter<'_, W> {
    fn clone(&self) -> Self {
        Self {
            writer: Rc::clone(&self.writer),
        }
    }
}

impl<W> FramedWrite for SharedWriter<'_, W>
where
    W: FramedWrite,
{
    type WriteAllFut<'write>
        = Pin<Box<dyn core::future::Future<Output = std::io::Result<()>> + 'write>>
    where
        Self: 'write;

    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> Self::WriteAllFut<'a> {
        Box::pin(async {
            let mut writer = self.writer.lock().await;

            writer.write_all(buf).await?;
            Ok(())
        })
    }
}

impl<'a, W: FramedWrite> SharedWriter<'a, W> {
    fn new(writer: &'a mut W) -> Self {
        Self {
            writer: Rc::new(Mutex::new(writer)),
        }
    }
}

/// Connect sequence after authentication (and each resize reactivation). The stages
/// before it have their own budgets: [`X224_TIMEOUT`], [`TLS_TIMEOUT`], [`CREDSSP_TIMEOUT`].
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// One X.224 connection request: a real client sends it at once.
const X224_TIMEOUT: Duration = Duration::from_secs(3);
/// TLS handshake; the client's certificate prompt comes after it, inside CredSSP.
const TLS_TIMEOUT: Duration = Duration::from_secs(3);
/// CredSSP, generous because the client may wait on the user (certificate prompt).
const CREDSSP_TIMEOUT: Duration = Duration::from_secs(30);

/// Handshakes in flight at once; when full, the oldest unauthenticated one is evicted.
const MAX_HANDSHAKES: usize = 8;
/// Handshakes in flight from one source IP; past it, that IP's oldest unauthenticated one is
/// evicted (a client's idle probe must not refuse its next connection), else it is refused.
const MAX_HANDSHAKES_PER_IP: usize = 2;

#[derive(Debug, PartialEq, Eq)]
enum Admission {
    Accept,
    /// Accept after evicting the handshake with this id.
    AcceptEvicting(u64),
    Refuse,
}

struct Slot {
    id: u64,
    ip: IpAddr,
    /// Set once the authenticator accepted the client; such a handshake is never evicted.
    authenticated: Rc<Cell<bool>>,
    abort: AbortHandle,
}

/// Handshakes in flight, oldest first, so idle sockets cannot starve the owner.
#[derive(Default)]
struct HandshakeSlots {
    next_id: u64,
    slots: Vec<Slot>,
}

impl HandshakeSlots {
    fn admit(&self, ip: IpAddr) -> Admission {
        let mut same_ip = self.slots.iter().filter(|s| s.ip == ip);
        if same_ip.clone().count() >= MAX_HANDSHAKES_PER_IP {
            return match same_ip.find(|s| !s.authenticated.get()) {
                Some(oldest) => Admission::AcceptEvicting(oldest.id),
                None => Admission::Refuse,
            };
        }
        if self.slots.len() < MAX_HANDSHAKES {
            return Admission::Accept;
        }
        match self.slots.iter().find(|s| !s.authenticated.get()) {
            Some(oldest) => Admission::AcceptEvicting(oldest.id),
            None => Admission::Refuse,
        }
    }

    fn insert(&mut self, ip: IpAddr, authenticated: Rc<Cell<bool>>, abort: AbortHandle) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.slots.push(Slot { id, ip, authenticated, abort });
        id
    }

    fn remove(&mut self, id: u64) -> Option<Slot> {
        let index = self.slots.iter().position(|s| s.id == id)?;
        Some(self.slots.remove(index))
    }
}

/// A connection that passed negotiation, TLS and CredSSP.
struct Handshaked {
    framed: TokioFramed<TlsStream<TcpStream>>,
    acceptor: Acceptor,
    peer: SocketAddr,
}

/// Resolves to the slot id and, on success, the authenticated connection.
type HandshakeFuture = Pin<Box<dyn core::future::Future<Output = (u64, Option<Handshaked>)>>>;

struct HandshakeCtx {
    tls: TlsAcceptor,
    pub_key: Vec<u8>,
    security: nego::SecurityProtocol,
    auth: Arc<dyn RdpAuthenticator>,
}

fn start_handshake(
    ctx: &Rc<HandshakeCtx>,
    handshakes: &mut FuturesUnordered<HandshakeFuture>,
    slots: &mut HandshakeSlots,
    accepted: std::io::Result<(TcpStream, SocketAddr)>,
) {
    let (stream, peer) = match accepted {
        Ok(accepted) => accepted,
        Err(error) => {
            warn!(%error, "Accept failed");
            return;
        }
    };
    let admission = slots.admit(peer.ip());
    if admission == Admission::Refuse {
        warn!(%peer, "Too many handshakes in flight, closing connection");
        return;
    }
    if !ctx.auth.admit(peer.ip()) {
        warn!(%peer, "Source is in authentication backoff, closing connection");
        return;
    }
    if let Admission::AcceptEvicting(id) = admission {
        if let Some(evicted) = slots.remove(id) {
            // Not a failure: the evicted socket may be the owner's.
            info!(evicted = %evicted.ip, new = %peer, "Handshake slots busy, evicting the oldest unauthenticated one");
            evicted.abort.abort();
        }
    }
    set_keepalive(&stream);
    if let Err(error) = stream.set_nodelay(true) {
        debug!(%error, "Failed to set TCP_NODELAY");
    }
    let authenticated = Rc::new(Cell::new(false));
    let ctx = Rc::clone(ctx);
    let inner_authenticated = Rc::clone(&authenticated);
    let (future, abort) = abortable(async move {
        match handshake(&ctx, stream, peer, &inner_authenticated).await {
            Ok(hs) => Some(hs),
            Err(error) if error.is::<TlsIdle>() => {
                debug!(%peer, "Client left a negotiated connection idle before TLS (spare connection)");
                None
            }
            Err(error) => {
                warn!(%peer, error = format!("{error:#}"), "Handshake failed");
                None
            }
        }
    });
    let id = slots.insert(peer.ip(), authenticated, abort);
    handshakes.push(Box::pin(async move { (id, future.await.ok().flatten()) }));
}

/// The client negotiated, then did not finish TLS within [`TLS_TIMEOUT`]. The iOS client
/// opens such a spare connection beside each real one; no credential was tried, so it is
/// neither logged as a failure nor counted toward backoff (per-IP slots still bound it).
#[derive(Debug)]
struct TlsIdle;

impl core::fmt::Display for TlsIdle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("TLS accept timed out")
    }
}

impl std::error::Error for TlsIdle {}

/// Records a pre-authentication failure for `ip` (never a credential guess).
async fn report_handshake_failed(auth: &Arc<dyn RdpAuthenticator>, ip: IpAddr) {
    let auth = Arc::clone(auth);
    let _ = task::spawn_blocking(move || auth.handshake_failed(ip)).await;
}

/// Negotiation, TLS and CredSSP, then the authenticator's final decision.
///
/// Each stage has its own timeout. Failures and timeouts before authentication are
/// reported with `handshake_failed`, except an idle TLS stage ([`TlsIdle`]); a failed CredSSP exchange counts once as a
/// credential failure. `authenticated` is set when the authenticator accepted the client.
async fn handshake(
    ctx: &HandshakeCtx,
    stream: TcpStream,
    peer: SocketAddr,
    authenticated: &Rc<Cell<bool>>,
) -> Result<Handshaked> {
    debug!(%peer, "Received connection");
    let auth = Arc::clone(&ctx.auth);
    let identity = task::spawn_blocking(move || auth.ntlm_identity())
        .await?
        .context("no Mac account enrolled")?;
    let creds = ironrdp_acceptor::credssp::ntlm_hash_credentials(&identity.username, &identity.nt_hash);
    drop(identity);

    // Desktop size and capabilities are set when this becomes the active session.
    let size = DesktopSize { width: 0, height: 0 };
    let mut acceptor = Acceptor::new(ctx.security, size, Vec::new(), Some(creds));

    let peer_ip = peer.ip();
    let pre_auth = async {
        let begin = tokio::time::timeout(
            X224_TIMEOUT,
            ironrdp_acceptor::accept_begin(TokioFramed::new(stream), &mut acceptor),
        )
        .await
        .context("security negotiation timed out")?
        .context("security negotiation failed")?;
        let BeginResult::ShouldUpgrade(stream) = begin else {
            bail!("client did not negotiate NLA");
        };
        tokio::time::timeout(TLS_TIMEOUT, ctx.tls.accept(stream))
            .await
            .map_err(|_| anyhow::Error::new(TlsIdle))?
            .context("TLS accept failed")
    };
    let tls_stream = match pre_auth.await {
        Ok(tls_stream) => tls_stream,
        Err(error) => {
            if !error.is::<TlsIdle>() {
                report_handshake_failed(&ctx.auth, peer_ip).await;
            }
            return Err(error);
        }
    };
    let mut framed = TokioFramed::new(tls_stream);
    acceptor.mark_security_upgrade_as_done();

    let auth = Arc::clone(&ctx.auth);
    // authorize() records its own failure; CredSSP errors before it are recorded below.
    let authorize_ran = Rc::new(Cell::new(false));
    let authorize_ran_inner = Rc::clone(&authorize_ran);
    let authenticated_inner = Rc::clone(authenticated);
    let mut network_client = ironrdp_tokio::reqwest::ReqwestNetworkClient::new();
    let credssp = ironrdp_acceptor::accept_credssp(
        &mut framed,
        &mut acceptor,
        &mut network_client,
        peer.to_string().into(),
        ctx.pub_key.clone(),
        None,
        |delegated| async move {
            authorize_ran_inner.set(true);
            let username = delegated.username.account_name().to_owned();
            let domain = delegated.username.domain_name().unwrap_or_default().to_owned();
            let ok = task::spawn_blocking(move || auth.authorize(peer_ip, &username, &domain, delegated.password.as_ref()))
                .await
                .unwrap_or(false);
            authenticated_inner.set(ok);
            ok
        },
    );
    let Ok(result) = tokio::time::timeout(CREDSSP_TIMEOUT, credssp).await else {
        if !authorize_ran.get() {
            report_handshake_failed(&ctx.auth, peer_ip).await;
        }
        bail!("CredSSP timed out");
    };
    if let Err(error) = result {
        if !authorize_ran.get() {
            let auth = Arc::clone(&ctx.auth);
            let _ = task::spawn_blocking(move || auth.auth_failed(peer_ip)).await;
        }
        return Err(error).context("NLA authentication failed");
    }

    Ok(Handshaked { framed, acceptor, peer })
}

/// Detects dead peers (phone out of range) instead of holding the session forever.
fn set_keepalive(stream: &TcpStream) {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10));
    if let Err(error) = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive) {
        debug!(%error, "Failed to enable TCP keepalive");
    }
}

fn log_session_end(peer: SocketAddr, result: Result<()>) {
    if let Err(error) = result {
        let err_str = format!("{error:#}");
        if err_str.contains("reset by peer") || err_str.contains("Broken pipe") {
            warn!(%peer, "Client disconnected: {err_str}");
        } else {
            error!(%peer, error = err_str, "Connection error");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::KeyboardEvent;

    struct Recorder(Arc<std::sync::Mutex<Vec<u16>>>);

    impl RdpServerInputHandler for Recorder {
        fn keyboard(&mut self, _event: KeyboardEvent) {}

        fn mouse(&mut self, event: MouseEvent) {
            if let MouseEvent::Move { x, .. } = event {
                self.0.lock().expect("recorder lock").push(x);
            }
        }
    }

    struct LayoutRecorder(Arc<std::sync::Mutex<Vec<u32>>>);

    #[async_trait::async_trait]
    impl RdpServerDisplay for LayoutRecorder {
        async fn size(&mut self) -> DesktopSize {
            DesktopSize { width: 0, height: 0 }
        }

        async fn updates(&mut self) -> Result<Box<dyn crate::display::RdpServerDisplayUpdates>> {
            bail!("not used")
        }

        fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
            let width = layout.monitors()[0].dimensions().0;
            self.0.lock().expect("recorder lock").push(width);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn monitor_layouts_apply_in_arrival_order() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let display: Box<dyn RdpServerDisplay> = Box::new(LayoutRecorder(Arc::clone(&seen)));
        let display = Arc::new(Mutex::new(display));
        let backend = DisplayControlBackend::new(Arc::clone(&display));
        let expected: Vec<u32> = (0..100).map(|n| 200 + 2 * n).collect();
        {
            // Hold the display so every layout queues up behind it.
            let _busy = display.lock().await;
            for &width in &expected {
                let layout = DisplayControlMonitorLayout::new_single_primary_monitor(width, 600, None, None)
                    .expect("valid layout");
                backend.monitor_layout(layout);
            }
        }

        for _ in 0..200 {
            if seen.lock().expect("recorder lock").len() == expected.len() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(*seen.lock().expect("recorder lock"), expected);
    }

    fn display_control(seen: &Arc<std::sync::Mutex<Vec<u32>>>) -> DisplayControlChannel {
        let display: Box<dyn RdpServerDisplay> = Box::new(LayoutRecorder(Arc::clone(seen)));
        DisplayControlChannel { backend: DisplayControlBackend::new(Arc::new(Mutex::new(display))) }
    }

    #[tokio::test]
    async fn display_control_open_sends_caps_for_one_monitor_up_to_8192_square() {
        use dvc::DvcProcessor as _;
        let mut channel = display_control(&Arc::default());
        let msgs = channel.start(1).expect("start");
        assert_eq!(msgs.len(), 1);
        let bytes = encode_vec(msgs[0].as_ref()).expect("encode");
        // MS-RDPEDISP 2.2.2.1: header (type 5, length 20), MaxNumMonitors, AreaFactorA, AreaFactorB.
        let words: Vec<u32> = bytes.chunks(4).map(|c| u32::from_le_bytes(c.try_into().expect("u32"))).collect();
        assert_eq!(words, [5, 20, 1, 8192, 8192]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn display_control_forwards_monitor_layouts() {
        use dvc::DvcProcessor as _;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut channel = display_control(&seen);
        let layout = DisplayControlMonitorLayout::new_single_primary_monitor(1544, 816, Some(200), None).expect("layout");
        let pdu = encode_vec(&ironrdp_displaycontrol::pdu::DisplayControlPdu::from(layout)).expect("encode");
        assert!(channel.process(1, &pdu).expect("process").is_empty());
        for _ in 0..200 {
            if !seen.lock().expect("recorder lock").is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(*seen.lock().expect("recorder lock"), [1544]);
    }

    fn slot_ip(n: u8) -> IpAddr {
        IpAddr::from([192, 0, 2, n])
    }

    fn occupy(slots: &mut HandshakeSlots, ip: IpAddr) -> (u64, Rc<Cell<bool>>) {
        let authenticated = Rc::new(Cell::new(false));
        let id = slots.insert(ip, Rc::clone(&authenticated), AbortHandle::new_pair().0);
        (id, authenticated)
    }

    #[test]
    fn a_full_ip_evicts_its_own_oldest_unauthenticated_handshake() {
        let mut slots = HandshakeSlots::default();
        let (first, _) = occupy(&mut slots, slot_ip(1));
        occupy(&mut slots, slot_ip(1));
        // A client's idle probe from a moment ago must not refuse its real connection.
        assert_eq!(slots.admit(slot_ip(1)), Admission::AcceptEvicting(first));
        assert_eq!(slots.admit(slot_ip(2)), Admission::Accept);
    }

    #[test]
    fn a_full_ip_of_authenticated_handshakes_refuses() {
        let mut slots = HandshakeSlots::default();
        occupy(&mut slots, slot_ip(1)).1.set(true);
        occupy(&mut slots, slot_ip(1)).1.set(true);
        assert_eq!(slots.admit(slot_ip(1)), Admission::Refuse);
    }

    #[test]
    fn full_slots_evict_the_oldest_unauthenticated_handshake() {
        let mut slots = HandshakeSlots::default();
        let mut ids = Vec::new();
        for n in 0..MAX_HANDSHAKES {
            ids.push(occupy(&mut slots, slot_ip(u8::try_from(n / 2).expect("small"))));
        }
        // The oldest one already authenticated: it is about to become the session.
        ids[0].1.set(true);
        assert_eq!(slots.admit(slot_ip(100)), Admission::AcceptEvicting(ids[1].0));
    }

    #[test]
    fn full_slots_of_authenticated_handshakes_refuse() {
        let mut slots = HandshakeSlots::default();
        for n in 0..MAX_HANDSHAKES {
            occupy(&mut slots, slot_ip(u8::try_from(n / 2).expect("small"))).1.set(true);
        }
        assert_eq!(slots.admit(slot_ip(100)), Admission::Refuse);
    }

    #[test]
    fn removing_a_handshake_frees_its_slot_and_ip_count() {
        let mut slots = HandshakeSlots::default();
        let (first, _) = occupy(&mut slots, slot_ip(1));
        occupy(&mut slots, slot_ip(1));
        assert!(slots.remove(first).is_some());
        assert!(slots.remove(first).is_none());
        assert_eq!(slots.admit(slot_ip(1)), Admission::Accept);
    }

    #[tokio::test]
    async fn mouse_worker_keeps_event_order() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler: Box<dyn RdpServerInputHandler> = Box::new(Recorder(Arc::clone(&seen)));
        let tx = spawn_mouse_worker(Arc::new(Mutex::new(handler)));
        for x in 0..200u16 {
            tx.send(MouseEvent::Move { x, y: 0 }).expect("worker alive");
        }
        drop(tx);

        for _ in 0..200 {
            if seen.lock().expect("recorder lock").len() == 200 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let expected: Vec<u16> = (0..200).collect();
        assert_eq!(*seen.lock().expect("recorder lock"), expected);
    }
}
