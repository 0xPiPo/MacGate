#![cfg_attr(doc, doc = include_str!("../README.md"))]
#![doc(html_logo_url = "https://cdnweb.devolutions.net/images/projects/devolutions/logos/devolutions-icon-shadow.svg")]

use ironrdp_async::{single_sequence_step, Framed, FramedRead, FramedWrite, NetworkClient, StreamWrapper};
use ironrdp_connector::sspi::credssp::EarlyUserAuthResult;
use ironrdp_connector::sspi::{AuthIdentity, KerberosServerConfig, Username};
use ironrdp_connector::{custom_err, general_err, ConnectorResult, ServerName};
use ironrdp_core::WriteBuf;
use tracing::{debug, instrument, trace};

mod channel_connection;
mod connection;
pub mod credssp;
mod finalization;
mod util;

pub use ironrdp_connector::DesktopSize;
use ironrdp_pdu::nego;

pub use self::channel_connection::{ChannelConnectionSequence, ChannelConnectionState};
pub use self::connection::{select_security_protocol, Acceptor, AcceptorResult, AcceptorState};
pub use self::finalization::{FinalizationSequence, FinalizationState};
use crate::credssp::resolve_generator;

pub enum BeginResult<S>
where
    S: StreamWrapper,
{
    ShouldUpgrade(S::InnerStream),
    Continue(Framed<S>),
}

pub async fn accept_begin<S>(mut framed: Framed<S>, acceptor: &mut Acceptor) -> ConnectorResult<BeginResult<S>>
where
    S: FramedRead + FramedWrite + StreamWrapper,
{
    let mut buf = WriteBuf::new();

    loop {
        if let Some(security) = acceptor.reached_security_upgrade() {
            let result = if security.is_empty() {
                BeginResult::Continue(framed)
            } else {
                BeginResult::ShouldUpgrade(framed.into_inner_no_leftover())
            };

            return Ok(result);
        }

        single_sequence_step(&mut framed, acceptor, &mut buf).await?;
    }
}

/// Runs CredSSP, then hands the delegated credentials to `authorize`.
///
/// The connection proceeds only when `authorize` returns true. With HYBRID_EX the
/// client learns the outcome (Early User Authorization Result) after `authorize`.
pub async fn accept_credssp<S, N, A, F>(
    framed: &mut Framed<S>,
    acceptor: &mut Acceptor,
    network_client: &mut N,
    client_computer_name: ServerName,
    public_key: Vec<u8>,
    kerberos_config: Option<KerberosServerConfig>,
    authorize: A,
) -> ConnectorResult<()>
where
    S: FramedRead + FramedWrite,
    N: NetworkClient,
    A: FnOnce(AuthIdentity) -> F,
    F: core::future::Future<Output = bool>,
{
    let mut buf = WriteBuf::new();

    if acceptor.should_perform_credssp() {
        perform_credssp_step(
            framed,
            acceptor,
            network_client,
            &mut buf,
            client_computer_name,
            public_key,
            kerberos_config,
            authorize,
        )
        .await
    } else {
        Ok(())
    }
}

pub async fn accept_finalize<S>(
    mut framed: Framed<S>,
    acceptor: &mut Acceptor,
) -> ConnectorResult<(Framed<S>, AcceptorResult)>
where
    S: FramedRead + FramedWrite,
{
    let mut buf = WriteBuf::new();

    loop {
        if let Some(result) = acceptor.get_result() {
            return Ok((framed, result));
        }
        single_sequence_step(&mut framed, acceptor, &mut buf).await?;
    }
}

