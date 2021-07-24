//! The main AccountsDb replication node responsible for replicating
//! AccountsDb information from peer a validator or another replica-node.

#![allow(clippy::integer_arithmetic)]

use {
    clap::{crate_description, crate_name, value_t, values_t, App, AppSettings, Arg},
    crossbeam_channel::unbounded,
    log::*,
    rand::{seq::SliceRandom, thread_rng, Rng},
    solana_clap_utils::{
        input_parsers::keypair_of,
        input_validators::{is_keypair_or_ask_keyword, is_parsable, is_pubkey},
        keypair::SKIP_SEED_PHRASE_VALIDATION_ARG,
    },
    solana_download_utils::download_snapshot,
    solana_genesis_utils::download_then_check_genesis_hash,
    solana_gossip::{
        cluster_info::{ClusterInfo, Node, VALIDATOR_PORT_RANGE},
        contact_info::ContactInfo,
        gossip_service::GossipService,
    },
    solana_ledger::{
        blockstore::Blockstore, blockstore_db::AccessType, blockstore_processor,
        leader_schedule_cache::LeaderScheduleCache,
    },
    solana_rpc::{
        max_slots::MaxSlots,
        optimistically_confirmed_bank_tracker::{
            OptimisticallyConfirmedBank, OptimisticallyConfirmedBankTracker,
        },
        rpc::JsonRpcConfig,
        rpc_pubsub_service::{PubSubConfig, PubSubService},
        rpc_service::JsonRpcService,
        rpc_subscriptions::RpcSubscriptions,
    },
    solana_runtime::{
        accounts_index::AccountSecondaryIndexes,
        bank_forks::BankForks,
        commitment::BlockCommitmentCache,
        hardened_unpack::MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
        snapshot_config::SnapshotConfig,
        snapshot_utils::{self, ArchiveFormat},
    },
    solana_sdk::{
        clock::Slot,
        exit::Exit,
        genesis_config::GenesisConfig,
        hash::Hash,
        pubkey::Pubkey,
        signature::{Keypair, Signer},
    },
    solana_validator::port_range_validator,
    std::{
        collections::HashSet,
        env, fs,
        net::{IpAddr, SocketAddr, UdpSocket},
        path::{Path, PathBuf},
        process::exit,
        sync::{
            atomic::{AtomicBool, AtomicU64},
            Arc, RwLock,
        },
        thread::sleep,
        time::{Duration, Instant},
    },
};

struct ReplicaNodeConfig {
    pub rpc_source_addr: SocketAddr,
    pub rpc_addr: SocketAddr,
    pub rpc_pubsub_addr: SocketAddr,
    pub ledger_path: PathBuf,
    pub snapshot_output_dir: PathBuf,
    pub snapshot_path: PathBuf,
    pub account_paths: Vec<PathBuf>,
    pub snapshot_info: (Slot, Hash),
    pub cluster_info: Arc<ClusterInfo>,
    pub rpc_config: JsonRpcConfig,
    pub snapshot_config: Option<SnapshotConfig>,
    pub pubsub_config: PubSubConfig,
    pub account_indexes: AccountSecondaryIndexes,
    pub accounts_db_caching_enabled: bool,
    pub replica_exit: Arc<RwLock<Exit>>,
}

impl Default for ReplicaNodeConfig {
    fn default() -> Self {
        Self {
            rpc_source_addr: SocketAddr::from(([127, 0, 0, 1], 8001)),
            rpc_addr: SocketAddr::from(([127, 0, 0, 1], 8001)),
            rpc_pubsub_addr: SocketAddr::from(([127, 0, 0, 1], 8001)),
            ledger_path: PathBuf::default(),
            snapshot_output_dir: PathBuf::default(),
            snapshot_path: PathBuf::default(),
            account_paths: vec![],
            snapshot_info: (0, Hash::default()),
            cluster_info: Arc::new(ClusterInfo::default()),
            rpc_config: JsonRpcConfig::default(),
            snapshot_config: None,
            pubsub_config: PubSubConfig::default(),
            account_indexes: AccountSecondaryIndexes::default(),
            accounts_db_caching_enabled: false,
            replica_exit: Arc::new(RwLock::new(Exit::default())),
        }
    }
}

struct ReplicaNode {
    json_rpc_service: Option<JsonRpcService>,
    pubsub_service: Option<PubSubService>,
    optimistically_confirmed_bank_tracker: Option<OptimisticallyConfirmedBankTracker>,
}

