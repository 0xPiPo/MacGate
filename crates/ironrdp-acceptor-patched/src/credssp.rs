use ironrdp_async::NetworkClient;
use ironrdp_connector::sspi::credssp::{
    CredSspServer, CredentialsProxy, ServerError, ServerMode, ServerState, TsRequest,
};
use ironrdp_connector::sspi::generator::{Generator, GeneratorState};
use ironrdp_connector::sspi::negotiate::ProtocolConfig;
use ironrdp_connector::sspi::{self, AuthIdentity, KerberosServerConfig, NegotiateConfig, NetworkRequest, Username};
use ironrdp_connector::{
    custom_err, general_err, ConnectorError, ConnectorErrorKind, ConnectorResult, ServerName, Written,
};
use ironrdp_core::{other_err, WriteBuf};
use ironrdp_pdu::rdp::client_info::Credentials;
use ironrdp_pdu::PduHint;
use tracing::debug;

#[derive(Debug)]
pub(crate) enum CredsspState {
    Ongoing,
    Finished,
    ServerError(sspi::Error),
}

#[derive(Clone, Copy, Debug)]
struct CredsspTsRequestHint;

const CREDSSP_TS_REQUEST_HINT: CredsspTsRequestHint = CredsspTsRequestHint;

impl PduHint for CredsspTsRequestHint {
    fn find_size(&self, bytes: &[u8]) -> ironrdp_core::DecodeResult<Option<(bool, usize)>> {
        match TsRequest::read_length(bytes) {
            Ok(length) => Ok(Some((true, length))),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(other_err!("CredsspTsRequestHint", source: e)),
        }
    }
}

pub type CredsspProcessGenerator<'a> =
    Generator<'a, NetworkRequest, sspi::Result<Vec<u8>>, Result<ServerState, ServerError>>;

#[derive(Debug)]
pub struct CredsspSequence<'a> {
    server: CredSspServer<CredentialsProxyImpl<'a>>,
    state: CredsspState,
    /// The credentials the client delegated (TSCredentials), once CredSSP finished.
    delegated: Option<AuthIdentity>,
}

/// Server credentials for NTLM that hold the NT hash instead of a password.
///
/// The hex is written straight into one pre-sized buffer, so no copy of it is left
/// behind; the acceptor zeroizes it after CredSSP.
pub fn ntlm_hash_credentials(username: &str, nt_hash: &[u8; 16]) -> Credentials {
    use core::fmt::Write as _;
    let mut password = String::with_capacity(sspi::NTLM_HASH_PREFIX.len() + 2 * nt_hash.len());
    password.push_str(sspi::NTLM_HASH_PREFIX);
    for byte in nt_hash {
        let _ = write!(password, "{byte:02x}");
    }
    Credentials {
        username: username.to_owned(),
        password,
        domain: None,
    }
}

#[derive(Debug)]
struct CredentialsProxyImpl<'a> {
    credentials: &'a AuthIdentity,
}

impl<'a> CredentialsProxyImpl<'a> {
    fn new(credentials: &'a AuthIdentity) -> Self {
        Self { credentials }
    }
}

impl CredentialsProxy for CredentialsProxyImpl<'_> {
    type AuthenticationData = AuthIdentity;

    fn auth_data_by_user(&mut self, username: &Username) -> std::io::Result<Self::AuthenticationData> {
        // Case is ignored like Windows does; the caller authorizes the exact account later.
        if username.account_name().to_lowercase() != self.credentials.username.account_name().to_lowercase() {
            return Err(std::io::Error::other("invalid username"));
        }

        let mut data = self.credentials.clone();
        // keep the original user/domain
        data.username = username.clone();
        Ok(data)
    }
}

pub(crate) async fn resolve_generator(
    generator: &mut CredsspProcessGenerator<'_>,
    network_client: &mut impl NetworkClient,
) -> Result<ServerState, ServerError> {
    let mut state = generator.start();

    loop {
        match state {
            GeneratorState::Suspended(request) => {
                let response = network_client.send(&request).await.map_err(|err| ServerError {
                    ts_request: None,
                    error: sspi::Error::new(sspi::ErrorKind::InternalError, err),
                })?;
                state = generator.resume(Ok(response));
            }
            GeneratorState::Completed(client_state) => break client_state,
        }
    }
}

impl<'a> CredsspSequence<'a> {
    pub fn next_pdu_hint(&self) -> ConnectorResult<Option<&dyn PduHint>> {
        match &self.state {
            CredsspState::Ongoing => Ok(Some(&CREDSSP_TS_REQUEST_HINT)),
            CredsspState::Finished => Ok(None),
            CredsspState::ServerError(err) => Err(custom_err!("Credssp server error", err.clone())),
        }
    }