#[instrument(level = "trace", skip_all, ret)]
#[expect(clippy::too_many_arguments)]
async fn perform_credssp_step<S, N, A, F>(
    framed: &mut Framed<S>,
    acceptor: &mut Acceptor,
    network_client: &mut N,
    buf: &mut WriteBuf,
    client_computer_name: ServerName,
    public_key: Vec<u8>,
    kerberos_config: Option<KerberosServerConfig>,
    authorize: A,
) -> ConnectorResult<()>
where
    S: FramedRead + FramedWrite,
    N: NetworkClient,
    A: FnOnce(AuthIdentity) -> F,
    F: core::future::Future<Output = bool>,
{
    assert!(acceptor.should_perform_credssp());
    let AcceptorState::Credssp { protocol, .. } = acceptor.state else {
        unreachable!()
    };

    let result = credssp_loop(
        framed,
        acceptor,
        network_client,
        buf,
        client_computer_name,
        public_key,
        kerberos_config,
    )
    .await;

    // The hash-derived password is needed only here: reactivation starts after CredSSP.
    if let Some(mut creds) = acceptor.creds.take() {
        zeroize::Zeroize::zeroize(&mut creds.password);
    }

    let result = match result {
        Ok(delegated) => {
            if authorize(delegated).await {
                Ok(())
            } else {
                Err(general_err!("delegated credentials rejected"))
            }
        }
        Err(e) => Err(e),
    };

    if protocol.intersects(nego::SecurityProtocol::HYBRID_EX) {
        trace!(?result, "HYBRID_EX");

        let result = if result.is_ok() {
            EarlyUserAuthResult::Success
        } else {
            EarlyUserAuthResult::AccessDenied
        };

        buf.clear();
        result
            .to_buffer(&mut *buf)
            .map_err(|e| ironrdp_connector::custom_err!("to_buffer", e))?;
        let response = &buf[..result.buffer_len()];
        framed
            .write_all(response)
            .await
            .map_err(|e| ironrdp_connector::custom_err!("write all", e))?;
    }

    result?;

    acceptor.mark_credssp_as_done();

    return Ok(());

    async fn credssp_loop<S, N>(
        framed: &mut Framed<S>,
        acceptor: &mut Acceptor,
        network_client: &mut N,
        buf: &mut WriteBuf,
        client_computer_name: ServerName,
        public_key: Vec<u8>,
        kerberos_config: Option<KerberosServerConfig>,
    ) -> ConnectorResult<AuthIdentity>
    where
        S: FramedRead + FramedWrite,
        N: NetworkClient,
    {
        let creds = acceptor
            .creds
            .as_ref()
            .ok_or_else(|| general_err!("no credentials while doing credssp"))?;
        let username = Username::new(&creds.username, None).map_err(|e| custom_err!("invalid username", e))?;
        let identity = AuthIdentity {
            username,
            password: creds.password.clone().into(),
        };

        let mut sequence =
            credssp::CredsspSequence::init(&identity, client_computer_name, public_key, kerberos_config)?;

        loop {
            let Some(next_pdu_hint) = sequence.next_pdu_hint()? else {
                break;
            };

            debug!(
                acceptor.state = ?acceptor.state,
                hint = ?next_pdu_hint,
                "Wait for PDU"
            );

            let pdu = framed
                .read_by_hint(next_pdu_hint)
                .await
                .map_err(|e| ironrdp_connector::custom_err!("read frame by hint", e))?;

            trace!(length = pdu.len(), "PDU received");

            let Some(ts_request) = sequence.decode_client_message(&pdu)? else {
                break;
            };

            let result = {
                let mut generator = sequence.process_ts_request(ts_request);
                resolve_generator(&mut generator, network_client).await
            }; // drop generator

            buf.clear();
            let written = sequence.handle_process_result(result, buf)?;

            if let Some(response_len) = written.size() {
                let response = &buf[..response_len];
                trace!(response_len, "Send response");
                framed
                    .write_all(response)
                    .await
                    .map_err(|e| ironrdp_connector::custom_err!("write all", e))?;
            }
        }

        sequence
            .take_delegated()
            .ok_or_else(|| general_err!("client sent no credentials"))
    }
}

#[cfg(test)]
mod tests {
    use core::future::{ready, Future, Ready};
    use core::task::{Context, Poll, Waker};
    use std::io;

    use ironrdp_async::bytes::BytesMut;
    use ironrdp_connector::sspi::generator::NetworkRequest;
    use ironrdp_pdu::rdp::client_info::Credentials;

    use super::*;

    /// A peer that closes the connection at once.
    struct ClosedStream;

    impl StreamWrapper for ClosedStream {
        type InnerStream = ();
        fn from_inner((): ()) -> Self {
            Self
        }
        fn into_inner(self) {}
        fn get_inner(&self) -> &() {
            &()
        }
        fn get_inner_mut(&mut self) -> &mut () {
            unreachable!()
        }
    }

    impl FramedRead for ClosedStream {
        type ReadFut<'read> = Ready<io::Result<usize>>;
        fn read<'a>(&'a mut self, _buf: &'a mut BytesMut) -> Self::ReadFut<'a> {
            ready(Ok(0))
        }
    }

    impl FramedWrite for ClosedStream {
        type WriteAllFut<'write> = Ready<io::Result<()>>;
        fn write_all<'a>(&'a mut self, _buf: &'a [u8]) -> Self::WriteAllFut<'a> {
            ready(Ok(()))
        }
    }

    struct NoNetwork;

    impl NetworkClient for NoNetwork {
        #[expect(clippy::manual_async_fn, reason = "trait signature")]
        fn send(&mut self, _request: &NetworkRequest) -> impl Future<Output = ConnectorResult<Vec<u8>>> {
            async { Err(general_err!("no network in tests")) }
        }
    }

    /// Polls a future that never waits on I/O.
    fn run_ready<F: Future>(future: F) -> F::Output {
        let mut future = core::pin::pin!(future);
        match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("future waited"),
        }
    }

    #[test]
    fn credssp_failure_clears_the_server_credentials() {
        let creds = Credentials { username: "pipo".to_owned(), password: "secret".to_owned(), domain: None };
        let mut acceptor = Acceptor::new(nego::SecurityProtocol::HYBRID, DesktopSize { width: 0, height: 0 }, Vec::new(), Some(creds));
        acceptor.state = AcceptorState::Credssp {
            requested_protocol: nego::SecurityProtocol::HYBRID,
            protocol: nego::SecurityProtocol::HYBRID,
        };
        let mut framed = Framed::<ClosedStream>::new(());
        let result = run_ready(accept_credssp(
            &mut framed,
            &mut acceptor,
            &mut NoNetwork,
            ServerName::new("phone"),
            vec![0x42; 270],
            None,
            |_| async { true },
        ));
        assert!(result.is_err());
        assert!(acceptor.creds.is_none());
    }
}
