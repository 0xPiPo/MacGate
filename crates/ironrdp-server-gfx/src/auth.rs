//! Authentication hooks the embedding application implements.

use core::fmt;
use core::net::IpAddr;

use zeroize::Zeroizing;

/// Account and NT hash the NTLM exchange runs against.
pub struct NtlmIdentity {
    pub username: String,
    pub nt_hash: Zeroizing<[u8; 16]>,
}

impl fmt::Debug for NtlmIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NtlmIdentity")
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// Decides who may open a session.
///
/// The server calls every method from a blocking-capable context
/// (`spawn_blocking`) except [`RdpAuthenticator::admit`], which must be cheap.
pub trait RdpAuthenticator: Send + Sync + 'static {
    /// Called on TCP accept. `false` closes the socket before any handshake.
    ///
    /// May consume a global admission token (when many guesses arrive at once), so call it
    /// only for a connection that will actually handshake.
    fn admit(&self, peer: IpAddr) -> bool;

    /// The identity CredSSP/NTLM checks the client against. `None` refuses everyone.
    fn ntlm_identity(&self) -> Option<NtlmIdentity>;

    /// Final decision after CredSSP succeeded, with the delegated plaintext credentials.
    fn authorize(&self, peer: IpAddr, username: &str, domain: &str, password: &str) -> bool;

    /// CredSSP failed (wrong password or protocol error during authentication). Not called
    /// when [`RdpAuthenticator::authorize`] already rejected the attempt.
    fn auth_failed(&self, peer: IpAddr);

    /// The peer failed before authentication: bad X.224, refused negotiation, TLS failure
    /// or a stage timeout. Not a credential guess, but it still costs the source.
    fn handshake_failed(&self, peer: IpAddr);
}