    pub fn init(
        creds: &'a AuthIdentity,
        client_computer_name: ServerName,
        public_key: Vec<u8>,
        krb_config: Option<KerberosServerConfig>,
    ) -> ConnectorResult<Self> {
        let client_computer_name = client_computer_name.into_inner();
        let credentials = CredentialsProxyImpl::new(creds);

        let credssp_config: Box<dyn ProtocolConfig> = if let Some(krb_config) = krb_config {
            Box::new(krb_config)
        } else {
            Box::<sspi::ntlm::NtlmConfig>::default()
        };

        let server = CredSspServer::new(
            public_key,
            credentials,
            ServerMode::Negotiate(NegotiateConfig {
                protocol_config: credssp_config,
                package_list: None,
                client_computer_name,
            }),
        )
        .map_err(|e| ConnectorError::new("CredSSP", ConnectorErrorKind::Credssp(e)))?;

        let sequence = Self {
            server,
            state: CredsspState::Ongoing,
            delegated: None,
        };

        Ok(sequence)
    }

    /// Returns Some(ts_request) when a TS request is received from client,
    pub fn decode_client_message(&mut self, input: &[u8]) -> ConnectorResult<Option<TsRequest>> {
        match self.state {
            CredsspState::Ongoing => {
                let message = TsRequest::from_buffer(input).map_err(|e| custom_err!("TsRequest", e))?;
                debug!("Received TsRequest"); // contents carry NTLM responses: never log them
                Ok(Some(message))
            }
            _ => Err(general_err!(
                "attempted to feed client request to CredSSP sequence in an unexpected state"
            )),
        }
    }

    /// The delegated client credentials; `Some` once the sequence finished.
    pub fn take_delegated(&mut self) -> Option<AuthIdentity> {
        self.delegated.take()
    }

