//! Session lifecycle hooks the embedding application implements.

use core::net::SocketAddr;

/// Told when an authenticated connection becomes the active session.
pub trait SessionLifecycle: Send + Sync + 'static {
    /// Returns a guard that lives exactly as long as the active session.
    fn session_started(&self, peer: SocketAddr) -> Box<dyn SessionGuard>;
}

/// Dropping the guard ends the session: disconnect, error, idle timeout, server stop or
/// cancellation of the server future.
pub trait SessionGuard {
    /// A new authenticated connection takes over; called just before the guard drops.
    fn replaced(&mut self);
}
