use anyhow::bail;
use countmap::CountMap;
use crossbeam_channel::{Receiver, RecvError, RecvTimeoutError, Sender};
use futures::future::join_all;
use log::{debug, error, info, trace, warn};
use quinn::TokioRuntime;
use serde::de::Unexpected::Option;
use solana_lite_rpc_core::quic_connection_utils::QuicConnectionParameters;
use solana_lite_rpc_core::structures::identity_stakes::IdentityStakes;
use solana_lite_rpc_core::tx_store::empty_tx_store;
use solana_lite_rpc_services::tpu_utils::tpu_connection_manager::TpuConnectionManager;
use solana_rpc_client::rpc_client::SerializableTransaction;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::signer::keypair;
use solana_sdk::transaction::{Transaction, VersionedTransaction};
use solana_streamer::nonblocking::quic::ConnectionPeerType;
use solana_streamer::packet::PacketBatch;
use solana_streamer::quic::StreamStats;
use solana_streamer::streamer::StakedNodes;
use solana_streamer::tls_certificates::new_self_signed_tls_certificate;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::ops::Deref;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::SendError;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tokio::{join, spawn, task};
use tracing_subscriber::{filter::LevelFilter, fmt};

// note: logging will be auto-adjusted
const SAMPLE_TX_COUNT: u32 = 20;

const MAXIMUM_TRANSACTIONS_IN_QUEUE: usize = 200_000;
const MAX_QUIC_CONNECTIONS_PER_PEER: usize = 8; // like solana repo

#[test] // note: tokio runtimes get created as part of the integration test
pub fn wireup_and_send_txs_via_channel() {
    configure_logging();

    // value from solana - see quic streamer - see quic.rs -> rt()
    const NUM_QUIC_STREAMER_WORKER_THREADS: usize = 1;
    let runtime_quic1 = Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("quic-server")
        .enable_all()
        .build()
        .expect("failed to build tokio runtime for testing quic server");

    // lite-rpc
    let runtime_literpc = tokio::runtime::Builder::new_multi_thread()
        // see lite-rpc -> main.rs
        .worker_threads(16) // note: this value has changed with the "deadlock fix" - TODO experiment with it
        .enable_all()
        .build()
        .expect("failed to build tokio runtime for lite-rpc-tpu-client");

    let literpc_validator_identity = Arc::new(Keypair::new());
    let udp_listen_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let listen_addr = udp_listen_socket.local_addr().unwrap();

    let (inbound_packets_sender, inbound_packets_receiver) = crossbeam_channel::unbounded();

    runtime_quic1.block_on(async {
        /// setup solana Quic streamer
        // see log "Start quic server on UdpSocket { addr: 127.0.0.1:xxxxx, fd: 10 }"
        let staked_nodes = StakedNodes {
            total_stake: 100,
            max_stake: 40,
            min_stake: 0,
            ip_stake_map: Default::default(),
            pubkey_stake_map:
            // literpc_validator_identity.as_ref()
                if STAKE_CONNECTION {
                    let mut map = HashMap::new();
                    map.insert(literpc_validator_identity.pubkey(),30);
                    map
                } else { HashMap::default() }
        };

        let _solana_quic_streamer = SolanaQuicStreamer::new_start_listening(
            udp_listen_socket,
            inbound_packets_sender,
            staked_nodes,
        );
    });

    runtime_literpc.block_on(async {
        tokio::spawn(start_literpc_client(
            listen_addr,
            literpc_validator_identity,
        ));
    });

    let packet_consumer_jh = thread::spawn(move || {
        info!("start pulling packets...");
        let mut packet_count = 0;
        let time_to_first = Instant::now();
        let mut latest_tx = Instant::now();
        let timer = Instant::now();
        // second half
        let mut timer2 = None;
        let mut packet_count2 = 0;
        let mut count_map: CountMap<Signature> = CountMap::with_capacity(SAMPLE_TX_COUNT as usize);
        const WARMUP_TX_COUNT: u32 = SAMPLE_TX_COUNT / 2;
        while packet_count < SAMPLE_TX_COUNT {
            if latest_tx.elapsed() > Duration::from_secs(5) {
                warn!("abort after timeout waiting for packet from quic streamer");
                break;
            }

            let packet_batch = match inbound_packets_receiver
                .recv_timeout(Duration::from_millis(500))
            {
                Ok(batch) => batch,
                Err(_) => {
                    debug!("consumer thread did not receive packets on inbound channel recently - continue polling");
                    continue;
                }
            };

            // reset timer
            latest_tx = Instant::now();

            if packet_count == 0 {
                info!(
                    "time to first packet {}ms",
                    time_to_first.elapsed().as_millis()
                );
            }

            packet_count = packet_count + packet_batch.len() as u32;
            if timer2.is_some() {
                packet_count2 = packet_count2 + packet_batch.len() as u32;
            }

            for packet in packet_batch.iter() {
                let tx = packet
                    .deserialize_slice::<VersionedTransaction, _>(..)
                    .unwrap();
                trace!(
                    "read transaction from quic streaming server: {:?}",
                    tx.get_signature()
                );
                count_map.insert_or_increment(*tx.get_signature());
                // for ix in tx.message.instructions() {
                //     info!("instruction: {:?}", ix.data);
                // }
            }

            if packet_count == WARMUP_TX_COUNT {
                timer2 = Some(Instant::now());
            }
            // info!("received packets so far: {}", packet_count);
            if packet_count == SAMPLE_TX_COUNT {
                break;
            }
        } // -- while not all packets received - by count

        let total_duration = timer.elapsed();
        let half_duration = timer2
            .map(|t| t.elapsed())
            .unwrap_or(Duration::from_secs(3333));

        // throughput_50 is second half of transactions - should iron out startup effects
        info!(
            "consumed {} packets in {}us - throughput {:.2} tps, throughput_50 {:.2} tps, ",
            packet_count,
            total_duration.as_micros(),
            packet_count as f64 / total_duration.as_secs_f64(),
            packet_count2 as f64 / half_duration.as_secs_f64(),
        );

        info!("got all expected packets - shutting down tokio runtime with lite-rpc client");

        assert_eq!(
            count_map.len() as u32,
            SAMPLE_TX_COUNT,
            "count_map size should be equal to SAMPLE_TX_COUNT"
        );
        // note: this assumption will not hold as soon as test is configured to do fanout
        assert!(
            count_map.values().all(|cnt| *cnt == 1),
            "all transactions should be unique"
        );

        runtime_literpc.shutdown_timeout(Duration::from_millis(1000));
    });

    // shutdown streamer
    // solana_quic_streamer.shutdown().await;

    packet_consumer_jh.join().unwrap();
}

