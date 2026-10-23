//! End-to-end checks of the RDP accept path: NLA-only negotiation, handshake timeout,
//! concurrent handshakes and backoff. A real `RdpServer` listens on loopback.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ironrdp_server::{NtlmIdentity, RdpAuthenticator, RdpServer, TlsIdentityCtx};

struct TestAuth {
    admit: bool,
    handshake_failures: Arc<AtomicUsize>,
}

impl RdpAuthenticator for TestAuth {
    fn admit(&self, _peer: IpAddr) -> bool {
        self.admit
    }
    fn ntlm_identity(&self) -> Option<NtlmIdentity> {
        Some(NtlmIdentity { username: "pipo".into(), nt_hash: macrdp_core::auth::nt_hash("hunter2") })
    }
    fn authorize(&self, _peer: IpAddr, _username: &str, _domain: &str, _password: &str) -> bool {
        false
    }
    fn auth_failed(&self, _peer: IpAddr) {}
    fn handshake_failed(&self, _peer: IpAddr) {
        self.handshake_failures.fetch_add(1, Ordering::SeqCst);
    }
}

fn spawn_server(handshake_timeout: Duration, admit: bool) -> SocketAddr {
    spawn_server_counting(handshake_timeout, admit, Arc::default())
}

fn spawn_server_counting(handshake_timeout: Duration, admit: bool, handshake_failures: Arc<AtomicUsize>) -> SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let dir = tempfile::tempdir().unwrap();
            let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
            macrdp_core::tls::generate_self_signed_cert(&cert, &key).unwrap();
            let identity = TlsIdentityCtx::init_from_paths(&cert, &key).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let mut server = RdpServer::builder()
                .with_addr(addr)
                .with_hybrid(identity.make_acceptor().unwrap(), identity.pub_key.clone())
                .with_no_input()
                .with_no_display()
                .build();
            server.set_authenticator(Arc::new(TestAuth { admit, handshake_failures }));
            server.set_handshake_timeout(handshake_timeout);
            tx.send(addr).unwrap();
            server.run_on(listener).await.unwrap();
        });
    });
    rx.recv().unwrap()
}

const SSL: u32 = 0x1;
const STANDARD_RDP: u32 = 0x0;
const HYBRID_REQUIRED_BY_SERVER: u32 = 5;

/// TPKT + X.224 Connection Request + RDP_NEG_REQ asking for `protocols`.
fn connection_request(protocols: u32) -> Vec<u8> {
    let mut pdu = vec![0x03, 0x00, 0x00, 0x13, 0x0E, 0xE0, 0, 0, 0, 0, 0, 0x01, 0x00, 0x08, 0x00];
    pdu.extend_from_slice(&protocols.to_le_bytes());
    pdu
}

fn connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream
}

/// Sends a connection request and returns the RDP_NEG_FAILURE code, if the server sent one.
fn negotiate(addr: SocketAddr, protocols: u32) -> Option<u32> {
    let mut stream = connect(addr);
    stream.write_all(&connection_request(protocols)).unwrap();
    let mut response = [0u8; 19];
    stream.read_exact(&mut response).ok()?;
    // byte 11: RDP_NEG_* type; 0x03 = RDP_NEG_FAILURE
    (response[11] == 0x03).then(|| u32::from_le_bytes(response[15..19].try_into().unwrap()))
}

/// True when the server closed the connection (EOF or reset) before `within`.
fn closed_within(stream: &mut TcpStream, within: Duration) -> bool {
    stream.set_read_timeout(Some(within)).unwrap();
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe),
    }
}

#[test]
fn tls_only_client_is_told_nla_is_required() {
    let addr = spawn_server(Duration::from_secs(10), true);
    assert_eq!(negotiate(addr, SSL), Some(HYBRID_REQUIRED_BY_SERVER));
}

#[test]
fn standard_rdp_security_client_is_refused() {
    let addr = spawn_server(Duration::from_secs(10), true);
    assert_eq!(negotiate(addr, STANDARD_RDP), Some(HYBRID_REQUIRED_BY_SERVER));
}

#[test]
fn silent_client_is_dropped_after_the_x224_stage_timeout() {
    let failures = Arc::new(AtomicUsize::new(0));
    let addr = spawn_server_counting(Duration::from_secs(30), true, Arc::clone(&failures));
    let mut stream = connect(addr);
    let start = Instant::now();
    assert!(closed_within(&mut stream, Duration::from_secs(5)));
    assert!(start.elapsed() >= Duration::from_millis(2500));
    assert!(start.elapsed() < Duration::from_secs(4));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(failures.load(Ordering::SeqCst), 1, "a timeout is a pre-auth failure");
}

#[test]
fn third_concurrent_handshake_from_one_ip_evicts_that_ips_oldest() {
    let addr = spawn_server(Duration::from_secs(30), true);
    let mut first = connect(addr);
    let _second = connect(addr);
    std::thread::sleep(Duration::from_millis(100));
    let mut third = connect(addr);
    let start = Instant::now();
    assert!(closed_within(&mut first, Duration::from_secs(2)));
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(!closed_within(&mut third, Duration::from_millis(500)), "the new connection is served");
}

#[test]
fn idle_connection_after_negotiation_is_not_a_handshake_failure() {
    const HYBRID: u32 = 0x2;
    let failures = Arc::new(AtomicUsize::new(0));
    let addr = spawn_server_counting(Duration::from_secs(30), true, Arc::clone(&failures));
    let mut stream = connect(addr);
    stream.write_all(&connection_request(SSL | HYBRID)).unwrap();
    let mut response = [0u8; 19];
    stream.read_exact(&mut response).unwrap();
    assert_eq!(response[11], 0x02, "RDP_NEG_RSP");
    // No TLS ClientHello, like the iOS client's spare connection.
    assert!(closed_within(&mut stream, Duration::from_secs(5)));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(failures.load(Ordering::SeqCst), 0);
}

#[test]
fn stalled_handshake_does_not_block_other_clients() {
    let addr = spawn_server(Duration::from_secs(30), true);
    let _silent = connect(addr);
    std::thread::sleep(Duration::from_millis(100));
    let start = Instant::now();
    assert_eq!(negotiate(addr, SSL), Some(HYBRID_REQUIRED_BY_SERVER));
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn source_in_backoff_is_closed_before_handshake() {
    let addr = spawn_server(Duration::from_secs(30), false);
    let mut stream = connect(addr);
    assert!(closed_within(&mut stream, Duration::from_secs(2)));
}

#[test]
fn garbage_instead_of_x224_is_recorded_as_a_handshake_failure() {
    let failures = Arc::new(AtomicUsize::new(0));
    let addr = spawn_server_counting(Duration::from_secs(10), true, Arc::clone(&failures));
    let mut stream = connect(addr);
    stream.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
    assert!(closed_within(&mut stream, Duration::from_secs(3)));
    let start = Instant::now();
    while failures.load(Ordering::SeqCst) == 0 && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(failures.load(Ordering::SeqCst), 1);
}
