use {
    super::{broadcast_utils::ReceiveResults, *},
    crate::cluster_nodes::ClusterNodesCache,
    solana_ledger::{
        entry::{create_ticks, Entry, EntrySlice},
        shred::Shredder,
    },
    solana_runtime::blockhash_queue::BlockhashQueue,
    solana_sdk::{
        fee_calculator::FeeCalculator,
        hash::Hash,
        signature::{Keypair, Signer},
        transaction::Transaction,
    },
    std::collections::VecDeque,
};

// Queue which facilitates delivering shreds with a delay
type DelayedQueue = VecDeque<(Option<Pubkey>, Option<Vec<Shred>>)>;

#[derive(Clone)]
pub(super) struct BroadcastDuplicatesRun {
    config: BroadcastDuplicatesConfig,
    // Local queue for broadcast to track which duplicate blockhashes we've sent
    duplicate_queue: BlockhashQueue,
    // Shared queue between broadcast and transmit threads
    delayed_queue: Arc<Mutex<DelayedQueue>>,
    // Buffer for duplicate entries
    duplicate_entries_buffer: Vec<Entry>,
    last_duplicate_entry_hash: Hash,
    last_broadcast_slot: Slot,
    next_shred_index: u32,
    next_code_index: u32,
    shred_version: u16,
    keypair: Arc<Keypair>,
    cluster_nodes_cache: Arc<ClusterNodesCache<BroadcastStage>>,
}