fn configure_logging() {
    let env_filter = if SAMPLE_TX_COUNT < 100 {
        "trace,rustls=info,quinn_proto=debug"
    } else {
        "debug,quinn_proto=info,rustls=info,solana_streamer=debug"
    };
    tracing_subscriber::fmt::fmt()
        // .with_max_level(LevelFilter::DEBUG)
        .with_env_filter(env_filter)
        .init();
}

const STAKE_CONNECTION: bool = true;

const QUIC_CONNECTION_PARAMS: QuicConnectionParameters = QuicConnectionParameters {
    connection_timeout: Duration::from_secs(1),
    connection_retry_count: 10,
    finalize_timeout: Duration::from_millis(200),
    max_number_of_connections: 8,
    unistream_timeout: Duration::from_millis(500),
    write_timeout: Duration::from_secs(1),
    number_of_transactions_per_unistream: 10,
};

async fn start_literpc_client(
    streamer_listen_addrs: SocketAddr,
    literpc_validator_identity: Arc<Keypair>,
) -> anyhow::Result<()> {
    info!("Start lite-rpc test client ...");

    let fanout_slots = 4;

    // (String, Vec<u8>) (signature, transaction)
    let (sender, _) = tokio::sync::broadcast::channel(MAXIMUM_TRANSACTIONS_IN_QUEUE);
    let broadcast_sender = Arc::new(sender);
    let (certificate, key) = new_self_signed_tls_certificate(
        literpc_validator_identity.as_ref(),
        IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
    )
    .expect("Failed to initialize QUIC connection certificates");

    let tpu_connection_manager =
        TpuConnectionManager::new(certificate, key, fanout_slots as usize).await;

    // this effectively controls how many connections we will have
    let mut connections_to_keep: HashMap<Pubkey, SocketAddr> = HashMap::new();
    let addr1 = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    connections_to_keep.insert(
        Pubkey::from_str("1111111jepwNWbYG87sgwnBbUJnQHrPiUJzMpqJXZ")?,
        addr1,
    );

    let addr2 = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    connections_to_keep.insert(
        Pubkey::from_str("1111111k4AYMctpyJakWNvGcte6tR8BLyZw54R8qu")?,
        addr2,
    );

    // this is the real streamer
    connections_to_keep.insert(literpc_validator_identity.pubkey(), streamer_listen_addrs);

    // get information about the optional validator identity stake
    // populated from get_stakes_for_identity()
    let identity_stakes = IdentityStakes {
        peer_type: ConnectionPeerType::Staked,
        stakes: if STAKE_CONNECTION { 30 } else { 0 }, // stake of lite-rpc
        min_stakes: 0,
        max_stakes: 40,
        total_stakes: 100,
    };

    // solana_streamer::nonblocking::quic: Peer type: Staked, stake 30, total stake 0, max streams 128 receive_window Ok(12320) from peer 127.0.0.1:8000

    tpu_connection_manager
        .update_connections(
            broadcast_sender.clone(),
            connections_to_keep,
            identity_stakes,
            // note: tx_store is useless in this scenario as it is never changed; it's only used to check for duplicates
            empty_tx_store().clone(),
            QUIC_CONNECTION_PARAMS,
        )
        .await;

    // TODO this is a race
    sleep(Duration::from_millis(1500)).await;

    for i in 0..SAMPLE_TX_COUNT {
        let raw_sample_tx = build_raw_sample_tx(i);
        debug!(
            "broadcast transaction {} to {} receivers: {}",
            raw_sample_tx.0,
            broadcast_sender.receiver_count(),
            format!("hi {}", i)
        );

        broadcast_sender.send(raw_sample_tx)?;
    }

    sleep(Duration::from_secs(30)).await;

    Ok(())
}