fn start_gossip_node(
    identity_keypair: Arc<Keypair>,
    cluster_entrypoints: &[ContactInfo],
    ledger_path: &Path,
    gossip_addr: &SocketAddr,
    gossip_socket: UdpSocket,
    expected_shred_version: Option<u16>,
    gossip_validators: Option<HashSet<Pubkey>>,
    should_check_duplicate_instance: bool,
) -> (Arc<ClusterInfo>, Arc<AtomicBool>, GossipService) {
    let contact_info = ClusterInfo::gossip_contact_info(
        identity_keypair.pubkey(),
        *gossip_addr,
        expected_shred_version.unwrap_or(0),
    );
    let mut cluster_info = ClusterInfo::new(contact_info, identity_keypair);
    cluster_info.set_entrypoints(cluster_entrypoints.to_vec());
    cluster_info.restore_contact_info(ledger_path, 0);
    let cluster_info = Arc::new(cluster_info);

    let gossip_exit_flag = Arc::new(AtomicBool::new(false));
    let gossip_service = GossipService::new(
        &cluster_info,
        None,
        gossip_socket,
        gossip_validators,
        should_check_duplicate_instance,
        &gossip_exit_flag,
    );
    info!("Started gossip node");
    (cluster_info, gossip_exit_flag, gossip_service)
}

// Struct maintaining information about banks
struct ReplicaBankInfo {
    bank_forks: Arc<RwLock<BankForks>>,
    optimistically_confirmed_bank: Arc<RwLock<OptimisticallyConfirmedBank>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
}

// Initialize the replica by downloading snapshot from the peer, initialize
// the BankForks, OptimisticallyConfirmedBank, LeaderScheduleCache and
// BlockCommitmentCache and return the info wrapped as ReplicaBankInfo.
fn initialize_from_snapshot(
    replica_config: &ReplicaNodeConfig,
    snapshot_config: &SnapshotConfig,
    genesis_config: &GenesisConfig,
) -> ReplicaBankInfo {
    info!(
        "Downloading snapshot from the peer into {:?}",
        replica_config.snapshot_output_dir
    );

    download_snapshot(
        &replica_config.rpc_source_addr,
        &replica_config.snapshot_output_dir,
        replica_config.snapshot_info,
        false,
        snapshot_config.maximum_snapshots_to_retain,
        &mut None,
    )
    .unwrap();

    fs::create_dir_all(&snapshot_config.snapshot_path).expect("Couldn't create snapshot directory");

    let archive_info = snapshot_utils::get_highest_full_snapshot_archive_info(
        replica_config.snapshot_output_dir.to_path_buf(),
    )
    .unwrap();

    let process_options = blockstore_processor::ProcessOptions {
        account_indexes: replica_config.account_indexes.clone(),
        accounts_db_caching_enabled: replica_config.accounts_db_caching_enabled,
        ..blockstore_processor::ProcessOptions::default()
    };

    info!(
        "Build bank from snapshot archive: {:?}",
        &snapshot_config.snapshot_path
    );
    let (bank0, _) = snapshot_utils::bank_from_snapshot_archives(
        &replica_config.account_paths,
        &[],
        &snapshot_config.snapshot_path,
        archive_info.path(),
        None,
        *archive_info.archive_format(),
        genesis_config,
        process_options.debug_keys.clone(),
        None,
        process_options.account_indexes.clone(),
        process_options.accounts_db_caching_enabled,
        process_options.limit_load_slot_count_from_snapshot,
        process_options.shrink_ratio,
        process_options.accounts_db_test_hash_calculation,
        process_options.verify_index,
    )
    .unwrap();

    let bank0_slot = bank0.slot();
    let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank0));

    let bank_forks = Arc::new(RwLock::new(BankForks::new(bank0)));

    let optimistically_confirmed_bank =
        OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);

    let mut block_commitment_cache = BlockCommitmentCache::default();
    block_commitment_cache.initialize_slots(bank0_slot);
    let block_commitment_cache = Arc::new(RwLock::new(block_commitment_cache));

    ReplicaBankInfo {
        bank_forks,
        optimistically_confirmed_bank,
        leader_schedule_cache,
        block_commitment_cache,
    }
}