impl BroadcastDuplicatesRun {
    pub(super) fn new(
        keypair: Arc<Keypair>,
        shred_version: u16,
        config: BroadcastDuplicatesConfig,
    ) -> Self {
        let cluster_nodes_cache = Arc::new(ClusterNodesCache::<BroadcastStage>::new(
            CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
            CLUSTER_NODES_CACHE_TTL,
        ));
        let mut delayed_queue = DelayedQueue::new();
        delayed_queue.resize(config.duplicate_send_delay, (None, None));
        Self {
            config,
            delayed_queue: Arc::new(Mutex::new(delayed_queue)),
            duplicate_queue: BlockhashQueue::default(),
            duplicate_entries_buffer: vec![],
            next_shred_index: u32::MAX,
<<<<<<< HEAD
            last_broadcast_slot: 0,
            last_duplicate_entry_hash: Hash::default(),
=======
            next_code_index: 0,
>>>>>>> 65d59f4ef (tracks erasure coding shreds' indices explicitly (#21822))
            shred_version,
            keypair,
            cluster_nodes_cache,
        }
    }

    fn queue_or_create_duplicate_entries(
        &mut self,
        bank: &Arc<Bank>,
        receive_results: &ReceiveResults,
    ) -> (Vec<Entry>, u32) {
        // If the last entry hash is default, grab the last blockhash from the parent bank
        if self.last_duplicate_entry_hash == Hash::default() {
            self.last_duplicate_entry_hash = bank.last_blockhash();
        }

        // Create duplicate entries by..
        //  1) rearranging real entries so that all transaction entries are moved to
        //     the front and tick entries are moved to the back.
        //  2) setting all transaction entries to zero hashes and all tick entries to `hashes_per_tick`.
        //  3) removing any transactions which reference blockhashes which aren't in the
        //     duplicate blockhash queue.
        let (duplicate_entries, next_shred_index) = if bank.slot() > MINIMUM_DUPLICATE_SLOT {
            let mut tx_entries: Vec<Entry> = receive_results
                .entries
                .iter()
                .filter_map(|entry| {
                    if entry.is_tick() {
                        return None;
                    }

                    let transactions: Vec<Transaction> = entry
                        .transactions
                        .iter()
                        .filter(|tx| {
                            self.duplicate_queue
                                .get_hash_age(&tx.message.recent_blockhash)
                                .is_some()
                        })
                        .cloned()
                        .collect();
                    if !transactions.is_empty() {
                        Some(Entry::new_mut(
                            &mut self.last_duplicate_entry_hash,
                            &mut 0,
                            transactions,
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            let mut tick_entries = create_ticks(
                receive_results.entries.tick_count(),
                bank.hashes_per_tick().unwrap_or_default(),
                self.last_duplicate_entry_hash,
            );
            self.duplicate_entries_buffer.append(&mut tx_entries);
            self.duplicate_entries_buffer.append(&mut tick_entries);

            // Only send out duplicate entries when the block is finished otherwise the
            // recipient will start repairing for shreds they haven't received yet and
            // hit duplicate slot issues before we want them to.
            let entries = if receive_results.last_tick_height == bank.max_tick_height() {
                self.duplicate_entries_buffer.drain(..).collect()
            } else {
                vec![]
            };

            // Set next shred index to 0 since we are sending the full slot
            (entries, 0)
        } else {
            // Send real entries until we hit min duplicate slot
            (receive_results.entries.clone(), self.next_shred_index)
        };

        // Save last duplicate entry hash to avoid invalid entry hash errors
        if let Some(last_duplicate_entry) = duplicate_entries.last() {
            self.last_duplicate_entry_hash = last_duplicate_entry.hash;
        }

        (duplicate_entries, next_shred_index)
    }
}

/// Duplicate slots should only be sent once all validators have started.
/// This constant is intended to be used as a buffer so that all validators
/// are live before sending duplicate slots.
pub const MINIMUM_DUPLICATE_SLOT: Slot = 20;

impl BroadcastRun for BroadcastDuplicatesRun {
    fn run(
        &mut self,
        blockstore: &Arc<Blockstore>,
        receiver: &Receiver<WorkingBankEntry>,
        socket_sender: &Sender<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>,
        blockstore_sender: &Sender<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>,
    ) -> Result<()> {
        // 1) Pull entries from banking stage
        let receive_results = broadcast_utils::recv_slot_entries(receiver)?;
        let bank = receive_results.bank.clone();
        let last_tick_height = receive_results.last_tick_height;

<<<<<<< HEAD
        if self.next_shred_index == u32::MAX {
            self.next_shred_index = blockstore
                .meta(bank.slot())
                .expect("Database error")
                .map(|meta| meta.consumed)
                .unwrap_or(0) as u32
=======
        if bank.slot() != self.current_slot {
            self.next_shred_index = 0;
            self.next_code_index = 0;
            self.current_slot = bank.slot();
            self.prev_entry_hash = None;
            self.num_slots_broadcasted += 1;
>>>>>>> 65d59f4ef (tracks erasure coding shreds' indices explicitly (#21822))
        }

        // We were not the leader, but just became leader again
        if bank.slot() > self.last_broadcast_slot + 1 {
            self.last_duplicate_entry_hash = Hash::default();
        }
        self.last_broadcast_slot = bank.slot();

        let shredder = Shredder::new(
            bank.slot(),
            bank.parent().unwrap().slot(),
            self.keypair.clone(),
            (bank.tick_height() % bank.ticks_per_slot()) as u8,
            self.shred_version,
        )
        .expect("Expected to create a new shredder");

        let (data_shreds, coding_shreds) = shredder.entries_to_shreds(
<<<<<<< HEAD
=======
            keypair,
>>>>>>> 65d59f4ef (tracks erasure coding shreds' indices explicitly (#21822))
            &receive_results.entries,
            last_tick_height == bank.max_tick_height(),
            self.next_shred_index,
            self.next_code_index,
        );

        self.next_shred_index += data_shreds.len() as u32;
<<<<<<< HEAD
        let (duplicate_entries, next_duplicate_shred_index) =
            self.queue_or_create_duplicate_entries(&bank, &receive_results);
        let (duplicate_data_shreds, duplicate_coding_shreds) = if !duplicate_entries.is_empty() {
            shredder.entries_to_shreds(
                &duplicate_entries,
                last_tick_height == bank.max_tick_height(),
                next_duplicate_shred_index,
            )
        } else {
            (vec![], vec![])
        };

        // Manually track the shred index because relying on slot meta consumed is racy
        if last_tick_height == bank.max_tick_height() {
            self.next_shred_index = 0;
            self.duplicate_queue
                .register_hash(&self.last_duplicate_entry_hash, &FeeCalculator::default());
        }
=======
        if let Some(index) = coding_shreds.iter().map(Shred::index).max() {
            self.next_code_index = index + 1;
        }
        let last_shreds = last_entries.map(|(original_last_entry, duplicate_extra_last_entries)| {
            let (original_last_data_shred, _) =
                shredder.entries_to_shreds(keypair, &[original_last_entry], true, self.next_shred_index, self.next_code_index);

            let (partition_last_data_shred, _) =
                // Don't mark the last shred as last so that validators won't know that
                // they've gotten all the shreds, and will continue trying to repair
                shredder.entries_to_shreds(keypair, &duplicate_extra_last_entries, true, self.next_shred_index, self.next_code_index);
>>>>>>> 65d59f4ef (tracks erasure coding shreds' indices explicitly (#21822))

        // Partition network with duplicate and real shreds based on stake
        let bank_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let mut duplicate_recipients = HashMap::new();
        let mut real_recipients = HashMap::new();

        let mut stakes: Vec<(Pubkey, u64)> = bank
            .epoch_staked_nodes(bank_epoch)
            .unwrap()
            .iter()
            .filter(|(pubkey, _)| **pubkey != self.keypair.pubkey())
            .map(|(pubkey, stake)| (*pubkey, *stake))
            .collect();
        stakes.sort_by(|(l_key, l_stake), (r_key, r_stake)| {
            if r_stake == l_stake {
                l_key.cmp(r_key)
            } else {
                r_stake.cmp(l_stake)
            }
        });

        let highest_staked_node = stakes.first().cloned().map(|x| x.0);
        let stake_total: u64 = stakes.iter().map(|(_, stake)| *stake).sum();
        let mut cumulative_stake: u64 = 0;
        for (pubkey, stake) in stakes.into_iter().rev() {
            cumulative_stake += stake;
            if (100 * cumulative_stake / stake_total) as u8 <= self.config.stake_partition {
                duplicate_recipients.insert(pubkey, stake);
            } else {
                real_recipients.insert(pubkey, stake);
            }
        }

        if let Some(highest_staked_node) = highest_staked_node {
            if bank.slot() > MINIMUM_DUPLICATE_SLOT && last_tick_height == bank.max_tick_height() {
                warn!(
                    "{} sent duplicate slot {} to nodes: {:?}",
                    self.keypair.pubkey(),
                    bank.slot(),
                    &duplicate_recipients,
                );
                warn!(
                    "Duplicate shreds for slot {} will be broadcast in {} slot(s)",
                    bank.slot(),
                    self.config.duplicate_send_delay
                );

                let delayed_shreds: Option<Vec<Shred>> = vec![
                    duplicate_data_shreds.last().cloned(),
                    data_shreds.last().cloned(),
                ]
                .into_iter()
                .collect();
                self.delayed_queue
                    .lock()
                    .unwrap()
                    .push_back((Some(highest_staked_node), delayed_shreds));
            }
        }

        let _duplicate_recipients = Arc::new(duplicate_recipients);
        let _real_recipients = Arc::new(real_recipients);

        let data_shreds = Arc::new(data_shreds);
        blockstore_sender.send((data_shreds.clone(), None))?;

        // 3) Start broadcast step
        socket_sender.send((Arc::new(duplicate_data_shreds), None))?;
        socket_sender.send((Arc::new(duplicate_coding_shreds), None))?;
        socket_sender.send((data_shreds, None))?;
        socket_sender.send((Arc::new(coding_shreds), None))?;

        Ok(())
    }
    fn transmit(
        &mut self,
        receiver: &Arc<Mutex<TransmitReceiver>>,
        cluster_info: &ClusterInfo,
        sock: &UdpSocket,
        bank_forks: &Arc<RwLock<BankForks>>,
    ) -> Result<()> {
        // Check the delay queue for shreds that are ready to be sent
        let (delayed_recipient, delayed_shreds) = {
            let mut delayed_deque = self.delayed_queue.lock().unwrap();
            if delayed_deque.len() > self.config.duplicate_send_delay {
                delayed_deque.pop_front().unwrap()
            } else {
                (None, None)
            }
        };

        let (shreds, _) = receiver.lock().unwrap().recv()?;
        if shreds.is_empty() {
            return Ok(());
        }
        let slot = shreds.first().unwrap().slot();
        assert!(shreds.iter().all(|shred| shred.slot() == slot));
        let root_bank = bank_forks.read().unwrap().root_bank();
        let epoch = root_bank.get_leader_schedule_epoch(slot);
        let stakes = root_bank.epoch_staked_nodes(epoch).unwrap_or_default();
        let socket_addr_space = cluster_info.socket_addr_space();
        for peer in cluster_info.tvu_peers() {
            // Forward shreds to circumvent gossip
            if stakes.get(&peer.id).is_some() {
                shreds.iter().for_each(|shred| {
                    if socket_addr_space.check(&peer.tvu_forwards) {
                        sock.send_to(&shred.payload, &peer.tvu_forwards).unwrap();
                    }
                });
            }

            // After a delay, broadcast duplicate shreds to a single node
            if let Some(shreds) = delayed_shreds.as_ref() {
                if Some(peer.id) == delayed_recipient {
                    shreds.iter().for_each(|shred| {
                        if socket_addr_space.check(&peer.tvu) {
                            sock.send_to(&shred.payload, &peer.tvu).unwrap();
                        }
                    });
                }
            }
        }

        Ok(())
    }
    fn record(
        &mut self,
        receiver: &Arc<Mutex<RecordReceiver>>,
        blockstore: &Arc<Blockstore>,
    ) -> Result<()> {
        let (data_shreds, _) = receiver.lock().unwrap().recv()?;
        blockstore.insert_shreds(data_shreds.to_vec(), None, true)?;
        Ok(())
    }
}
