use log::{debug, error, info, trace, warn};
use quinn::{ClientConfig, Connection, ConnectionError, Endpoint, EndpointConfig, IdleTimeout, SendStream, TokioRuntime, TransportConfig, VarInt, WriteError};
use solana_sdk::pubkey::Pubkey;
use std::{
    collections::VecDeque,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use anyhow::bail;
use futures::future::join_all;
use itertools::Itertools;
use solana_sdk::quic::QUIC_MAX_TIMEOUT_MS;
use tokio::{sync::RwLock, time::timeout};
use tokio::time::error::Elapsed;
use tracing::instrument;

const ALPN_TPU_PROTOCOL_ID: &[u8] = b"solana-tpu";

pub struct QuicConnectionUtils {}

pub enum QuicConnectionError {
    TimeOut,
    ConnectionError { retry: bool },
}

// TODO check whot we need from this
#[derive(Clone, Copy)]
pub struct QuicConnectionParameters {
    // pub connection_timeout: Duration,
    pub unistream_timeout: Duration,
    pub write_timeout: Duration,
    pub finalize_timeout: Duration,
    pub connection_retry_count: usize,
    // pub max_number_of_connections: usize,
    // pub number_of_transactions_per_unistream: usize,
}

impl QuicConnectionUtils {
    // TODO move to a more specific place
    pub fn create_tpu_client_endpoint(certificate: rustls::Certificate, key: rustls::PrivateKey) -> Endpoint {
        let mut endpoint = {
            let client_socket =
                solana_net_utils::bind_in_range(IpAddr::V4(Ipv4Addr::UNSPECIFIED), (8000, 10000))
                    .expect("create_endpoint bind_in_range")
                    .1;
            let config = EndpointConfig::default();
            quinn::Endpoint::new(config, None, client_socket, TokioRuntime)
                .expect("create_endpoint quinn::Endpoint::new")
        };

        let mut crypto = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_single_cert(vec![certificate], key)
            .expect("Failed to set QUIC client certificates");

        crypto.enable_early_data = true;

        crypto.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];

        let mut config = ClientConfig::new(Arc::new(crypto));

        // note: this should be aligned with solana quic server's endpoint config
        let mut transport_config = TransportConfig::default();
        // no remotely-initiated streams required
        transport_config.max_concurrent_uni_streams(VarInt::from_u32(0));
        transport_config.max_concurrent_bidi_streams(VarInt::from_u32(0));
        let timeout = IdleTimeout::try_from(Duration::from_millis(QUIC_MAX_TIMEOUT_MS as u64)).unwrap();
        transport_config.max_idle_timeout(Some(timeout));
        transport_config.keep_alive_interval(None);
        config.transport_config(Arc::new(transport_config));

        endpoint.set_default_client_config(config);

        endpoint
    }

    pub async fn make_connection(
        endpoint: Endpoint,
        addr: SocketAddr,
        connection_timeout: Duration,
    ) -> anyhow::Result<Connection> {
        let connecting = endpoint.connect(addr, "connect")?;
        let res = timeout(connection_timeout, connecting).await??;
        Ok(res)
    }

    pub async fn make_connection_0rtt(
        endpoint: Endpoint,
        addr: SocketAddr,
        connection_timeout: Duration,
    ) -> anyhow::Result<Connection> {
        let connecting = endpoint.connect(addr, "connect")?;
        let connection = match connecting.into_0rtt() {
            Ok((connection, zero_rtt)) => {
                if (timeout(connection_timeout, zero_rtt).await).is_ok() {
                    connection
                } else {
                    return Err(ConnectionError::TimedOut.into());
                }
            }
            Err(connecting) => {
                if let Ok(connecting_result) = timeout(connection_timeout, connecting).await {
                    connecting_result?
                } else {
                    return Err(ConnectionError::TimedOut.into());
                }
            }
        };
        Ok(connection)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        identity: Pubkey,
        already_connected: bool,
        endpoint: Endpoint,
        tpu_address: SocketAddr,
        connection_timeout: Duration,
        connection_retry_count: usize,
        exit_signal: Arc<AtomicBool>,
        on_connect: fn(),
    ) -> Option<Connection> {
        for _ in 0..connection_retry_count {
            let conn = if already_connected {
                Self::make_connection_0rtt(endpoint.clone(), tpu_address, connection_timeout).await
            } else {
                Self::make_connection(endpoint.clone(), tpu_address, connection_timeout).await
            };
            match conn {
                Ok(conn) => {
                    on_connect();
                    return Some(conn);
                }
                Err(e) => {
                    warn!("Could not connect to tpu {}/{}, error: {}", tpu_address, identity, e);
                    if exit_signal.load(Ordering::Relaxed) {
                        break;
                    }
                }
            }
        }
        None
    }

    pub async fn write_all(
        mut send_stream: SendStream,
        tx: &Vec<u8>,
        // identity: Pubkey,
        connection_params: QuicConnectionParameters,
    ) -> Result<(), QuicConnectionError> {
        let write_timeout_res = timeout(
            connection_params.write_timeout,
            send_stream.write_all(tx.as_slice()),
        )
            .await;
        match write_timeout_res {
            Ok(write_res) => {
                if let Err(e) = write_res {
                    trace!(
                        "Error while writing transaction for {}, error {}",
                        "identity",
                        e
                    );
                    return Err(QuicConnectionError::ConnectionError { retry: true });
                }
            }
            Err(_) => {
                warn!("timeout while writing transaction for {}", "identity");
                return Err(QuicConnectionError::TimeOut);
            }
        }

        let finish_timeout_res =
            timeout(connection_params.finalize_timeout, send_stream.finish()).await;
        match finish_timeout_res {
            Ok(finish_res) => {
                if let Err(e) = finish_res {
                    trace!(
                        "Error while finishing transaction for {}, error {}",
                        "identity",
                        e
                    );
                    return Err(QuicConnectionError::ConnectionError { retry: false });
                }
            }
            Err(_) => {
                warn!("timeout while finishing transaction for {}", "identity");
                return Err(QuicConnectionError::TimeOut);
            }
        }

        Ok(())
    }

    pub async fn write_all_simple(
        send_stream: &mut SendStream,
        tx: &Vec<u8>,
        connection_timeout: Duration,
    )  {
        let write_timeout_res =
            timeout(connection_timeout, send_stream.write_all(tx.as_slice())).await;
        match write_timeout_res {
            Ok(write_res) => {
                if let Err(e) = write_res {
                    trace!(
                        "Error while writing transaction for TBD, error {}",
                        // identity, // TODO add more context
                        e
                    );
                    return;
                }
            }
            Err(_) => {
                warn!("timeout while writing transaction for TBD"); // TODO add more context
                panic!("TODO handle timeout"); // FIXME
            }
        }

        let finish_timeout_res = timeout(connection_timeout, send_stream.finish()).await;
        match finish_timeout_res {
            Ok(finish_res) => {
                if let Err(e) = finish_res {
                    // last_stable_id.store(connection_stable_id, Ordering::Relaxed);
                    trace!(
                        "Error while writing transaction for TBD, error {}",
                        // identity,
                        e
                    );
                    return;
                }
            }
            Err(_) => {
                warn!("timeout while finishing transaction for TBD"); // TODO
                panic!("TODO handle timeout"); // FIXME
            }
        }

    }

    pub async fn open_unistream(
        connection: &Connection,
        connection_timeout: Duration,
    ) -> Result<SendStream, QuicConnectionError> {
        match timeout(connection_timeout, connection.open_uni()).await {
            Ok(Ok(unistream)) => Ok(unistream),
            Ok(Err(_)) => Err(QuicConnectionError::ConnectionError { retry: true }),
            Err(_) => Err(QuicConnectionError::TimeOut),
        }
    }

    pub async fn open_unistream_simple(
        connection: Connection,
        connection_timeout: Duration,
    ) -> (Option<SendStream>, bool) {
        match timeout(connection_timeout, connection.open_uni()).await {
            Ok(Ok(unistream)) => (Some(unistream), false),
            Ok(Err(_)) => {
                // reset connection for next retry
                (None, true)
            }
            // timeout
            Err(_) => (None, false),
        }
    }


    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(skip_all, level = "debug")]
    pub async fn send_transaction_batch_serial(
        connection: Connection,
        txs: Vec<Vec<u8>>,
        exit_signal: Arc<AtomicBool>,
        connection_timeout: Duration,
    ) {
        let (mut stream, _retry_conn) =
            Self::open_unistream_simple(connection.clone(), connection_timeout)
                .await;
        if let Some(ref mut send_stream) = stream {
            if exit_signal.load(Ordering::Relaxed) {
                return;
            }

            for tx in txs {
                let write_timeout_res =
                    timeout(connection_timeout, send_stream.write_all(tx.as_slice())).await;
                match write_timeout_res {
                    Ok(no_timeout) => {
                        match no_timeout {
                            Ok(()) => {}
                            Err(write_error) => {
                                error!("Error writing transaction to stream: {}", write_error);
                            }
                        }
                    }
                    Err(elapsed) => {
                        warn!("timeout sending transactions");
                    }
                }


            }
            // TODO wrap in timeout
            stream.unwrap().finish().await.unwrap();

        } else {
            panic!("no retry handling"); // FIXME
        }
    }

    // open streams in parallel
    // one stream is used for one transaction
    // number of parallel streams that connect to TPU must be limited by caller (should be 8)
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(skip_all, level = "debug")]
    pub async fn send_transaction_batch_parallel(
        connection: Connection,
        txs: Vec<Vec<u8>>,
        exit_signal: Arc<AtomicBool>,
        connection_timeout: Duration,
    ) {
        assert_ne!(txs.len(), 0, "no transactions to send");
        debug!("Opening {} parallel quic streams", txs.len());

        let all_send_fns = (0..txs.len()).map(|i| Self::send_tx_to_new_stream(&txs[i], connection.clone(), connection_timeout)).collect_vec();

        join_all(all_send_fns).await;

        debug!("connection stats (proxy send tx parallel): {}", connection_stats(&connection));
    }


    async fn send_tx_to_new_stream(tx: &Vec<u8>, connection: Connection, connection_timeout: Duration) {
        let mut send_stream = Self::open_unistream_simple(connection.clone(), connection_timeout)
            .await.0
            .unwrap();

        let write_timeout_res =
            timeout(connection_timeout, send_stream.write_all(tx.as_slice())).await;
        match write_timeout_res {
            Ok(no_timeout) => {
                match no_timeout {
                    Ok(()) => {}
                    Err(write_error) => {
                        error!("Error writing transaction to stream: {}", write_error);
                    }
                }
            }
            Err(elapsed) => {
                warn!("timeout sending transactions");
            }
        }

        // TODO wrap in small timeout
        let _ = timeout(Duration::from_millis(200), send_stream.finish()).await;

    }
}

pub struct SkipServerVerification;

impl SkipServerVerification {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

//  stable_id 140266619216912, rtt=2.156683ms,
// stats FrameStats { ACK: 3, CONNECTION_CLOSE: 0, CRYPTO: 3,
// DATA_BLOCKED: 0, DATAGRAM: 0, HANDSHAKE_DONE: 1, MAX_DATA: 0,
// MAX_STREAM_DATA: 1, MAX_STREAMS_BIDI: 0, MAX_STREAMS_UNI: 0, NEW_CONNECTION_ID: 4,
// NEW_TOKEN: 0, PATH_CHALLENGE: 0, PATH_RESPONSE: 0, PING: 0, RESET_STREAM: 0,
// RETIRE_CONNECTION_ID: 1, STREAM_DATA_BLOCKED: 0, STREAMS_BLOCKED_BIDI: 0,
// STREAMS_BLOCKED_UNI: 0, STOP_SENDING: 0, STREAM: 0 }
pub fn connection_stats(connection: &Connection) -> String {
    format!("stable_id {} stats {:?}, rtt={:?}",
            connection.stable_id(), connection.stats().frame_rx, connection.stats().path.rtt)
}