fn start_client_rpc_services(
    replica_config: &ReplicaNodeConfig,
    genesis_config: &GenesisConfig,
    cluster_info: Arc<ClusterInfo>,
    bank_info: &ReplicaBankInfo,
) -> (
    Option<JsonRpcService>,
    Option<PubSubService>,
    Option<OptimisticallyConfirmedBankTracker>,
) {
    let ReplicaBankInfo {
        bank_forks,
        optimistically_confirmed_bank,
        leader_schedule_cache,
        block_commitment_cache,
    } = bank_info;
    let blockstore = Arc::new(
        Blockstore::open_with_access_type(
            &replica_config.ledger_path,
            AccessType::PrimaryOnly,
            None,
            false,
        )
        .unwrap(),
    );

    let max_complete_transaction_status_slot = Arc::new(AtomicU64::new(0));

    let max_slots = Arc::new(MaxSlots::default());
    let exit = Arc::new(AtomicBool::new(false));

    let subscriptions = Arc::new(RpcSubscriptions::new(
        &exit,
        bank_forks.clone(),
        block_commitment_cache.clone(),
        optimistically_confirmed_bank.clone(),
    ));

    let rpc_override_health_check = Arc::new(AtomicBool::new(false));
    if ContactInfo::is_valid_address(&replica_config.rpc_addr) {
        assert!(ContactInfo::is_valid_address(
            &replica_config.rpc_pubsub_addr
        ));
    } else {
        assert!(!ContactInfo::is_valid_address(
            &replica_config.rpc_pubsub_addr
        ));
    }

    let (_bank_notification_sender, bank_notification_receiver) = unbounded();
    (
        Some(JsonRpcService::new(
            replica_config.rpc_addr,
            replica_config.rpc_config.clone(),
            replica_config.snapshot_config.clone(),
            bank_forks.clone(),
            block_commitment_cache.clone(),
            blockstore,
            cluster_info,
            None,
            genesis_config.hash(),
            &replica_config.ledger_path,
            replica_config.replica_exit.clone(),
            None,
            rpc_override_health_check,
            optimistically_confirmed_bank.clone(),
            0,
            0,
            max_slots,
            leader_schedule_cache.clone(),
            max_complete_transaction_status_slot,
        )),
        Some(PubSubService::new(
            replica_config.pubsub_config.clone(),
            &subscriptions,
            replica_config.rpc_pubsub_addr,
            &exit,
        )),
        Some(OptimisticallyConfirmedBankTracker::new(
            bank_notification_receiver,
            &exit,
            bank_forks.clone(),
            optimistically_confirmed_bank.clone(),
            subscriptions.clone(),
        )),
    )
}

fn get_cluster_shred_version(entrypoints: &[SocketAddr]) -> Option<u16> {
    let entrypoints = {
        let mut index: Vec<_> = (0..entrypoints.len()).collect();
        index.shuffle(&mut rand::thread_rng());
        index.into_iter().map(|i| &entrypoints[i])
    };
    for entrypoint in entrypoints {
        match solana_net_utils::get_cluster_shred_version(entrypoint) {
            Err(err) => eprintln!("get_cluster_shred_version failed: {}, {}", entrypoint, err),
            Ok(0) => eprintln!("zero sherd-version from entrypoint: {}", entrypoint),
            Ok(shred_version) => {
                info!(
                    "obtained shred-version {} from {}",
                    shred_version, entrypoint
                );
                return Some(shred_version);
            }
        }
    }
    None
}

