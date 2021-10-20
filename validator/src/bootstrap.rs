use {
    log::*,
    rand::{seq::SliceRandom, thread_rng, Rng},
    solana_client::rpc_client::RpcClient,
    solana_core::validator::{ValidatorConfig, ValidatorStartProgress},
    solana_download_utils::{download_snapshot_archive, DownloadProgressRecord},
    solana_genesis_utils::download_then_check_genesis_hash,
    solana_gossip::{
        cluster_info::{ClusterInfo, Node},
        contact_info::ContactInfo,
        crds_value,
        gossip_service::GossipService,
    },
    solana_runtime::{
        snapshot_archive_info::SnapshotArchiveInfoGetter,
        snapshot_package::SnapshotType,
        snapshot_utils::{
            self, DEFAULT_MAX_FULL_SNAPSHOT_ARCHIVES_TO_RETAIN,
            DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN,
        },
    },
    solana_sdk::{
        clock::Slot,
        commitment_config::CommitmentConfig,
        hash::Hash,
        pubkey::Pubkey,
        signature::{Keypair, Signer},
    },
    solana_streamer::socket::SocketAddrSpace,
    std::{
        collections::{HashMap, HashSet},
        net::{SocketAddr, TcpListener, UdpSocket},
        path::Path,
        process::exit,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::sleep,
        time::{Duration, Instant},
    },
};

