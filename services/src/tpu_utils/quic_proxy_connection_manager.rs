use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::bail;
use std::time::Duration;

use itertools::Itertools;
use log::{debug, error, info, trace};
use quinn::{
    ClientConfig, Endpoint, EndpointConfig, IdleTimeout, TokioRuntime, TransportConfig, VarInt,
};
use solana_sdk::pubkey::Pubkey;

use solana_sdk::transaction::VersionedTransaction;
use tokio::sync::{broadcast::Receiver, broadcast::Sender, RwLock};

use solana_lite_rpc_core::proxy_request_format::TpuForwardingRequest;
use solana_lite_rpc_core::quic_connection_utils::{
    QuicConnectionParameters, SkipServerVerification,
};

use crate::tpu_utils::quinn_auto_reconnect::AutoReconnect;

#[derive(Clone, Copy, Debug)]
pub struct TpuNode {
    pub tpu_identity: Pubkey,
    pub tpu_address: SocketAddr,
}

pub struct QuicProxyConnectionManager {
    endpoint: Endpoint,
    simple_thread_started: AtomicBool,
    proxy_addr: SocketAddr,
    current_tpu_nodes: Arc<RwLock<Vec<TpuNode>>>,
}

const CHUNK_SIZE_PER_STREAM: usize = 20;

impl QuicProxyConnectionManager {
    pub async fn new(
        certificate: rustls::Certificate,
        key: rustls::PrivateKey,
        proxy_addr: SocketAddr,
    ) -> Self {
        info!("Configure Quic proxy connection manager to {}", proxy_addr);
        let endpoint = Self::create_proxy_client_endpoint(certificate, key);

        Self {
            endpoint,
            simple_thread_started: AtomicBool::from(false),
            proxy_addr,
            current_tpu_nodes: Arc::new(RwLock::new(vec![])),
        }
    }

    pub async fn update_connection(
        &self,
        transaction_sender: Arc<Sender<(String, Vec<u8>)>>,
        // for duration of this slot these tpu nodes will receive the transactions
        connections_to_keep: HashMap<Pubkey, SocketAddr>,
        connection_parameters: QuicConnectionParameters,
    ) {
        debug!(
            "reconfigure quic proxy connection (# of tpu nodes: {})",
            connections_to_keep.len()
        );

        {
            let list_of_nodes = connections_to_keep
                .iter()
                .map(|(identity, tpu_address)| TpuNode {
                    tpu_identity: *identity,
                    tpu_address: *tpu_address,
                })
                .collect_vec();

            let mut lock = self.current_tpu_nodes.write().await;
            *lock = list_of_nodes;
        }

        if self.simple_thread_started.load(Relaxed) {
            // already started
            return;
        }
        self.simple_thread_started.store(true, Relaxed);

        info!("Starting very simple proxy thread");

        let transaction_receiver = transaction_sender.subscribe();

        let exit_signal = Arc::new(AtomicBool::new(false));

        tokio::spawn(Self::read_transactions_and_broadcast(
            transaction_receiver,
            self.current_tpu_nodes.clone(),
            self.proxy_addr,
            self.endpoint.clone(),
            exit_signal,
            connection_parameters,
        ));
    }

    fn create_proxy_client_endpoint(
        certificate: rustls::Certificate,
        key: rustls::PrivateKey,
    ) -> Endpoint {
        const ALPN_TPU_FORWARDPROXY_PROTOCOL_ID: &[u8] = b"solana-tpu-forward-proxy";

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
        crypto.alpn_protocols = vec![ALPN_TPU_FORWARDPROXY_PROTOCOL_ID.to_vec()];

        let mut config = ClientConfig::new(Arc::new(crypto));

        // note: this config must be aligned with quic-proxy's server config
        let mut transport_config = TransportConfig::default();
        let _timeout = IdleTimeout::try_from(Duration::from_secs(1)).unwrap();
        // no remotely-initiated streams required
        transport_config.max_concurrent_uni_streams(VarInt::from_u32(0));
        transport_config.max_concurrent_bidi_streams(VarInt::from_u32(0));
        let timeout = Duration::from_secs(10).try_into().unwrap();
        transport_config.max_idle_timeout(Some(timeout));
        transport_config.keep_alive_interval(Some(Duration::from_millis(500)));

        config.transport_config(Arc::new(transport_config));
        endpoint.set_default_client_config(config);

        endpoint
    }

    async fn read_transactions_and_broadcast(
        mut transaction_receiver: Receiver<(String, Vec<u8>)>,
        current_tpu_nodes: Arc<RwLock<Vec<TpuNode>>>,
        proxy_addr: SocketAddr,
        endpoint: Endpoint,
        exit_signal: Arc<AtomicBool>,
        connection_parameters: QuicConnectionParameters,
    ) {
        let auto_connection = AutoReconnect::new(endpoint, proxy_addr);

        loop {
            // exit signal set
            if exit_signal.load(Ordering::Relaxed) {
                break;
            }

            tokio::select! {
                // TODO add timeout
                tx = transaction_receiver.recv() => {

                    let first_tx: Vec<u8> = match tx {
                        Ok((_sig, tx)) => {
                            tx
                        },
                        Err(e) => {
                            error!(
                                "Broadcast channel error (close) on recv: {} - aborting", e);
                            return;
                        }
                    };

                    let mut txs = vec![first_tx];
                    for _ in 1..connection_parameters.number_of_transactions_per_unistream {
                        if let Ok((_signature, tx)) = transaction_receiver.try_recv() {
                            txs.push(tx);
                        }
                    }

                    let tpu_fanout_nodes = current_tpu_nodes.read().await.clone();

                    trace!("Sending copy of transaction batch of {} txs to {} tpu nodes via quic proxy",
                            txs.len(), tpu_fanout_nodes.len());

                    for target_tpu_node in tpu_fanout_nodes {
                        Self::send_copy_of_txs_to_quicproxy(
                            &txs, &auto_connection,
                            proxy_addr,
                            target_tpu_node.tpu_address,
                            target_tpu_node.tpu_identity)
                        .await.unwrap();
                    }

                },
            };
        }
    }

    async fn send_copy_of_txs_to_quicproxy(
        raw_tx_batch: &[Vec<u8>],
        auto_connection: &AutoReconnect,
        _proxy_address: SocketAddr,
        tpu_target_address: SocketAddr,
        target_tpu_identity: Pubkey,
    ) -> anyhow::Result<()> {
        let mut txs = vec![];

        for raw_tx in raw_tx_batch {
            let tx = match bincode::deserialize::<VersionedTransaction>(raw_tx) {
                Ok(tx) => tx,
                Err(err) => {
                    bail!(err.to_string());
                }
            };
            txs.push(tx);
        }

        for chunk in txs.chunks(CHUNK_SIZE_PER_STREAM) {
            let forwarding_request =
                TpuForwardingRequest::new(tpu_target_address, target_tpu_identity, chunk.into());
            debug!("forwarding_request: {}", forwarding_request);

            let proxy_request_raw =
                bincode::serialize(&forwarding_request).expect("Expect to serialize transactions");

            let send_result = auto_connection.send_uni(proxy_request_raw).await;

            match send_result {
                Ok(()) => {
                    debug!("Successfully sent {} txs to quic proxy", txs.len());
                }
                Err(e) => {
                    bail!("Failed to send data to quic proxy: {:?}", e);
                }
            }
        } // -- one chunk

        Ok(())
    }
}