// Discover the RPC peer node via Gossip and return's ContactInfo
// And the initial snapshot info: (Slot, Hash)
// Alternatively, this can be solved via a RPC call instead of using gossip.
fn get_rpc_peer_node(
    cluster_info: &ClusterInfo,
    cluster_entrypoints: &[ContactInfo],
    expected_shred_version: Option<u16>,
    peer_pubkey: &Pubkey,
    snapshot_output_dir: &Path,
) -> Option<(ContactInfo, Option<(Slot, Hash)>)> {
    let mut newer_cluster_snapshot_timeout = None;
    let mut retry_reason = None;
    loop {
        sleep(Duration::from_secs(1));
        info!("Searching for the rpc peer node and latest snapshot information.");
        info!("\n{}", cluster_info.rpc_info_trace());

        let shred_version =
            expected_shred_version.unwrap_or_else(|| cluster_info.my_shred_version());
        if shred_version == 0 {
            let all_zero_shred_versions = cluster_entrypoints.iter().all(|cluster_entrypoint| {
                cluster_info
                    .lookup_contact_info_by_gossip_addr(&cluster_entrypoint.gossip)
                    .map_or(false, |entrypoint| entrypoint.shred_version == 0)
            });

            if all_zero_shred_versions {
                eprintln!(
                    "Entrypoint shred version is zero.  Restart with --expected-shred-version"
                );
                exit(1);
            }
            info!("Waiting to adopt entrypoint shred version...");
            continue;
        }

        info!(
            "Searching for an RPC service with shred version {}{}...",
            shred_version,
            retry_reason
                .as_ref()
                .map(|s| format!(" (Retrying: {})", s))
                .unwrap_or_default()
        );

        let rpc_peers = cluster_info
            .all_rpc_peers()
            .into_iter()
            .filter(|contact_info| contact_info.shred_version == shred_version)
            .collect::<Vec<_>>();
        let rpc_peers_total = rpc_peers.len();

        let rpc_peers_trusted = rpc_peers
            .iter()
            .filter(|rpc_peer| &rpc_peer.id == peer_pubkey)
            .count();

        info!(
            "Total {} RPC nodes found. {} trusted",
            rpc_peers_total, rpc_peers_trusted
        );

        let mut highest_snapshot_hash: Option<(Slot, Hash)> =
            snapshot_utils::get_highest_full_snapshot_archive_info(snapshot_output_dir).map(
                |snapshot_archive_info| {
                    (*snapshot_archive_info.slot(), *snapshot_archive_info.hash())
                },
            );
        let eligible_rpc_peers = {
            let mut eligible_rpc_peers = vec![];

            for rpc_peer in rpc_peers.iter() {
                if &rpc_peer.id != peer_pubkey {
                    continue;
                }
                cluster_info.get_snapshot_hash_for_node(&rpc_peer.id, |snapshot_hashes| {
                    for snapshot_hash in snapshot_hashes {
                        if highest_snapshot_hash.is_none()
                            || snapshot_hash.0 > highest_snapshot_hash.unwrap().0
                        {
                            // Found a higher snapshot, remove all nodes with a lower snapshot
                            eligible_rpc_peers.clear();
                            highest_snapshot_hash = Some(*snapshot_hash)
                        }

                        if Some(*snapshot_hash) == highest_snapshot_hash {
                            eligible_rpc_peers.push(rpc_peer.clone());
                        }
                    }
                });
            }

            match highest_snapshot_hash {
                None => {
                    assert!(eligible_rpc_peers.is_empty());
                }
                Some(highest_snapshot_hash) => {
                    if eligible_rpc_peers.is_empty() {
                        match newer_cluster_snapshot_timeout {
                            None => newer_cluster_snapshot_timeout = Some(Instant::now()),
                            Some(newer_cluster_snapshot_timeout) => {
                                if newer_cluster_snapshot_timeout.elapsed().as_secs() > 180 {
                                    warn!("giving up newer snapshot from the cluster");
                                    return None;
                                }
                            }
                        }
                        retry_reason = Some(format!(
                            "Wait for newer snapshot than local: {:?}",
                            highest_snapshot_hash
                        ));
                        continue;
                    }

                    info!(
                        "Highest available snapshot slot is {}, available from {} node{}: {:?}",
                        highest_snapshot_hash.0,
                        eligible_rpc_peers.len(),
                        if eligible_rpc_peers.len() > 1 {
                            "s"
                        } else {
                            ""
                        },
                        eligible_rpc_peers
                            .iter()
                            .map(|contact_info| contact_info.id)
                            .collect::<Vec<_>>()
                    );
                }
            }
            eligible_rpc_peers
        };

        if !eligible_rpc_peers.is_empty() {
            let contact_info =
                &eligible_rpc_peers[thread_rng().gen_range(0, eligible_rpc_peers.len())];
            return Some((contact_info.clone(), highest_snapshot_hash));
        } else {
            retry_reason = Some("No snapshots available".to_owned());
        }
    }
}