#[derive(Debug)]
pub struct RpcBootstrapConfig {
    pub no_genesis_fetch: bool,
    pub no_snapshot_fetch: bool,
    pub no_untrusted_rpc: bool,
    pub max_genesis_archive_unpacked_size: u64,
    pub no_check_vote_account: bool,
    pub incremental_snapshot_fetch: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn rpc_bootstrap(
    node: &Node,
    identity_keypair: &Arc<Keypair>,
    ledger_path: &Path,
    snapshot_archives_dir: &Path,
    vote_account: &Pubkey,
    authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
    cluster_entrypoints: &[ContactInfo],
    validator_config: &mut ValidatorConfig,
    bootstrap_config: RpcBootstrapConfig,
    do_port_check: bool,
    use_progress_bar: bool,
    maximum_local_snapshot_age: Slot,
    should_check_duplicate_instance: bool,
    start_progress: &Arc<RwLock<ValidatorStartProgress>>,
    minimal_snapshot_download_speed: f32,
    maximum_snapshot_download_abort: u64,
    socket_addr_space: SocketAddrSpace,
) {
    if do_port_check {
        let mut order: Vec<_> = (0..cluster_entrypoints.len()).collect();
        order.shuffle(&mut thread_rng());
        if order.into_iter().all(|i| {
            !verify_reachable_ports(
                node,
                &cluster_entrypoints[i],
                validator_config,
                &socket_addr_space,
            )
        }) {
            exit(1);
        }
    }

    if bootstrap_config.no_genesis_fetch && bootstrap_config.no_snapshot_fetch {
        return;
    }

    if bootstrap_config.incremental_snapshot_fetch {
        debug!("rpc_bootstrap with incremental snapshot fetch");
        with_incremental_snapshots::rpc_bootstrap(
            node,
            identity_keypair,
            ledger_path,
            snapshot_archives_dir,
            vote_account,
            authorized_voter_keypairs,
            cluster_entrypoints,
            validator_config,
            bootstrap_config,
            use_progress_bar,
            maximum_local_snapshot_age,
            should_check_duplicate_instance,
            start_progress,
            minimal_snapshot_download_speed,
            maximum_snapshot_download_abort,
            socket_addr_space,
        )
    } else {
        debug!("rpc_bootstrap without incremental snapshot fetch");
        without_incremental_snapshots::rpc_bootstrap(
            node,
            identity_keypair,
            ledger_path,
            snapshot_archives_dir,
            vote_account,
            authorized_voter_keypairs,
            cluster_entrypoints,
            validator_config,
            bootstrap_config,
            use_progress_bar,
            maximum_local_snapshot_age,
            should_check_duplicate_instance,
            start_progress,
            minimal_snapshot_download_speed,
            maximum_snapshot_download_abort,
            socket_addr_space,
        )
    }
}

fn verify_reachable_ports(
    node: &Node,
    cluster_entrypoint: &ContactInfo,
    validator_config: &ValidatorConfig,
    socket_addr_space: &SocketAddrSpace,
) -> bool {
    let mut udp_sockets = vec![&node.sockets.gossip, &node.sockets.repair];

    if ContactInfo::is_valid_address(&node.info.serve_repair, socket_addr_space) {
        udp_sockets.push(&node.sockets.serve_repair);
    }
    if ContactInfo::is_valid_address(&node.info.tpu, socket_addr_space) {
        udp_sockets.extend(node.sockets.tpu.iter());
    }
    if ContactInfo::is_valid_address(&node.info.tpu_forwards, socket_addr_space) {
        udp_sockets.extend(node.sockets.tpu_forwards.iter());
    }
    if ContactInfo::is_valid_address(&node.info.tpu_vote, socket_addr_space) {
        udp_sockets.extend(node.sockets.tpu_vote.iter());
    }
    if ContactInfo::is_valid_address(&node.info.tvu, socket_addr_space) {
        udp_sockets.extend(node.sockets.tvu.iter());
        udp_sockets.extend(node.sockets.broadcast.iter());
        udp_sockets.extend(node.sockets.retransmit_sockets.iter());
    }
    if ContactInfo::is_valid_address(&node.info.tvu_forwards, socket_addr_space) {
        udp_sockets.extend(node.sockets.tvu_forwards.iter());
    }

    let mut tcp_listeners = vec![];
    if let Some((rpc_addr, rpc_pubsub_addr)) = validator_config.rpc_addrs {
        for (purpose, bind_addr, public_addr) in &[
            ("RPC", rpc_addr, &node.info.rpc),
            ("RPC pubsub", rpc_pubsub_addr, &node.info.rpc_pubsub),
        ] {
            if ContactInfo::is_valid_address(public_addr, socket_addr_space) {
                tcp_listeners.push((
                    bind_addr.port(),
                    TcpListener::bind(bind_addr).unwrap_or_else(|err| {
                        error!(
                            "Unable to bind to tcp {:?} for {}: {}",
                            bind_addr, purpose, err
                        );
                        exit(1);
                    }),
                ));
            }
        }
    }

    if let Some(ip_echo) = &node.sockets.ip_echo {
        let ip_echo = ip_echo.try_clone().expect("unable to clone tcp_listener");
        tcp_listeners.push((ip_echo.local_addr().unwrap().port(), ip_echo));
    }

    solana_net_utils::verify_reachable_ports(
        &cluster_entrypoint.gossip,
        tcp_listeners,
        &udp_sockets,
    )
}

fn is_trusted_validator(id: &Pubkey, trusted_validators: &Option<HashSet<Pubkey>>) -> bool {
    if let Some(trusted_validators) = trusted_validators {
        trusted_validators.contains(id)
    } else {
        false
    }
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
    socket_addr_space: SocketAddrSpace,
) -> (Arc<ClusterInfo>, Arc<AtomicBool>, GossipService) {
    let contact_info = ClusterInfo::gossip_contact_info(
        identity_keypair.pubkey(),
        *gossip_addr,
        expected_shred_version.unwrap_or(0),
    );
    let mut cluster_info = ClusterInfo::new(contact_info, identity_keypair, socket_addr_space);
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
    (cluster_info, gossip_exit_flag, gossip_service)
}

fn get_rpc_peers(
    cluster_info: &ClusterInfo,
    cluster_entrypoints: &[ContactInfo],
    validator_config: &ValidatorConfig,
    blacklisted_rpc_nodes: &mut HashSet<Pubkey>,
    blacklist_timeout: &Instant,
    retry_reason: &mut Option<String>,
) -> Option<Vec<ContactInfo>> {
    let shred_version = validator_config
        .expected_shred_version
        .unwrap_or_else(|| cluster_info.my_shred_version());
    if shred_version == 0 {
        let all_zero_shred_versions = cluster_entrypoints.iter().all(|cluster_entrypoint| {
            cluster_info
                .lookup_contact_info_by_gossip_addr(&cluster_entrypoint.gossip)
                .map_or(false, |entrypoint| entrypoint.shred_version == 0)
        });

        if all_zero_shred_versions {
            eprintln!("Entrypoint shred version is zero.  Restart with --expected-shred-version");
            exit(1);
        }
        info!("Waiting to adopt entrypoint shred version...");
        return None;
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

    // Filter out blacklisted nodes
    let rpc_peers: Vec<_> = rpc_peers
        .into_iter()
        .filter(|rpc_peer| !blacklisted_rpc_nodes.contains(&rpc_peer.id))
        .collect();
    let rpc_peers_blacklisted = rpc_peers_total - rpc_peers.len();
    let rpc_peers_trusted = rpc_peers
        .iter()
        .filter(|rpc_peer| is_trusted_validator(&rpc_peer.id, &validator_config.trusted_validators))
        .count();

    info!(
        "Total {} RPC nodes found. {} known, {} blacklisted ",
        rpc_peers_total, rpc_peers_trusted, rpc_peers_blacklisted
    );

    if rpc_peers_blacklisted == rpc_peers_total {
        *retry_reason =
            if !blacklisted_rpc_nodes.is_empty() && blacklist_timeout.elapsed().as_secs() > 60 {
                // If all nodes are blacklisted and no additional nodes are discovered after 60 seconds,
                // remove the blacklist and try them all again
                blacklisted_rpc_nodes.clear();
                Some("Blacklist timeout expired".to_owned())
            } else {
                Some("Wait for known rpc peers".to_owned())
            };
        return None;
    }

    Some(rpc_peers)
}

fn check_vote_account(
    rpc_client: &RpcClient,
    identity_pubkey: &Pubkey,
    vote_account_address: &Pubkey,
    authorized_voter_pubkeys: &[Pubkey],
) -> Result<(), String> {
    let vote_account = rpc_client
        .get_account_with_commitment(vote_account_address, CommitmentConfig::confirmed())
        .map_err(|err| format!("failed to fetch vote account: {}", err.to_string()))?
        .value
        .ok_or_else(|| format!("vote account does not exist: {}", vote_account_address))?;

    if vote_account.owner != solana_vote_program::id() {
        return Err(format!(
            "not a vote account (owned by {}): {}",
            vote_account.owner, vote_account_address
        ));
    }

    let identity_account = rpc_client
        .get_account_with_commitment(identity_pubkey, CommitmentConfig::confirmed())
        .map_err(|err| format!("failed to fetch identity account: {}", err.to_string()))?
        .value
        .ok_or_else(|| format!("identity account does not exist: {}", identity_pubkey))?;

    let vote_state = solana_vote_program::vote_state::VoteState::from(&vote_account);
    if let Some(vote_state) = vote_state {
        if vote_state.authorized_voters().is_empty() {
            return Err("Vote account not yet initialized".to_string());
        }

        if vote_state.node_pubkey != *identity_pubkey {
            return Err(format!(
                "vote account's identity ({}) does not match the validator's identity {}).",
                vote_state.node_pubkey, identity_pubkey
            ));
        }

        for (_, vote_account_authorized_voter_pubkey) in vote_state.authorized_voters().iter() {
            if !authorized_voter_pubkeys.contains(vote_account_authorized_voter_pubkey) {
                return Err(format!(
                    "authorized voter {} not available",
                    vote_account_authorized_voter_pubkey
                ));
            }
        }
    } else {
        return Err(format!(
            "invalid vote account data for {}",
            vote_account_address
        ));
    }

    // Maybe we can calculate minimum voting fee; rather than 1 lamport
    if identity_account.lamports <= 1 {
        return Err(format!(
            "underfunded identity account ({}): only {} lamports available",
            identity_pubkey, identity_account.lamports
        ));
    }

    Ok(())
}

/// Get the Slot and Hash of the local snapshot with the highest slot.  Can be either a full
/// snapshot or an incremental snapshot.
fn get_highest_local_snapshot_hash(
    snapshot_archives_dir: impl AsRef<Path>,
) -> Option<(Slot, Hash)> {
    if let Some(full_snapshot_info) =
        snapshot_utils::get_highest_full_snapshot_archive_info(&snapshot_archives_dir)
    {
        if let Some(incremental_snapshot_info) =
            snapshot_utils::get_highest_incremental_snapshot_archive_info(
                &snapshot_archives_dir,
                full_snapshot_info.slot(),
            )
        {
            Some((
                incremental_snapshot_info.slot(),
                *incremental_snapshot_info.hash(),
            ))
        } else {
            Some((full_snapshot_info.slot(), *full_snapshot_info.hash()))
        }
    } else {
        None
    }
}

mod without_incremental_snapshots {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    pub fn rpc_bootstrap(
        node: &Node,
        identity_keypair: &Arc<Keypair>,
        ledger_path: &Path,
        snapshot_archives_dir: &Path,
        vote_account: &Pubkey,
        authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
        cluster_entrypoints: &[ContactInfo],
        validator_config: &mut ValidatorConfig,
        bootstrap_config: RpcBootstrapConfig,
        use_progress_bar: bool,
        maximum_local_snapshot_age: Slot,
        should_check_duplicate_instance: bool,
        start_progress: &Arc<RwLock<ValidatorStartProgress>>,
        minimal_snapshot_download_speed: f32,
        maximum_snapshot_download_abort: u64,
        socket_addr_space: SocketAddrSpace,
    ) {
        let mut blacklisted_rpc_nodes = HashSet::new();
        let mut gossip = None;
        let mut download_abort_count = 0;
        loop {
            if gossip.is_none() {
                *start_progress.write().unwrap() = ValidatorStartProgress::SearchingForRpcService;

                gossip = Some(start_gossip_node(
                    identity_keypair.clone(),
                    cluster_entrypoints,
                    ledger_path,
                    &node.info.gossip,
                    node.sockets.gossip.try_clone().unwrap(),
                    validator_config.expected_shred_version,
                    validator_config.gossip_validators.clone(),
                    should_check_duplicate_instance,
                    socket_addr_space,
                ));
            }

            let rpc_node_details = get_rpc_node(
                &gossip.as_ref().unwrap().0,
                cluster_entrypoints,
                validator_config,
                &mut blacklisted_rpc_nodes,
                &bootstrap_config,
                snapshot_archives_dir,
            );
            if rpc_node_details.is_none() {
                return;
            }
            let (rpc_contact_info, snapshot_hash) = rpc_node_details.unwrap();

            info!(
                "Using RPC service from node {}: {:?}",
                rpc_contact_info.id, rpc_contact_info.rpc
            );
            let rpc_client = RpcClient::new_socket(rpc_contact_info.rpc);

            let result = match rpc_client.get_version() {
            Ok(rpc_version) => {
                info!("RPC node version: {}", rpc_version.solana_core);
                Ok(())
            }
            Err(err) => Err(format!("Failed to get RPC node version: {}", err)),
        }
        .and_then(|_| {
            let genesis_config = download_then_check_genesis_hash(
                &rpc_contact_info.rpc,
                ledger_path,
                validator_config.expected_genesis_hash,
                bootstrap_config.max_genesis_archive_unpacked_size,
                bootstrap_config.no_genesis_fetch,
                use_progress_bar,
            );

            if let Ok(genesis_config) = genesis_config {
                let genesis_hash = genesis_config.hash();
                if validator_config.expected_genesis_hash.is_none() {
                    info!("Expected genesis hash set to {}", genesis_hash);
                    validator_config.expected_genesis_hash = Some(genesis_hash);
                }
            }

            if let Some(expected_genesis_hash) = validator_config.expected_genesis_hash {
                // Sanity check that the RPC node is using the expected genesis hash before
                // downloading a snapshot from it
                let rpc_genesis_hash = rpc_client
                    .get_genesis_hash()
                    .map_err(|err| format!("Failed to get genesis hash: {}", err))?;

                if expected_genesis_hash != rpc_genesis_hash {
                    return Err(format!(
                        "Genesis hash mismatch: expected {} but RPC node genesis hash is {}",
                        expected_genesis_hash, rpc_genesis_hash
                    ));
                }
            }

            if let Some(snapshot_hash) = snapshot_hash {
                let use_local_snapshot = match get_highest_local_snapshot_hash(snapshot_archives_dir) {
                    None => {
                        info!("Downloading snapshot for slot {} since there is not a local snapshot", snapshot_hash.0);
                        false
                    }
                    Some((highest_local_snapshot_slot, _hash)) => {
                        if highest_local_snapshot_slot
                            > snapshot_hash.0.saturating_sub(maximum_local_snapshot_age)
                        {
                            info!(
                                "Reusing local snapshot at slot {} instead \
                                   of downloading a snapshot for slot {}",
                                highest_local_snapshot_slot, snapshot_hash.0
                            );
                            true
                        } else {
                            info!(
                                "Local snapshot from slot {} is too old. \
                                  Downloading a newer snapshot for slot {}",
                                highest_local_snapshot_slot, snapshot_hash.0
                            );
                            false
                        }
                    }
                };

                if use_local_snapshot {
                    Ok(())
                } else {
                    rpc_client
                        .get_slot_with_commitment(CommitmentConfig::finalized())
                        .map_err(|err| format!("Failed to get RPC node slot: {}", err))
                        .and_then(|slot| {
                            *start_progress.write().unwrap() =
                                ValidatorStartProgress::DownloadingSnapshot {
                                    slot: snapshot_hash.0,
                                    rpc_addr: rpc_contact_info.rpc,
                                };
                            info!("RPC node root slot: {}", slot);
                            let (cluster_info, gossip_exit_flag, gossip_service) =
                                gossip.take().unwrap();
                            cluster_info.save_contact_info();
                            gossip_exit_flag.store(true, Ordering::Relaxed);
                            let (maximum_full_snapshot_archives_to_retain, maximum_incremental_snapshot_archives_to_retain) = if let Some(snapshot_config) =
                                validator_config.snapshot_config.as_ref()
                            {
                                (snapshot_config.maximum_full_snapshot_archives_to_retain, snapshot_config.maximum_incremental_snapshot_archives_to_retain)
                            } else {
                                (DEFAULT_MAX_FULL_SNAPSHOT_ARCHIVES_TO_RETAIN, DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN)
                            };
                            let ret = download_snapshot_archive(
                                &rpc_contact_info.rpc,
                                snapshot_archives_dir,
                                snapshot_hash,
                                SnapshotType::FullSnapshot,
                                maximum_full_snapshot_archives_to_retain,
                                maximum_incremental_snapshot_archives_to_retain,
                                use_progress_bar,
                                &mut Some(Box::new(|download_progress: &DownloadProgressRecord| {
                                    debug!("Download progress: {:?}", download_progress);

                                    if download_progress.last_throughput <  minimal_snapshot_download_speed
                                       && download_progress.notification_count <= 1
                                       && download_progress.percentage_done <= 2_f32
                                       && download_progress.estimated_remaining_time > 60_f32
                                       && download_abort_count < maximum_snapshot_download_abort {
                                        if let Some(ref trusted_validators) = validator_config.trusted_validators {
                                            if trusted_validators.contains(&rpc_contact_info.id)
                                               && trusted_validators.len() == 1
                                               && bootstrap_config.no_untrusted_rpc {
                                                warn!("The snapshot download is too slow, throughput: {} < min speed {} bytes/sec, but will NOT abort \
                                                      and try a different node as it is the only known validator and the --only-known-rpc flag \
                                                      is set. \
                                                      Abort count: {}, Progress detail: {:?}",
                                                      download_progress.last_throughput, minimal_snapshot_download_speed,
                                                      download_abort_count, download_progress);
                                                return true; // Do not abort download from the one-and-only known validator
                                            }
                                        }
                                        warn!("The snapshot download is too slow, throughput: {} < min speed {} bytes/sec, will abort \
                                               and try a different node. Abort count: {}, Progress detail: {:?}",
                                               download_progress.last_throughput, minimal_snapshot_download_speed,
                                               download_abort_count, download_progress);
                                        download_abort_count += 1;
                                        false
                                    } else {
                                        true
                                    }
                                })),
                            );

                            gossip_service.join().unwrap();
                            ret
                        })
                }
            } else {
                Ok(())
            }
        })
        .map(|_| {
            if !validator_config.voting_disabled && !bootstrap_config.no_check_vote_account {
                check_vote_account(
                    &rpc_client,
                    &identity_keypair.pubkey(),
                    vote_account,
                    &authorized_voter_keypairs
                        .read()
                        .unwrap()
                        .iter()
                        .map(|k| k.pubkey())
                        .collect::<Vec<_>>(),
                )
                .unwrap_or_else(|err| {
                    // Consider failures here to be more likely due to user error (eg,
                    // incorrect `solana-validator` command-line arguments) rather than the
                    // RPC node failing.
                    //
                    // Power users can always use the `--no-check-vote-account` option to
                    // bypass this check entirely
                    error!("{}", err);
                    exit(1);
                });
            }
        });

            if result.is_ok() {
                break;
            }
            warn!("{}", result.unwrap_err());

            if let Some(ref trusted_validators) = validator_config.trusted_validators {
                if trusted_validators.contains(&rpc_contact_info.id) {
                    continue; // Never blacklist a trusted node
                }
            }

            info!(
                "Excluding {} as a future RPC candidate",
                rpc_contact_info.id
            );
            blacklisted_rpc_nodes.insert(rpc_contact_info.id);
        }
        if let Some((cluster_info, gossip_exit_flag, gossip_service)) = gossip.take() {
            cluster_info.save_contact_info();
            gossip_exit_flag.store(true, Ordering::Relaxed);
            gossip_service.join().unwrap();
        }
    }

    fn get_rpc_node(
        cluster_info: &ClusterInfo,
        cluster_entrypoints: &[ContactInfo],
        validator_config: &ValidatorConfig,
        blacklisted_rpc_nodes: &mut HashSet<Pubkey>,
        bootstrap_config: &RpcBootstrapConfig,
        snapshot_archives_dir: &Path,
    ) -> Option<(ContactInfo, Option<(Slot, Hash)>)> {
        let mut blacklist_timeout = Instant::now();
        let mut newer_cluster_snapshot_timeout = None;
        let mut retry_reason = None;
        loop {
            sleep(Duration::from_secs(1));
            info!("\n{}", cluster_info.rpc_info_trace());

            let rpc_peers = get_rpc_peers(
                cluster_info,
                cluster_entrypoints,
                validator_config,
                blacklisted_rpc_nodes,
                &blacklist_timeout,
                &mut retry_reason,
            );
            if rpc_peers.is_none() {
                continue;
            }
            let rpc_peers = rpc_peers.unwrap();
            blacklist_timeout = Instant::now();

            let mut highest_snapshot_hash = get_highest_local_snapshot_hash(snapshot_archives_dir);
            let eligible_rpc_peers = if bootstrap_config.no_snapshot_fetch {
                rpc_peers
            } else {
                let trusted_snapshot_hashes =
                    get_trusted_snapshot_hashes(cluster_info, &validator_config.trusted_validators);

                let mut eligible_rpc_peers = vec![];

                for rpc_peer in rpc_peers.iter() {
                    if bootstrap_config.no_untrusted_rpc
                        && !is_trusted_validator(&rpc_peer.id, &validator_config.trusted_validators)
                    {
                        continue;
                    }
                    cluster_info.get_snapshot_hash_for_node(&rpc_peer.id, |snapshot_hashes| {
                        for snapshot_hash in snapshot_hashes {
                            if let Some(ref trusted_snapshot_hashes) = trusted_snapshot_hashes {
                                if !trusted_snapshot_hashes.contains(snapshot_hash) {
                                    // Ignore all untrusted snapshot hashes
                                    continue;
                                }
                            }

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

    fn get_trusted_snapshot_hashes(
        cluster_info: &ClusterInfo,
        trusted_validators: &Option<HashSet<Pubkey>>,
    ) -> Option<HashSet<(Slot, Hash)>> {
        if let Some(trusted_validators) = trusted_validators {
            let mut trusted_snapshot_hashes = HashSet::new();
            for trusted_validator in trusted_validators {
                cluster_info.get_snapshot_hash_for_node(trusted_validator, |snapshot_hashes| {
                    for snapshot_hash in snapshot_hashes {
                        trusted_snapshot_hashes.insert(*snapshot_hash);
                    }
                });
            }
            Some(trusted_snapshot_hashes)
        } else {
            None
        }
    }
}

mod with_incremental_snapshots {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    pub fn rpc_bootstrap(
        node: &Node,
        identity_keypair: &Arc<Keypair>,
        ledger_path: &Path,
        snapshot_archives_dir: &Path,
        vote_account: &Pubkey,
        authorized_voter_keypairs: Arc<RwLock<Vec<Arc<Keypair>>>>,
        cluster_entrypoints: &[ContactInfo],
        validator_config: &mut ValidatorConfig,
        bootstrap_config: RpcBootstrapConfig,
        use_progress_bar: bool,
        maximum_local_snapshot_age: Slot,
        should_check_duplicate_instance: bool,
        start_progress: &Arc<RwLock<ValidatorStartProgress>>,
        minimal_snapshot_download_speed: f32,
        maximum_snapshot_download_abort: u64,
        socket_addr_space: SocketAddrSpace,
    ) {
        let mut blacklisted_rpc_nodes = HashSet::new();
        let mut gossip = None;
        let mut download_abort_count = 0;
        loop {
            if gossip.is_none() {
                *start_progress.write().unwrap() = ValidatorStartProgress::SearchingForRpcService;

                gossip = Some(start_gossip_node(
                    identity_keypair.clone(),
                    cluster_entrypoints,
                    ledger_path,
                    &node.info.gossip,
                    node.sockets.gossip.try_clone().unwrap(),
                    validator_config.expected_shred_version,
                    validator_config.gossip_validators.clone(),
                    should_check_duplicate_instance,
                    socket_addr_space,
                ));
            }

            let rpc_node_details = get_rpc_node(
                &gossip.as_ref().unwrap().0,
                cluster_entrypoints,
                validator_config,
                &mut blacklisted_rpc_nodes,
                &bootstrap_config,
            );
            if rpc_node_details.is_none() {
                return;
            }
            let (rpc_contact_info, snapshot_hashes) = rpc_node_details.unwrap();

            info!(
                "Using RPC service from node {}: {:?}",
                rpc_contact_info.id, rpc_contact_info.rpc
            );
            let rpc_client = RpcClient::new_socket(rpc_contact_info.rpc);

            let result = match rpc_client.get_version() {
                Ok(rpc_version) => {
                    info!("RPC node version: {}", rpc_version.solana_core);
                    Ok(())
                }
                Err(err) => Err(format!("Failed to get RPC node version: {}", err)),
            }
            .and_then(|_| {
                let genesis_config = download_then_check_genesis_hash(
                    &rpc_contact_info.rpc,
                    ledger_path,
                    validator_config.expected_genesis_hash,
                    bootstrap_config.max_genesis_archive_unpacked_size,
                    bootstrap_config.no_genesis_fetch,
                    use_progress_bar,
                );

                if let Ok(genesis_config) = genesis_config {
                    let genesis_hash = genesis_config.hash();
                    if validator_config.expected_genesis_hash.is_none() {
                        info!("Expected genesis hash set to {}", genesis_hash);
                        validator_config.expected_genesis_hash = Some(genesis_hash);
                    }
                }

                if let Some(expected_genesis_hash) = validator_config.expected_genesis_hash {
                    // Sanity check that the RPC node is using the expected genesis hash before
                    // downloading a snapshot from it
                    let rpc_genesis_hash = rpc_client
                        .get_genesis_hash()
                        .map_err(|err| format!("Failed to get genesis hash: {}", err))?;

                    if expected_genesis_hash != rpc_genesis_hash {
                        return Err(format!(
                            "Genesis hash mismatch: expected {} but RPC node genesis hash is {}",
                            expected_genesis_hash, rpc_genesis_hash
                        ));
                    }
                }

                let (cluster_info, gossip_exit_flag, gossip_service) = gossip.take().unwrap();
                cluster_info.save_contact_info();
                gossip_exit_flag.store(true, Ordering::Relaxed);
                gossip_service.join().unwrap();

                let rpc_client_slot = rpc_client
                    .get_slot_with_commitment(CommitmentConfig::finalized())
                    .map_err(|err| format!("Failed to get RPC node slot: {}", err))?;
                info!("RPC node root slot: {}", rpc_client_slot);

                download_snapshots(
                    snapshot_archives_dir,
                    validator_config,
                    &bootstrap_config,
                    use_progress_bar,
                    maximum_local_snapshot_age,
                    start_progress,
                    minimal_snapshot_download_speed,
                    maximum_snapshot_download_abort,
                    &mut download_abort_count,
                    snapshot_hashes,
                    &rpc_contact_info,
                )
            })
            .map(|_| {
                if !validator_config.voting_disabled && !bootstrap_config.no_check_vote_account {
                    check_vote_account(
                        &rpc_client,
                        &identity_keypair.pubkey(),
                        vote_account,
                        &authorized_voter_keypairs
                            .read()
                            .unwrap()
                            .iter()
                            .map(|k| k.pubkey())
                            .collect::<Vec<_>>(),
                    )
                    .unwrap_or_else(|err| {
                        // Consider failures here to be more likely due to user error (eg,
                        // incorrect `solana-validator` command-line arguments) rather than the
                        // RPC node failing.
                        //
                        // Power users can always use the `--no-check-vote-account` option to
                        // bypass this check entirely
                        error!("{}", err);
                        exit(1);
                    });
                }
            });

            if result.is_ok() {
                break;
            }
            warn!("{}", result.unwrap_err());

            if let Some(ref trusted_validators) = validator_config.trusted_validators {
                if trusted_validators.contains(&rpc_contact_info.id) {
                    continue; // Never blacklist a trusted node
                }
            }

            info!(
                "Excluding {} as a future RPC candidate",
                rpc_contact_info.id
            );
            blacklisted_rpc_nodes.insert(rpc_contact_info.id);
        }
        if let Some((cluster_info, gossip_exit_flag, gossip_service)) = gossip.take() {
            cluster_info.save_contact_info();
            gossip_exit_flag.store(true, Ordering::Relaxed);
            gossip_service.join().unwrap();
        }
    }

    /// Get an RPC peer node to download from.
    ///
    /// This function finds the highest compatible snapshots from the cluster, then picks one peer
    /// at random to use (return).  The chosen snapshots are:
    /// 1. have a full snapshot slot that is a multiple of our full snapshot interval
    /// 2. have the highest full snapshot slot
    /// 3. have the highest incremental snapshot slot
    ///
    /// NOTE: If the node has configured a full snapshot interval that is non-standard, it is
    /// possible that there are no compatible snapshot hashes available.  At that time, a node may
    /// (1) try again, (2) change its full snapshot interval back to a standard/default value, or
    /// (3) disable downloading an incremental snapshot and instead download the highest full
    /// snapshot hash, regardless of compatibility.
    #[allow(clippy::type_complexity)]
    fn get_rpc_node(
        cluster_info: &ClusterInfo,
        cluster_entrypoints: &[ContactInfo],
        validator_config: &ValidatorConfig,
        blacklisted_rpc_nodes: &mut HashSet<Pubkey>,
        bootstrap_config: &RpcBootstrapConfig,
    ) -> Option<(ContactInfo, Option<((Slot, Hash), Option<(Slot, Hash)>)>)> {
        let mut blacklist_timeout = Instant::now();
        let mut newer_cluster_snapshot_timeout = None;
        let mut retry_reason = None;
        loop {
            sleep(Duration::from_secs(1));
            info!("\n{}", cluster_info.rpc_info_trace());

            let rpc_peers = get_rpc_peers(
                cluster_info,
                cluster_entrypoints,
                validator_config,
                blacklisted_rpc_nodes,
                &blacklist_timeout,
                &mut retry_reason,
            );
            if rpc_peers.is_none() {
                continue;
            }
            let rpc_peers = rpc_peers.unwrap();
            blacklist_timeout = Instant::now();

            if bootstrap_config.no_snapshot_fetch {
                if rpc_peers.is_empty() {
                    retry_reason = Some("No RPC peers available.".to_owned());
                    continue;
                } else {
                    let random_peer = &rpc_peers[thread_rng().gen_range(0, rpc_peers.len())];
                    return Some((random_peer.clone(), None));
                }
            }

            let trusted_incremental_snapshot_hashes =
                get_trusted_incremental_snapshot_hashes(cluster_info, validator_config);

            let mut incremental_snapshot_hashes =
                get_incremental_snapshot_hashes_from_eligible_peers(
                    cluster_info,
                    validator_config,
                    bootstrap_config,
                    &rpc_peers,
                );

            retain_trusted_incremental_snapshot_hashes(
                &trusted_incremental_snapshot_hashes,
                &mut incremental_snapshot_hashes,
            );

            retain_incremental_snapshot_hashes_with_a_multiple_of_full_snapshot_archive_interval(
                validator_config
                    .snapshot_config
                    .as_ref()
                    .unwrap()
                    .full_snapshot_archive_interval_slots,
                &mut incremental_snapshot_hashes,
            );

            retain_incremental_snapshot_hashes_with_highest_full_snapshot_slot(
                &mut incremental_snapshot_hashes,
            );

            retain_incremental_snapshot_hashes_with_highest_incremental_snapshot_slot(
                &mut incremental_snapshot_hashes,
            );

            if incremental_snapshot_hashes.is_empty() {
                match newer_cluster_snapshot_timeout {
                    None => newer_cluster_snapshot_timeout = Some(Instant::now()),
                    Some(newer_cluster_snapshot_timeout) => {
                        if newer_cluster_snapshot_timeout.elapsed().as_secs() > 180 {
                            warn!(
                                "Giving up, did not get newer snapshots from the cluster. \
                              If this persists, it may be that there are no nodes with \
                              the same full snapshot interval. Try using the default \
                              value (i.e. do not set --full-snapshot-interval-slots). \
                              Alternatively, disable fetching incremental snapshots, just \
                              fetch full snapshots, by setting --no-incremental-snapshot-fetch."
                            );
                            return None;
                        }
                    }
                }
                retry_reason = Some("No snapshots available".to_owned());
                continue;
            } else {
                let rpc_peers = incremental_snapshot_hashes
                    .iter()
                    .map(|(rpc_peer, ..)| rpc_peer.id)
                    .collect::<Vec<_>>();
                let (final_rpc_peer, final_full_snapshot_hash, final_incremental_snapshot_hash) =
                    get_final_peer_and_incremental_snapshot_hash(&incremental_snapshot_hashes);
                info!(
                    "Highest available snapshot slot is {}, available from {} node{}: {:?}",
                    final_incremental_snapshot_hash
                        .map(|(slot, _hash)| slot)
                        .unwrap_or(final_full_snapshot_hash.0),
                    rpc_peers.len(),
                    if rpc_peers.len() > 1 { "s" } else { "" },
                    rpc_peers,
                );
                return Some((
                    final_rpc_peer,
                    Some((final_full_snapshot_hash, final_incremental_snapshot_hash)),
                ));
            }
        }
    }

    /// Get the incremental snapshot hashes from trusted peers.
    ///
    /// The hashes are put into a map from full snapshot hash to a set of incremental snapshot
    /// hashes.  This map will be used as the golden hashes; when peers are queried for their
    /// individual incremental snapshot hashes, their results will be checked against this map to
    /// verify correctness.
    ///
    /// NOTE: Only a single full snashot hash is allowed per slot.  If somehow two trusted peers
    /// have a full snapshot hash with the same slot and _different_ hashes, the second will be
    /// skipped, and its incremental snapshot hashes will not be added to the map.
    fn get_trusted_incremental_snapshot_hashes(
        cluster_info: &ClusterInfo,
        validator_config: &ValidatorConfig,
    ) -> HashMap<(Slot, Hash), HashSet<(Slot, Hash)>> {
        let mut trusted_incremental_snapshot_hashes: HashMap<(Slot, Hash), HashSet<(Slot, Hash)>> =
            HashMap::new();
        validator_config
        .trusted_validators
        .iter()
        .for_each(|trusted_validators| {
            trusted_validators
            .iter()
            .for_each(|trusted_validator| {
                if let Some(crds_value::IncrementalSnapshotHashes {base: full_snapshot_hash, hashes: incremental_snapshot_hashes, ..}) = cluster_info.get_incremental_snapshot_hashes_for_node(trusted_validator) {
                    match trusted_incremental_snapshot_hashes.get_mut(&full_snapshot_hash) {
                        Some(hashes) => {
                            // Do not add these hashes if there's already an incremental
                            // snapshot hash with this same slot, but with a _different_ hash.
                            // NOTE: There's no good reason for trusted validators to
                            // produce incremental snapshots at the same slot with
                            // different hashes, so this should not happen.
                            for incremental_snapshot_hash in incremental_snapshot_hashes {
                                if !hashes.iter().any(|(slot, hash)| slot == &incremental_snapshot_hash.0 && hash != &incremental_snapshot_hash.1) {
                                    hashes.insert(incremental_snapshot_hash);
                                } else {
                                    info!("Ignoring incremental snapshot hash from trusted validator {} with a slot we've already seen (slot: {}), but a different hash.", trusted_validator, incremental_snapshot_hash.0);
                                }
                            }
                        }
                        None => {
                            // Do not add these hashes if there's already a full snapshot hash
                            // with the same slot but with a _different_ hash.
                            // NOTE: There's no good reason for trusted validators to
                            // produce full snapshots at the same slot with different
                            // hashes, so this should not happen.
                            if !trusted_incremental_snapshot_hashes.keys().any(
                                |(slot, hash)| {
                                    slot == &full_snapshot_hash.0
                                        && hash != &full_snapshot_hash.1
                                },
                            ) {
                                let mut hashes = HashSet::new();
                                hashes.extend(incremental_snapshot_hashes);
                                trusted_incremental_snapshot_hashes
                                    .insert(full_snapshot_hash, hashes);
                            } else {
                                info!("Ignoring full snapshot hashes from trusted validator {} with a slot we've already seen (slot: {}), but a different hash.", trusted_validator, full_snapshot_hash.0);
                            }
                        }
                    }
                }
            })
        });

        trace!(
            "incremental snapshot hashes from trusted peers: {:?}",
            &trusted_incremental_snapshot_hashes
        );
        trusted_incremental_snapshot_hashes
    }

    /// Get incremental snapshot hashes from all the eligible peers.  This fn will get only one
    /// incremental snapshot hash per peer (the one with the highest slot).  This may be just a full
    /// snapshot hash, or a combo full (i.e. base) snapshot hash and incremental snapshot hash.
    ///
    /// Returns a vector of tuples, which are:
    /// - ContactInfo:          the rpc peer
    /// - (Slot, Hash):         the base (i.e. full) snapshot hash
    /// - Option<(Slot, Hash)>: the highest incremental snapshot hash, if one exists
    #[allow(clippy::type_complexity)]
    fn get_incremental_snapshot_hashes_from_eligible_peers(
        cluster_info: &ClusterInfo,
        validator_config: &ValidatorConfig,
        bootstrap_config: &RpcBootstrapConfig,
        rpc_peers: &[ContactInfo],
    ) -> Vec<(ContactInfo, (Slot, Hash), Option<(Slot, Hash)>)> {
        let mut incremental_snapshot_hashes = Vec::new();
        for rpc_peer in rpc_peers {
            if bootstrap_config.no_untrusted_rpc
                && !is_trusted_validator(&rpc_peer.id, &validator_config.trusted_validators)
            {
                // Ignore all untrusted peers
                continue;
            }

            cluster_info
                .get_incremental_snapshot_hashes_for_node(&rpc_peer.id)
                .and_then(
                    |crds_value::IncrementalSnapshotHashes { base, hashes, .. }| {
                        // Newer hashes are pushed to the end of `hashes`, so the last element should
                        // be the newest (i.e. have the highest slot).
                        //
                        // NOTE: It is important that the result of `last().map()` is the return  value
                        // from `and_then()`, so that the `or_else()` can run in _both_ scenarios where
                        // either (1) the peer does not have incremental snapshots enabled, or (2) the
                        // peer has not generated any incremental snapshots yet.
                        hashes.last().map(|incremental_snapshot_hash| {
                            incremental_snapshot_hashes.push((
                                rpc_peer.clone(),
                                base,
                                Some(*incremental_snapshot_hash),
                            ))
                        })
                    },
                )
                // If the peer does not have any incremental snapshot hashes, then get its highest full
                // snapshot hash instead.
                .or_else(|| {
                    cluster_info.get_snapshot_hash_for_node(&rpc_peer.id, |hashes| {
                        if let Some(full_snapshot_hash) = hashes.last() {
                            incremental_snapshot_hashes.push((
                                rpc_peer.clone(),
                                *full_snapshot_hash,
                                None,
                            ))
                        }
                    })
                });
        }

        trace!(
            "incremental snapshot hashes from eligible peers: {:?}",
            &incremental_snapshot_hashes
        );
        incremental_snapshot_hashes
    }

    /// Retain the incremental snapshot hashes that match a hash from a trusted peer
    #[allow(clippy::type_complexity)]
    fn retain_trusted_incremental_snapshot_hashes(
        trusted_incremental_snapshot_hashes: &HashMap<(Slot, Hash), HashSet<(Slot, Hash)>>,
        incremental_snapshot_hashes: &mut Vec<(ContactInfo, (Slot, Hash), Option<(Slot, Hash)>)>,
    ) {
        incremental_snapshot_hashes.retain(
            |(_rpc_peer, full_snapshot_hash, incremental_snapshot_hash)| {
                trusted_incremental_snapshot_hashes
                    .get(full_snapshot_hash)
                    .map(|trusted_incremental_hashes| {
                        if incremental_snapshot_hash.is_none() {
                            false
                        } else {
                            trusted_incremental_hashes
                                .contains(incremental_snapshot_hash.as_ref().unwrap())
                        }
                    })
                    .unwrap_or(false)
            },
        );

        trace!(
            "retain trusted incremental snapshot hashes: {:?}",
            &incremental_snapshot_hashes
        );
    }

    /// Retain the incremental snapshot hashes with a full snapshot slot that is a multiple of the full
    /// snapshot archive interval
    #[allow(clippy::type_complexity)]
    fn retain_incremental_snapshot_hashes_with_a_multiple_of_full_snapshot_archive_interval(
        full_snapshot_archive_interval: Slot,
        incremental_snapshot_hashes: &mut Vec<(ContactInfo, (Slot, Hash), Option<(Slot, Hash)>)>,
    ) {
        incremental_snapshot_hashes.retain(
            |(_rpc_peer, full_snapshot_hash, _incremental_snapshot_hash)| {
                full_snapshot_hash.0 % full_snapshot_archive_interval == 0
            },
        );

        trace!(
                "retain incremental snapshot hashes with a multiple of full snapshot archive interval: {:?}",
                &incremental_snapshot_hashes
            );
    }

    /// Retain the incremental snapshot hashes with the highest full snapshot slot
    #[allow(clippy::type_complexity)]
    fn retain_incremental_snapshot_hashes_with_highest_full_snapshot_slot(
        incremental_snapshot_hashes: &mut Vec<(ContactInfo, (Slot, Hash), Option<(Slot, Hash)>)>,
    ) {
        // retain the hashes with the highest full snapshot slot
        // do a two-pass algorithm
        // 1. find the full snapshot hash with the highest full snapshot slot
        // 2. retain elems with that full snapshot hash
        let mut highest_full_snapshot_hash = (Slot::MIN, Hash::default());
        for (_rpc_peer, full_snapshot_hash, _incremental_snapshot_hash) in
            incremental_snapshot_hashes.iter()
        {
            if full_snapshot_hash.0 > highest_full_snapshot_hash.0 {
                highest_full_snapshot_hash = *full_snapshot_hash;
            }
        }

        incremental_snapshot_hashes.retain(
            |(_rpc_peer, full_snapshot_hash, _incremental_snapshot_hash)| {
                full_snapshot_hash == &highest_full_snapshot_hash
            },
        );

        trace!(
            "retain incremental snapshot hashes with highest full snapshot slot: {:?}",
            &incremental_snapshot_hashes
        );
    }

    /// Retain the incremental snapshot hashes with the highest incremental snapshot slot
    #[allow(clippy::type_complexity)]
    fn retain_incremental_snapshot_hashes_with_highest_incremental_snapshot_slot(
        incremental_snapshot_hashes: &mut Vec<(ContactInfo, (Slot, Hash), Option<(Slot, Hash)>)>,
    ) {
        let mut highest_incremental_snapshot_hash: Option<(Slot, Hash)> = None;
        for (_rpc_peer, _full_snapshot_hash, incremental_snapshot_hash) in
            incremental_snapshot_hashes.iter()
        {
            if incremental_snapshot_hash.is_none() {
                continue;
            }
            let incremental_snapshot_hash = incremental_snapshot_hash.as_ref().unwrap();

            if highest_incremental_snapshot_hash.is_none()
                || incremental_snapshot_hash.0 > highest_incremental_snapshot_hash.unwrap().0
            {
                highest_incremental_snapshot_hash = Some(*incremental_snapshot_hash);
            }
        }

        incremental_snapshot_hashes.retain(
            |(_rpc_peer, _full_snapshot_hash, incremental_snapshot_hash)| {
                incremental_snapshot_hash == &highest_incremental_snapshot_hash
            },
        );

        trace!(
            "retain incremental snapshot hashes with highest incremental snapshot slot: {:?}",
            &incremental_snapshot_hashes
        );
    }

    /// Get a final peer from the remaining incremental snapshot hashes.  At this point all the
    /// snapshot hashes should (must) be the same, and only the peers are different.  Pick an element
    /// from the vector at random and return it.
    #[allow(clippy::type_complexity)]
    fn get_final_peer_and_incremental_snapshot_hash(
        incremental_snapshot_hashes: &[(ContactInfo, (Slot, Hash), Option<(Slot, Hash)>)],
    ) -> (ContactInfo, (Slot, Hash), Option<(Slot, Hash)>) {
        assert!(!incremental_snapshot_hashes.is_empty());

        // pick a final rpc peer at random
        let (final_rpc_peer, final_full_snapshot_hash, final_incremental_snapshot_hash) =
            &incremental_snapshot_hashes
                [thread_rng().gen_range(0, incremental_snapshot_hashes.len())];

        // It is a programmer bug if the assert fires!  By the time this function is called, the
        // only remaining `incremental_snapshot_hashes` should all be the same.
        assert!(
            incremental_snapshot_hashes.iter().all(
                |(_, full_snapshot_hash, incremental_snapshot_hash)| {
                    full_snapshot_hash == final_full_snapshot_hash
                        && incremental_snapshot_hash == final_incremental_snapshot_hash
                }
            ),
            "To safely pick a peer at random, all the snapshot hashes must be the same"
        );

        let final_peer_and_incremental_snapshot_hash = (
            final_rpc_peer.clone(),
            *final_full_snapshot_hash,
            *final_incremental_snapshot_hash,
        );
        trace!(
            "final peer and incremental snapshot hash: {:?}",
            &final_peer_and_incremental_snapshot_hash
        );
        final_peer_and_incremental_snapshot_hash
    }

    /// Check to see if we can use our local snapshots, otherwise download newer ones.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn download_snapshots(
        snapshot_archives_dir: &Path,
        validator_config: &ValidatorConfig,
        bootstrap_config: &RpcBootstrapConfig,
        use_progress_bar: bool,
        maximum_local_snapshot_age: Slot,
        start_progress: &Arc<RwLock<ValidatorStartProgress>>,
        minimal_snapshot_download_speed: f32,
        maximum_snapshot_download_abort: u64,
        download_abort_count: &mut u64,
        snapshot_hashes: Option<((Slot, Hash), Option<(Slot, Hash)>)>,
        rpc_contact_info: &ContactInfo,
    ) -> Result<(), String> {
        if snapshot_hashes.is_none() {
            return Ok(());
        }
        let (full_snapshot_hash, incremental_snapshot_hash) = snapshot_hashes.unwrap();

        // If the local snapshots are new enough, then use 'em; no need to download new snapshots
        if should_use_local_snapshot(
            snapshot_archives_dir,
            maximum_local_snapshot_age,
            full_snapshot_hash,
            incremental_snapshot_hash,
        ) {
            return Ok(());
        }

        // Check and see if we've already got the full snapshot; if not, download it
        if snapshot_utils::get_full_snapshot_archives(snapshot_archives_dir)
            .into_iter()
            .any(|snapshot_archive| {
                snapshot_archive.slot() == full_snapshot_hash.0
                    && snapshot_archive.hash() == &full_snapshot_hash.1
            })
        {
            info!(
            "Full snapshot archive already exists locally. Skipping download. slot: {}, hash: {}",
            full_snapshot_hash.0, full_snapshot_hash.1
        );
        } else {
            download_snapshot(
                snapshot_archives_dir,
                validator_config,
                bootstrap_config,
                use_progress_bar,
                start_progress,
                minimal_snapshot_download_speed,
                maximum_snapshot_download_abort,
                download_abort_count,
                rpc_contact_info,
                full_snapshot_hash,
                SnapshotType::FullSnapshot,
            )?;
        }

        // Check and see if we've already got the incremental snapshot; if not, download it
        if let Some(incremental_snapshot_hash) = incremental_snapshot_hash {
            if snapshot_utils::get_incremental_snapshot_archives(snapshot_archives_dir)
                .into_iter()
                .any(|snapshot_archive| {
                    snapshot_archive.slot() == incremental_snapshot_hash.0
                        && snapshot_archive.hash() == &incremental_snapshot_hash.1
                        && snapshot_archive.base_slot() == full_snapshot_hash.0
                })
            {
                info!(
            "Incremental snapshot archive already exists locally. Skipping download. slot: {}, hash: {}",
            incremental_snapshot_hash.0, incremental_snapshot_hash.1
        );
            } else {
                download_snapshot(
                    snapshot_archives_dir,
                    validator_config,
                    bootstrap_config,
                    use_progress_bar,
                    start_progress,
                    minimal_snapshot_download_speed,
                    maximum_snapshot_download_abort,
                    download_abort_count,
                    rpc_contact_info,
                    incremental_snapshot_hash,
                    SnapshotType::IncrementalSnapshot(full_snapshot_hash.0),
                )?;
            }
        }

        Ok(())
    }

    /// Download a snapshot
    #[allow(clippy::too_many_arguments)]
    fn download_snapshot(
        snapshot_archives_dir: &Path,
        validator_config: &ValidatorConfig,
        bootstrap_config: &RpcBootstrapConfig,
        use_progress_bar: bool,
        start_progress: &Arc<RwLock<ValidatorStartProgress>>,
        minimal_snapshot_download_speed: f32,
        maximum_snapshot_download_abort: u64,
        download_abort_count: &mut u64,
        rpc_contact_info: &ContactInfo,
        desired_snapshot_hash: (Slot, Hash),
        snapshot_type: SnapshotType,
    ) -> Result<(), String> {
        let (
            maximum_full_snapshot_archives_to_retain,
            maximum_incremental_snapshot_archives_to_retain,
        ) = if let Some(snapshot_config) = validator_config.snapshot_config.as_ref() {
            (
                snapshot_config.maximum_full_snapshot_archives_to_retain,
                snapshot_config.maximum_incremental_snapshot_archives_to_retain,
            )
        } else {
            (
                DEFAULT_MAX_FULL_SNAPSHOT_ARCHIVES_TO_RETAIN,
                DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN,
            )
        };
        *start_progress.write().unwrap() = ValidatorStartProgress::DownloadingSnapshot {
            slot: desired_snapshot_hash.0,
            rpc_addr: rpc_contact_info.rpc,
        };
        download_snapshot_archive(
            &rpc_contact_info.rpc,
            snapshot_archives_dir,
            desired_snapshot_hash,
            snapshot_type,
            maximum_full_snapshot_archives_to_retain,
            maximum_incremental_snapshot_archives_to_retain,
            use_progress_bar,
            &mut Some(Box::new(|download_progress: &DownloadProgressRecord| {
                debug!("Download progress: {:?}", download_progress);
                if download_progress.last_throughput < minimal_snapshot_download_speed
                    && download_progress.notification_count <= 1
                    && download_progress.percentage_done <= 2_f32
                    && download_progress.estimated_remaining_time > 60_f32
                    && *download_abort_count < maximum_snapshot_download_abort
                {
                    if let Some(ref trusted_validators) = validator_config.trusted_validators {
                        if trusted_validators.contains(&rpc_contact_info.id)
                            && trusted_validators.len() == 1
                            && bootstrap_config.no_untrusted_rpc
                        {
                            warn!("The snapshot download is too slow, throughput: {} < min speed {} bytes/sec, but will NOT abort \
                                      and try a different node as it is the only known validator and the --only-known-rpc flag \
                                      is set. \
                                      Abort count: {}, Progress detail: {:?}",
                                      download_progress.last_throughput, minimal_snapshot_download_speed,
                                      download_abort_count, download_progress);
                            return true; // Do not abort download from the one-and-only known validator
                        }
                    }
                    warn!("The snapshot download is too slow, throughput: {} < min speed {} bytes/sec, will abort \
                               and try a different node. Abort count: {}, Progress detail: {:?}",
                               download_progress.last_throughput, minimal_snapshot_download_speed,
                               download_abort_count, download_progress);
                    *download_abort_count += 1;
                    false
                } else {
                    true
                }
            })),
        )
    }

    /// Check to see if bootstrap should load from its local snapshots or not.  If not, then snapshots
    /// will be downloaded.
    fn should_use_local_snapshot(
        snapshot_archives_dir: &Path,
        maximum_local_snapshot_age: Slot,
        full_snapshot_hash: (Slot, Hash),
        incremental_snapshot_hash: Option<(Slot, Hash)>,
    ) -> bool {
        let cluster_snapshot_slot = incremental_snapshot_hash
            .map(|(slot, _)| slot)
            .unwrap_or(full_snapshot_hash.0);

        match get_highest_local_snapshot_hash(snapshot_archives_dir) {
            None => {
                info!(
                    "Downloading a snapshot for slot {} since there is not a local snapshot.",
                    cluster_snapshot_slot,
                );
                false
            }
            Some((local_snapshot_slot, _)) => {
                if local_snapshot_slot
                    >= cluster_snapshot_slot.saturating_sub(maximum_local_snapshot_age)
                {
                    info!(
                    "Reusing local snapshot at slot {} instead of downloading a snapshot for slot {}.",
                    local_snapshot_slot,
                    cluster_snapshot_slot,
                );
                    true
                } else {
                    info!(
                    "Local snapshot from slot {} is too old. Downloading a newer snapshot for slot {}.",
                    local_snapshot_slot,
                    cluster_snapshot_slot,
                );
                    false
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::type_complexity)]
        use super::*;

        fn default_contact_info_for_tests() -> ContactInfo {
            let sock_addr = SocketAddr::from(([1, 1, 1, 1], 11_111));
            ContactInfo {
                id: Pubkey::default(),
                gossip: sock_addr,
                tvu: sock_addr,
                tvu_forwards: sock_addr,
                repair: sock_addr,
                tpu: sock_addr,
                tpu_forwards: sock_addr,
                tpu_vote: sock_addr,
                rpc: sock_addr,
                rpc_pubsub: sock_addr,
                serve_repair: sock_addr,
                wallclock: 123456789,
                shred_version: 1,
            }
        }

        #[test]
        fn test_get_only_trusted_incremental_snapshot_hashes() {
            let trusted_incremental_snapshot_hashes: HashMap<(Slot, Hash), HashSet<(Slot, Hash)>> =
                [
                    (
                        (200_000, Hash::new_unique()),
                        [
                            (200_200, Hash::new_unique()),
                            (200_400, Hash::new_unique()),
                            (200_600, Hash::new_unique()),
                            (200_800, Hash::new_unique()),
                        ]
                        .iter()
                        .cloned()
                        .collect(),
                    ),
                    (
                        (300_000, Hash::new_unique()),
                        [
                            (300_200, Hash::new_unique()),
                            (300_400, Hash::new_unique()),
                            (300_600, Hash::new_unique()),
                        ]
                        .iter()
                        .cloned()
                        .collect(),
                    ),
                ]
                .iter()
                .cloned()
                .collect();

            let trusted_incremental_snapshot_hash_elem =
                trusted_incremental_snapshot_hashes.iter().next().unwrap();
            let trusted_full_snapshot_hash = trusted_incremental_snapshot_hash_elem.0;
            let trusted_incremental_snapshot_hash = trusted_incremental_snapshot_hash_elem
                .1
                .iter()
                .next()
                .unwrap();

            let contact_info = default_contact_info_for_tests();
            let incremental_snapshot_hashes: Vec<(
                ContactInfo,
                (Slot, Hash),
                Option<(Slot, Hash)>,
            )> = vec![
                // bad full snapshot hash, no incremental snapshot hash
                (contact_info.clone(), (111_000, Hash::default()), None),
                // bad everything
                (
                    contact_info.clone(),
                    (111_000, Hash::default()),
                    Some((111_111, Hash::default())),
                ),
                // good full snapshot hash, no incremental snapshot hash
                (contact_info.clone(), (*trusted_full_snapshot_hash), None),
                // bad full snapshot hash, good (not possible) incremental snapshot hash
                (
                    contact_info.clone(),
                    (111_000, Hash::default()),
                    Some(*trusted_incremental_snapshot_hash),
                ),
                // good full snapshot hash, bad incremental snapshot hash
                (
                    contact_info.clone(),
                    (*trusted_full_snapshot_hash),
                    Some((111_111, Hash::default())),
                ),
                // good everything
                (
                    contact_info.clone(),
                    (*trusted_full_snapshot_hash),
                    Some(*trusted_incremental_snapshot_hash),
                ),
            ];

            let expected: Vec<(ContactInfo, (Slot, Hash), Option<(Slot, Hash)>)> = vec![(
                contact_info,
                *trusted_full_snapshot_hash,
                Some(*trusted_incremental_snapshot_hash),
            )];
            let mut actual = incremental_snapshot_hashes;
            retain_trusted_incremental_snapshot_hashes(
                &trusted_incremental_snapshot_hashes,
                &mut actual,
            );
            assert_eq!(expected, actual);
        }

        #[test]
        fn test_get_incremental_snapshot_hashes_with_a_multiple_of_our_full_snapshot_interval() {
            let full_snapshot_archive_interval = 100_000;
            let contact_info = default_contact_info_for_tests();
            let incremental_snapshot_hashes: Vec<(
                ContactInfo,
                (Slot, Hash),
                Option<(Slot, Hash)>,
            )> = vec![
                // good full snapshot hash slots
                (contact_info.clone(), (100_000, Hash::default()), None),
                (contact_info.clone(), (200_000, Hash::default()), None),
                (contact_info.clone(), (300_000, Hash::default()), None),
                // bad full snapshot hash slots
                (contact_info.clone(), (999, Hash::default()), None),
                (contact_info.clone(), (100_777, Hash::default()), None),
                (contact_info.clone(), (101_000, Hash::default()), None),
                (contact_info.clone(), (999_000, Hash::default()), None),
            ];

            let expected: Vec<(ContactInfo, (Slot, Hash), Option<(Slot, Hash)>)> = vec![
                (contact_info.clone(), (100_000, Hash::default()), None),
                (contact_info.clone(), (200_000, Hash::default()), None),
                (contact_info, (300_000, Hash::default()), None),
            ];
            let mut actual = incremental_snapshot_hashes;
            retain_incremental_snapshot_hashes_with_a_multiple_of_full_snapshot_archive_interval(
                full_snapshot_archive_interval,
                &mut actual,
            );
            assert_eq!(expected, actual);
        }

        #[test]
        fn test_retain_incremental_snapshot_hashes_with_highest_full_snapshot_slot() {
            let contact_info = default_contact_info_for_tests();
            let incremental_snapshot_hashes: Vec<(
                ContactInfo,
                (Slot, Hash),
                Option<(Slot, Hash)>,
            )> = vec![
                // old
                (
                    contact_info.clone(),
                    (100_000, Hash::default()),
                    Some((100_100, Hash::default())),
                ),
                (
                    contact_info.clone(),
                    (100_000, Hash::default()),
                    Some((100_200, Hash::default())),
                ),
                (
                    contact_info.clone(),
                    (100_000, Hash::default()),
                    Some((100_300, Hash::default())),
                ),
                // new
                (
                    contact_info.clone(),
                    (200_000, Hash::default()),
                    Some((200_100, Hash::default())),
                ),
                (
                    contact_info.clone(),
                    (200_000, Hash::default()),
                    Some((200_200, Hash::default())),
                ),
                (
                    contact_info.clone(),
                    (200_000, Hash::default()),
                    Some((200_300, Hash::default())),
                ),
            ];

            let expected: Vec<(ContactInfo, (Slot, Hash), Option<(Slot, Hash)>)> = vec![
                (
                    contact_info.clone(),
                    (200_000, Hash::default()),
                    Some((200_100, Hash::default())),
                ),
                (
                    contact_info.clone(),
                    (200_000, Hash::default()),
                    Some((200_200, Hash::default())),
                ),
                (
                    contact_info,
                    (200_000, Hash::default()),
                    Some((200_300, Hash::default())),
                ),
            ];
            let mut actual = incremental_snapshot_hashes;
            retain_incremental_snapshot_hashes_with_highest_full_snapshot_slot(&mut actual);
            assert_eq!(expected, actual);
        }

        #[test]
        fn test_retain_incremental_snapshot_hashes_with_highest_incremental_snasphot_slot() {
            let contact_info = default_contact_info_for_tests();
            let incremental_snapshot_hashes: Vec<(
                ContactInfo,
                (Slot, Hash),
                Option<(Slot, Hash)>,
            )> = vec![
                (contact_info.clone(), (200_000, Hash::default()), None),
                (
                    contact_info.clone(),
                    (200_000, Hash::default()),
                    Some((200_100, Hash::default())),
                ),
                (
                    contact_info.clone(),
                    (200_000, Hash::default()),
                    Some((200_200, Hash::default())),
                ),
                (
                    contact_info.clone(),
                    (200_000, Hash::default()),
                    Some((200_300, Hash::default())),
                ),
                (
                    contact_info.clone(),
                    (200_000, Hash::default()),
                    Some((200_010, Hash::default())),
                ),
                (
                    contact_info.clone(),
                    (200_000, Hash::default()),
                    Some((200_020, Hash::default())),
                ),
                (
                    contact_info.clone(),
                    (200_000, Hash::default()),
                    Some((200_030, Hash::default())),
                ),
            ];

            let expected: Vec<(ContactInfo, (Slot, Hash), Option<(Slot, Hash)>)> = vec![(
                contact_info,
                (200_000, Hash::default()),
                Some((200_300, Hash::default())),
            )];
            let mut actual = incremental_snapshot_hashes;
            retain_incremental_snapshot_hashes_with_highest_incremental_snapshot_slot(&mut actual);
            assert_eq!(expected, actual);
        }
    }
}