#[tokio::test]
// taken from solana -> test_nonblocking_quic_client_multiple_writes
async fn solana_quic_streamer_start() {
    let (sender, _receiver) = crossbeam_channel::unbounded();
    let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
    // will create random free port
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let exit = Arc::new(AtomicBool::new(false));
    // keypair to derive the server tls certificate
    let keypair = Keypair::new();
    // gossip_host is used in the server certificate
    let gossip_host = "127.0.0.1".parse().unwrap();
    let stats = Arc::new(StreamStats::default());
    let (_, t) = solana_streamer::nonblocking::quic::spawn_server(
        sock.try_clone().unwrap(),
        &keypair,
        gossip_host,
        sender,
        exit.clone(),
        1,
        staked_nodes,
        10,
        10,
        stats.clone(),
        1000,
    )
    .unwrap();

    let addr = sock.local_addr().unwrap().ip();
    let port = sock.local_addr().unwrap().port();
    let tpu_addr = SocketAddr::new(addr, port);

    // sleep(Duration::from_millis(500)).await;

    exit.store(true, Ordering::Relaxed);
    t.await.unwrap();

    stats.report();
}

struct SolanaQuicStreamer {
    sock: UdpSocket,
    exit: Arc<AtomicBool>,
    join_handler: JoinHandle<()>,
    stats: Arc<StreamStats>,
}

impl SolanaQuicStreamer {
    pub fn get_socket_addr(&self) -> SocketAddr {
        self.sock.local_addr().unwrap()
    }
}

impl SolanaQuicStreamer {
    pub async fn shutdown(self) {
        self.exit.store(true, Ordering::Relaxed);
        self.join_handler.await.unwrap();
        self.stats.report();
    }
}

impl SolanaQuicStreamer {
    fn new_start_listening(
        udp_socket: UdpSocket,
        sender: Sender<PacketBatch>,
        staked_nodes: StakedNodes,
    ) -> Self {
        let staked_nodes = Arc::new(RwLock::new(staked_nodes));
        let exit = Arc::new(AtomicBool::new(false));
        // keypair to derive the server tls certificate
        let keypair = Keypair::new();
        // gossip_host is used in the server certificate
        let gossip_host = "127.0.0.1".parse().unwrap();
        let stats = Arc::new(StreamStats::default());
        let (_, jh) = solana_streamer::nonblocking::quic::spawn_server(
            udp_socket.try_clone().unwrap(),
            &keypair,
            gossip_host,
            sender,
            exit.clone(),
            MAX_QUIC_CONNECTIONS_PER_PEER,
            staked_nodes.clone(),
            10,
            10,
            stats.clone(),
            1000,
        )
        .unwrap();

        let addr = udp_socket.local_addr().unwrap().ip();
        let port = udp_socket.local_addr().unwrap().port();
        let tpu_addr = SocketAddr::new(addr, port);

        Self {
            sock: udp_socket,
            exit,
            join_handler: jh,
            stats,
        }
    }
}

const MEMO_PROGRAM_ID: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

pub fn build_raw_sample_tx(i: u32) -> (String, Vec<u8>) {
    let payer_keypair = Keypair::from_base58_string(
        "rKiJ7H5UUp3JR18kNyTF1XPuwPKHEM7gMLWHZPWP5djrW1vSjfwjhvJrevxF9MPmUmN9gJMLHZdLMgc9ao78eKr",
    );

    let tx = build_sample_tx(&payer_keypair, i);

    let raw_tx = bincode::serialize::<VersionedTransaction>(&tx).expect("failed to serialize tx");

    (tx.get_signature().to_string(), raw_tx)
}

fn build_sample_tx(payer_keypair: &Keypair, i: u32) -> VersionedTransaction {
    let blockhash = Hash::default();
    create_memo_tx(format!("hi {}", i).as_bytes(), payer_keypair, blockhash).into()
}

fn create_memo_tx(msg: &[u8], payer: &Keypair, blockhash: Hash) -> Transaction {
    let memo = Pubkey::from_str(MEMO_PROGRAM_ID).unwrap();

    let instruction = Instruction::new_with_bytes(memo, msg, vec![]);
    let message = Message::new(&[instruction], Some(&payer.pubkey()));
    Transaction::new(&[payer], message, blockhash)
}