    pub fn process_ts_request(&mut self, request: TsRequest) -> CredsspProcessGenerator<'_> {
        self.server.process(request)
    }

    pub fn handle_process_result(
        &mut self,
        result: Result<ServerState, ServerError>,
        output: &mut WriteBuf,
    ) -> ConnectorResult<Written> {
        let (ts_request, next_state) = match result {
            Ok(ServerState::ReplyNeeded(ts_request)) => (Some(ts_request), CredsspState::Ongoing),
            Ok(ServerState::Finished(id)) => {
                self.delegated = Some(id);
                (None, CredsspState::Finished)
            }
            Err(err) => (
                err.ts_request.map(|ts_request| *ts_request),
                CredsspState::ServerError(err.error),
            ),
        };

        self.state = next_state;
        if let Some(ts_request) = ts_request {
            debug!("Send TsRequest");
            let length = usize::from(ts_request.buffer_len());
            let unfilled_buffer = output.unfilled_to(length);

            ts_request
                .encode_ts_request(unfilled_buffer)
                .map_err(|e| custom_err!("TsRequest", e))?;

            output.advance(length);

            Ok(Written::from_size(length)?)
        } else {
            Ok(Written::Nothing)
        }
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_connector::sspi::credssp::{ClientMode, ClientState, CredSspClient, CredSspMode};
    use ironrdp_connector::sspi::Credentials as SspiCredentials;

    use super::*;

    /// NT hash of "password" (well-known test vector).
    const PASSWORD_HASH: [u8; 16] = [
        0x88, 0x46, 0xF7, 0xEA, 0xEE, 0x8F, 0xB1, 0x17, 0xAD, 0x06, 0xBD, 0xD8, 0x30, 0xB7, 0x58, 0x6C,
    ];

    /// Runs a CredSSP/NTLM exchange in memory between an sspi client and the acceptor's
    /// server side. Returns what the server received as delegated credentials.
    fn exchange(client_user: &str, client_password: &str) -> Result<AuthIdentity, String> {
        exchange_with(client_user, client_password, |_| {})
    }

    /// [`exchange`], with `tamper` applied to every client message before the server reads it.
    fn exchange_with(
        client_user: &str,
        client_password: &str,
        mut tamper: impl FnMut(&mut TsRequest),
    ) -> Result<AuthIdentity, String> {
        let public_key = vec![0x42; 270];
        let server_creds = ntlm_hash_credentials("pipo", &PASSWORD_HASH);
        let server_identity = AuthIdentity {
            username: Username::new(&server_creds.username, None).map_err(|e| e.to_string())?,
            password: server_creds.password.clone().into(),
        };
        let mut server = CredsspSequence::init(&server_identity, ServerName::new("phone"), public_key.clone(), None)
            .map_err(|e| e.to_string())?;

        let client_identity = AuthIdentity {
            username: Username::parse(client_user).map_err(|e| e.to_string())?,
            password: client_password.to_owned().into(),
        };
        let mut client = CredSspClient::new(
            public_key,
            SspiCredentials::AuthIdentity(client_identity),
            CredSspMode::WithCredentials,
            ClientMode::Negotiate(NegotiateConfig {
                protocol_config: Box::<sspi::ntlm::NtlmConfig>::default(),
                package_list: Some("ntlm".to_owned()),
                client_computer_name: "phone".to_owned(),
            }),
            "TERMSRV/mac".to_owned(),
        )
        .map_err(|e| e.to_string())?;

        let mut to_client = TsRequest::default();
        for _ in 0..8 {
            let state = client
                .process(to_client)
                .resolve_to_result()
                .map_err(|e| e.to_string())?;
            let (ClientState::ReplyNeeded(mut to_server) | ClientState::FinalMessage(mut to_server)) = state;
            tamper(&mut to_server);

            let result = server.process_ts_request(to_server).resolve_to_result();
            let mut buf = WriteBuf::new();
            server.handle_process_result(result, &mut buf).map_err(|e| e.to_string())?;
            server.next_pdu_hint().map_err(|e| format!("{e:?}"))?;
            if let Some(delegated) = server.take_delegated() {
                return Ok(delegated);
            }
            to_client = TsRequest::from_buffer(buf.filled()).map_err(|e| e.to_string())?;
        }
        Err("exchange did not finish".to_owned())
    }

    #[test]
    fn ntlm_against_stored_hash_accepts_the_right_password() {
        let delegated = exchange("pipo", "password").expect("authentication succeeds");
        assert_eq!(delegated.username.account_name(), "pipo");
        assert_eq!(delegated.password.as_ref(), "password");
    }

    #[test]
    fn ntlm_against_stored_hash_ignores_username_case() {
        let delegated = exchange("PIPO", "password").expect("authentication succeeds");
        assert_eq!(delegated.username.account_name(), "PIPO");
    }

    #[test]
    fn ntlm_against_stored_hash_rejects_a_wrong_password() {
        assert!(exchange("pipo", "wrong").is_err());
    }

    #[test]
    fn ntlm_against_stored_hash_rejects_an_empty_password() {
        assert!(exchange("pipo", "").is_err());
    }

    #[test]
    fn ntlm_rejects_another_user() {
        assert!(exchange("root", "password").is_err());
    }

    fn u16_at(message: &[u8], at: usize) -> usize {
        usize::from(u16::from_le_bytes([message[at], message[at + 1]]))
    }

    /// Strips the MIC from an NTLM AUTHENTICATE message in `request`: clears the MIC bit of
    /// MsvAvFlags in the NTLMv2 response (MS-NLMP 2.2.2.1, 2.2.2.7) and zeroes the MIC field
    /// (2.2.1.3). Returns true when a MIC bit was found and cleared.
    fn strip_mic(request: &mut TsRequest) -> bool {
        let Some(token) = request.nego_tokens.as_mut() else {
            return false;
        };
        let Some(start) = token
            .windows(12)
            .position(|w| w[..8] == *b"NTLMSSP\0" && w[8..] == 3u32.to_le_bytes())
        else {
            return false;
        };
        let message = &mut token[start..];
        let nt_len = u16_at(message, 20);
        let nt_offset = usize::try_from(u32::from_le_bytes(message[24..28].try_into().expect("4 bytes")))
            .expect("offset fits usize");
        // Skip NTProofStr (16) and the NTLMv2 client challenge header (28) to the AV pairs.
        let mut at = nt_offset + 16 + 28;
        let end = nt_offset + nt_len;
        let mut cleared = false;
        while at + 4 <= end {
            let (id, len) = (u16_at(message, at), u16_at(message, at + 2));
            if id == 0x0000 {
                break; // MsvAvEOL
            }
            if id == 0x0006 {
                let value = &mut message[at + 4..at + 8];
                let flags = u32::from_le_bytes(value.try_into().expect("4 bytes"));
                cleared = flags & 0x2 != 0;
                value.copy_from_slice(&(flags & !0x2).to_le_bytes());
            }
            at += 4 + len;
        }
        message[72..88].fill(0);
        cleared
    }

    /// sspi checks the MIC only when the client announces one and never verifies
    /// NTProofStr, so a client can drop the MIC. A wrong password must still fail: the
    /// session key (and so the pubKeyAuth seal) derives from the real NT hash. The OD
    /// re-check in the authenticator stays mandatory on top of this.
    #[test]
    fn ntlm_refuses_wrong_password_without_mic() {
        assert!(exchange_with("pipo", "wrong", |r| {
            strip_mic(r);
        })
        .is_err());
    }

    /// Control: the tamper finds a MIC to strip and keeps the message parseable. Even the
    /// right password fails: sspi derives the key exchange key from an NTProofStr it
    /// recomputes over the received (now altered) blob, so the pubKeyAuth seal breaks.
    #[test]
    fn mic_strip_keeps_right_password_parseable() {
        let mut stripped = false;
        let error = exchange_with("pipo", "password", |r| stripped |= strip_mic(r)).expect_err("tampered blob");
        assert!(stripped, "the client sent a MIC");
        assert!(error.contains("MessageAltered"), "not a parse error: {error}");
    }
}