// Get the RPC peer info given the peer's Pubkey
// Returns the ClusterInfo, the peer's ContactInfo and the initial snapshot info
fn get_rpc_peer_info(
    identity_keypair: Keypair,
    cluster_entrypoints: &[ContactInfo],
    ledger_path: &Path,
    node: &Node,
    expected_shred_version: Option<u16>,
    peer_pubkey: &Pubkey,
    snapshot_output_dir: &Path,
) -> (Arc<ClusterInfo>, ContactInfo, Option<(Slot, Hash)>) {
    let identity_keypair = Arc::new(identity_keypair);

    let gossip = Some(start_gossip_node(
        identity_keypair,
        cluster_entrypoints,
        ledger_path,
        &node.info.gossip,
        node.sockets.gossip.try_clone().unwrap(),
        expected_shred_version,
        None,
        true,
    ));

    let rpc_node_details = get_rpc_peer_node(
        &gossip.as_ref().unwrap().0,
        cluster_entrypoints,
        expected_shred_version,
        peer_pubkey,
        snapshot_output_dir,
    );
    let rpc_node_details = rpc_node_details.unwrap();

    (
        gossip.as_ref().unwrap().0.clone(),
        rpc_node_details.0,
        rpc_node_details.1,
    )
}

impl ReplicaNode {
    pub fn new(replica_config: ReplicaNodeConfig) -> Self {
        let genesis_config = download_then_check_genesis_hash(
            &replica_config.rpc_source_addr,
            &replica_config.ledger_path,
            None,
            MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            false,
            true,
        )
        .unwrap();

        let snapshot_config = SnapshotConfig {
            snapshot_interval_slots: std::u64::MAX,
            snapshot_package_output_path: replica_config.snapshot_output_dir.clone(),
            snapshot_path: replica_config.snapshot_path.clone(),
            archive_format: ArchiveFormat::TarBzip2,
            snapshot_version: snapshot_utils::SnapshotVersion::default(),
            maximum_snapshots_to_retain:
                snapshot_utils::DEFAULT_MAX_FULL_SNAPSHOT_ARCHIVES_TO_RETAIN,
        };

        let bank_info =
            initialize_from_snapshot(&replica_config, &snapshot_config, &genesis_config);

        let (json_rpc_service, pubsub_service, optimistically_confirmed_bank_tracker) =
            start_client_rpc_services(
                &replica_config,
                &genesis_config,
                replica_config.cluster_info.clone(),
                &bank_info,
            );

        ReplicaNode {
            json_rpc_service,
            pubsub_service,
            optimistically_confirmed_bank_tracker,
        }
    }

    pub fn join(self) {
        if let Some(json_rpc_service) = self.json_rpc_service {
            json_rpc_service.join().expect("rpc_service");
        }

        if let Some(pubsub_service) = self.pubsub_service {
            pubsub_service.join().expect("pubsub_service");
        }

        if let Some(optimistically_confirmed_bank_tracker) =
            self.optimistically_confirmed_bank_tracker
        {
            optimistically_confirmed_bank_tracker
                .join()
                .expect("optimistically_confirmed_bank_tracker");
        }
    }
}

pub fn main() {
    let default_dynamic_port_range =
        &format!("{}-{}", VALIDATOR_PORT_RANGE.0, VALIDATOR_PORT_RANGE.1);

    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .setting(AppSettings::VersionlessSubcommands)
        .setting(AppSettings::InferSubcommands)
        .arg(
            Arg::with_name(SKIP_SEED_PHRASE_VALIDATION_ARG.name)
                .long(SKIP_SEED_PHRASE_VALIDATION_ARG.long)
                .help(SKIP_SEED_PHRASE_VALIDATION_ARG.help),
        )
        .arg(
            Arg::with_name("ledger_path")
                .short("l")
                .long("ledger")
                .value_name("DIR")
                .takes_value(true)
                .required(true)
                .default_value("ledger")
                .help("Use DIR as ledger location"),
        )
        .arg(
            Arg::with_name("peer")
                .long("peer")
                .value_name("IP:PORT")
                .takes_value(true)
                .required(true)
                .help("The the IP:PORT for the peer validator/replica to download from"),
        )
        .arg(
            Arg::with_name("peer_pubkey")
                .long("peer-pubkey")
                .validator(is_pubkey)
                .value_name("The peer validator/replica IDENTITY")
                .multiple(true)
                .takes_value(true)
                .help("The pubkey for the target validator."),
        )
        .arg(
            Arg::with_name("account_paths")
                .long("accounts")
                .value_name("PATHS")
                .takes_value(true)
                .multiple(true)
                .help("Comma separated persistent accounts location"),
        )
        .arg(
            Arg::with_name("identity")
                .short("i")
                .long("identity")
                .value_name("KEYPAIR")
                .takes_value(true)
                .validator(is_keypair_or_ask_keyword)
                .help("Replica identity keypair"),
        )
        .arg(
            Arg::with_name("entrypoint")
                .short("n")
                .long("entrypoint")
                .value_name("HOST:PORT")
                .takes_value(true)
                .multiple(true)
                .validator(solana_net_utils::is_host_port)
                .help("Rendezvous with the cluster at this gossip entrypoint"),
        )
        .arg(
            Arg::with_name("bind_address")
                .long("bind-address")
                .value_name("HOST")
                .takes_value(true)
                .validator(solana_net_utils::is_host)
                .default_value("0.0.0.0")
                .help("IP address to bind the replica ports"),
        )
        .arg(
            Arg::with_name("rpc_bind_address")
                .long("rpc-bind-address")
                .value_name("HOST")
                .takes_value(true)
                .validator(solana_net_utils::is_host)
                .help("IP address to bind the Json RPC port [default: use --bind-address]"),
        )
        .arg(
            Arg::with_name("rpc_port")
                .long("rpc-port")
                .value_name("PORT")
                .takes_value(true)
                .validator(solana_validator::port_validator)
                .help("Enable JSON RPC on this port, and the next port for the RPC websocket"),
        )
        .arg(
            Arg::with_name("dynamic_port_range")
                .long("dynamic-port-range")
                .value_name("MIN_PORT-MAX_PORT")
                .takes_value(true)
                .default_value(default_dynamic_port_range)
                .validator(port_range_validator)
                .help("Range to use for dynamically assigned ports"),
        )
        .arg(
            Arg::with_name("expected_shred_version")
                .long("expected-shred-version")
                .value_name("VERSION")
                .takes_value(true)
                .validator(is_parsable::<u16>)
                .help("Require the shred version be this value"),
        )
        .arg(
            Arg::with_name("logfile")
                .short("o")
                .long("log")
                .value_name("FILE")
                .takes_value(true)
                .help(
                    "Redirect logging to the specified file, '-' for standard error. \
                       Sending the SIGUSR1 signal to the validator process will cause it \
                       to re-open the log file",
                ),
        )
        .get_matches();

    let bind_address = solana_net_utils::parse_host(matches.value_of("bind_address").unwrap())
        .expect("invalid bind_address");

    let rpc_bind_address = if matches.is_present("rpc_bind_address") {
        solana_net_utils::parse_host(matches.value_of("rpc_bind_address").unwrap())
            .expect("invalid rpc_bind_address")
    } else {
        bind_address
    };

    let identity_keypair = keypair_of(&matches, "identity").unwrap_or_else(|| {
        clap::Error::with_description(
            "The --identity <KEYPAIR> argument is required",
            clap::ErrorKind::ArgumentNotFound,
        )
        .exit();
    });

    let peer_pubkey = value_t!(matches, "peer_pubkey", Pubkey).unwrap();

    let entrypoint_addrs = values_t!(matches, "entrypoint", String)
        .unwrap_or_default()
        .into_iter()
        .map(|entrypoint| {
            solana_net_utils::parse_host_port(&entrypoint).unwrap_or_else(|e| {
                eprintln!("failed to parse entrypoint address: {}", e);
                exit(1);
            })
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let expected_shred_version = value_t!(matches, "expected_shred_version", u16)
        .ok()
        .or_else(|| get_cluster_shred_version(&entrypoint_addrs));

    let gossip_host: IpAddr = matches
        .value_of("gossip_host")
        .map(|gossip_host| {
            solana_net_utils::parse_host(gossip_host).unwrap_or_else(|err| {
                eprintln!("Failed to parse --gossip-host: {}", err);
                exit(1);
            })
        })
        .unwrap_or_else(|| {
            if !entrypoint_addrs.is_empty() {
                let mut order: Vec<_> = (0..entrypoint_addrs.len()).collect();
                order.shuffle(&mut thread_rng());

                let gossip_host = order.into_iter().find_map(|i| {
                    let entrypoint_addr = &entrypoint_addrs[i];
                    info!(
                        "Contacting {} to determine the validator's public IP address",
                        entrypoint_addr
                    );
                    solana_net_utils::get_public_ip_addr(entrypoint_addr).map_or_else(
                        |err| {
                            eprintln!(
                                "Failed to contact cluster entrypoint {}: {}",
                                entrypoint_addr, err
                            );
                            None
                        },
                        Some,
                    )
                });

                gossip_host.unwrap_or_else(|| {
                    eprintln!("Unable to determine the validator's public IP address");
                    exit(1);
                })
            } else {
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))
            }
        });

    let gossip_addr = SocketAddr::new(
        gossip_host,
        value_t!(matches, "gossip_port", u16).unwrap_or_else(|_| {
            solana_net_utils::find_available_port_in_range(bind_address, (0, 1)).unwrap_or_else(
                |err| {
                    eprintln!("Unable to find an available gossip port: {}", err);
                    exit(1);
                },
            )
        }),
    );

    let dynamic_port_range =
        solana_net_utils::parse_port_range(matches.value_of("dynamic_port_range").unwrap())
            .expect("invalid dynamic_port_range");

    let cluster_entrypoints = entrypoint_addrs
        .iter()
        .map(ContactInfo::new_gossip_entry_point)
        .collect::<Vec<_>>();

    let node = Node::new_with_external_ip(
        &identity_keypair.pubkey(),
        &gossip_addr,
        dynamic_port_range,
        bind_address,
    );

    let ledger_path = PathBuf::from(matches.value_of("ledger_path").unwrap());
    let snapshot_output_dir = if matches.is_present("snapshots") {
        PathBuf::from(matches.value_of("snapshots").unwrap())
    } else {
        ledger_path.clone()
    };
    let snapshot_path = snapshot_output_dir.join("snapshot");

    let account_paths: Vec<PathBuf> =
        if let Ok(account_paths) = values_t!(matches, "account_paths", String) {
            account_paths
                .join(",")
                .split(',')
                .map(PathBuf::from)
                .collect()
        } else {
            vec![ledger_path.join("accounts")]
        };

    let rpc_source_addr =
        solana_net_utils::parse_host_port(matches.value_of("peer").unwrap_or_else(|| {
            clap::Error::with_description(
                "The --peer <IP:PORT> argument is required",
                clap::ErrorKind::ArgumentNotFound,
            )
            .exit();
        }))
        .unwrap_or_else(|e| {
            eprintln!("failed to parse entrypoint address: {}", e);
            exit(1);
        });

    let rpc_port = value_t!(matches, "rpc_port", u16).unwrap_or_else(|_| {
        clap::Error::with_description(
            "The --rpc-port <PORT> argument is required",
            clap::ErrorKind::ArgumentNotFound,
        )
        .exit();
    });
    let rpc_addrs = (
        SocketAddr::new(rpc_bind_address, rpc_port),
        SocketAddr::new(rpc_bind_address, rpc_port + 1),
        // If additional ports are added, +2 needs to be skipped to avoid a conflict with
        // the websocket port (which is +2) in web3.js This odd port shifting is tracked at
        // https://github.com/solana-labs/solana/issues/12250
    );

    let logfile = {
        let logfile = matches
            .value_of("logfile")
            .map(|s| s.into())
            .unwrap_or_else(|| format!("solana-rpc-node-{}.log", identity_keypair.pubkey()));

        if logfile == "-" {
            None
        } else {
            println!("log file: {}", logfile);
            Some(logfile)
        }
    };

    let _logger_thread = solana_validator::redirect_stderr_to_file(logfile);

    let (cluster_info, rpc_contact_info, snapshot_info) = get_rpc_peer_info(
        identity_keypair,
        &cluster_entrypoints,
        &ledger_path,
        &node,
        expected_shred_version,
        &peer_pubkey,
        &snapshot_output_dir,
    );

    info!(
        "Using RPC service from node {}: {:?}, snapshot_info: {:?}",
        rpc_contact_info.id, rpc_contact_info.rpc, snapshot_info
    );

    let config = ReplicaNodeConfig {
        rpc_source_addr,
        rpc_addr: rpc_addrs.0,
        rpc_pubsub_addr: rpc_addrs.1,
        ledger_path,
        snapshot_output_dir,
        snapshot_path,
        account_paths,
        snapshot_info: snapshot_info.unwrap(),
        cluster_info,
        ..ReplicaNodeConfig::default()
    };

    let validator = ReplicaNode::new(config);
    validator.join();
}
