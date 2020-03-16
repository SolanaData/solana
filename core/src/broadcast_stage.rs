//! A stage to broadcast data from a leader node to validators
use self::{
    broadcast_fake_shreds_run::BroadcastFakeShredsRun,
    fail_entry_verification_broadcast_run::FailEntryVerificationBroadcastRun,
    standard_broadcast_run::StandardBroadcastRun,
};
use crate::{
    cluster_info::{ClusterInfo, ClusterInfoError},
    poh_recorder::WorkingBankEntry,
    replay_stage::MAX_UNCONFIRMED_SLOTS,
    result::{Error, Result},
};
use crossbeam_channel::{
    unbounded, Receiver as CrossbeamReceiver, RecvTimeoutError as CrossbeamRecvTimeoutError,
    Sender as CrossbeamSender,
};
use slot_transmit_shreds_cache::*;
use solana_ledger::{blockstore::Blockstore, shred::Shred, staking_utils};
use solana_metrics::{inc_new_counter_error, inc_new_counter_info};
use solana_runtime::bank::Bank;
use solana_sdk::clock::Slot;
use std::{
    collections::{HashMap, HashSet},
    net::UdpSocket,
    sync::atomic::{AtomicBool, Ordering},
    sync::mpsc::{channel, Receiver, RecvError, RecvTimeoutError, Sender},
    sync::{Arc, Mutex, RwLock},
    thread::{self, Builder, JoinHandle},
    time::{Duration, Instant},
};

mod broadcast_fake_shreds_run;
pub(crate) mod broadcast_utils;
mod fail_entry_verification_broadcast_run;
mod slot_transmit_shreds_cache;
mod standard_broadcast_run;

pub const NUM_INSERT_THREADS: usize = 2;
pub type RetransmitCacheSender = CrossbeamSender<(Slot, TransmitShreds)>;
pub type RetransmitCacheReceiver = CrossbeamReceiver<(Slot, TransmitShreds)>;
pub type RetransmitSlotsSender = CrossbeamSender<HashMap<Slot, Arc<Bank>>>;
pub type RetransmitSlotsReceiver = CrossbeamReceiver<HashMap<Slot, Arc<Bank>>>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum BroadcastStageReturnType {
    ChannelDisconnected,
}

#[derive(PartialEq, Clone, Debug)]
pub enum BroadcastStageType {
    Standard,
    FailEntryVerification,
    BroadcastFakeShreds,
}

impl BroadcastStageType {
    pub fn new_broadcast_stage(
        &self,
        sock: Vec<UdpSocket>,
        cluster_info: Arc<RwLock<ClusterInfo>>,
        receiver: Receiver<WorkingBankEntry>,
        retransmit_slots_receiver: RetransmitSlotsReceiver,
        exit_sender: &Arc<AtomicBool>,
        blockstore: &Arc<Blockstore>,
        shred_version: u16,
    ) -> BroadcastStage {
        let keypair = cluster_info.read().unwrap().keypair.clone();
        match self {
            BroadcastStageType::Standard => BroadcastStage::new(
                sock,
                cluster_info,
                receiver,
                retransmit_slots_receiver,
                exit_sender,
                blockstore,
                StandardBroadcastRun::new(keypair, shred_version),
            ),

            BroadcastStageType::FailEntryVerification => BroadcastStage::new(
                sock,
                cluster_info,
                receiver,
                retransmit_slots_receiver,
                exit_sender,
                blockstore,
                FailEntryVerificationBroadcastRun::new(keypair, shred_version),
            ),

            BroadcastStageType::BroadcastFakeShreds => BroadcastStage::new(
                sock,
                cluster_info,
                receiver,
                retransmit_slots_receiver,
                exit_sender,
                blockstore,
                BroadcastFakeShredsRun::new(keypair, 0, shred_version),
            ),
        }
    }
}

trait BroadcastRun {
    fn run(
        &mut self,
        blockstore: &Arc<Blockstore>,
        receiver: &Receiver<WorkingBankEntry>,
        socket_sender: &Sender<TransmitShreds>,
        blockstore_sender: &Sender<Arc<Vec<Shred>>>,
        retransmit_cache_sender: &RetransmitCacheSender,
    ) -> Result<()>;
    fn transmit(
        &self,
        receiver: &Arc<Mutex<Receiver<TransmitShreds>>>,
        cluster_info: &Arc<RwLock<ClusterInfo>>,
        sock: &UdpSocket,
    ) -> Result<()>;
    fn record(
        &self,
        receiver: &Arc<Mutex<Receiver<Arc<Vec<Shred>>>>>,
        blockstore: &Arc<Blockstore>,
    ) -> Result<()>;
}

// Implement a destructor for the BroadcastStage thread to signal it exited
// even on panics
struct Finalizer {
    exit_sender: Arc<AtomicBool>,
}

impl Finalizer {
    fn new(exit_sender: Arc<AtomicBool>) -> Self {
        Finalizer { exit_sender }
    }
}
// Implement a destructor for Finalizer.
impl Drop for Finalizer {
    fn drop(&mut self) {
        self.exit_sender.clone().store(true, Ordering::Relaxed);
    }
}

pub struct BroadcastStage {
    thread_hdls: Vec<JoinHandle<BroadcastStageReturnType>>,
}

impl BroadcastStage {
    #[allow(clippy::too_many_arguments)]
    fn run(
        blockstore: &Arc<Blockstore>,
        receiver: &Receiver<WorkingBankEntry>,
        socket_sender: &Sender<TransmitShreds>,
        blockstore_sender: &Sender<Arc<Vec<Shred>>>,
        retransmit_cache_sender: &RetransmitCacheSender,
        mut broadcast_stage_run: impl BroadcastRun,
    ) -> BroadcastStageReturnType {
        loop {
            let res = broadcast_stage_run.run(
                blockstore,
                receiver,
                socket_sender,
                blockstore_sender,
                retransmit_cache_sender,
            );
            let res = Self::handle_error(res, "run");
            if let Some(res) = res {
                return res;
            }
        }
    }
    fn handle_error(r: Result<()>, name: &str) -> Option<BroadcastStageReturnType> {
        if let Err(e) = r {
            match e {
                Error::RecvTimeoutError(RecvTimeoutError::Disconnected)
                | Error::SendError
                | Error::RecvError(RecvError)
                | Error::CrossbeamRecvTimeoutError(CrossbeamRecvTimeoutError::Disconnected) => {
                    return Some(BroadcastStageReturnType::ChannelDisconnected);
                }
                Error::RecvTimeoutError(RecvTimeoutError::Timeout)
                | Error::CrossbeamRecvTimeoutError(CrossbeamRecvTimeoutError::Timeout) => (),
                Error::ClusterInfoError(ClusterInfoError::NoPeers) => (), // TODO: Why are the unit-tests throwing hundreds of these?
                _ => {
                    inc_new_counter_error!("streamer-broadcaster-error", 1, 1);
                    error!("{} broadcaster error: {:?}", name, e);
                }
            }
        }
        None
    }

    /// Service to broadcast messages from the leader to layer 1 nodes.
    /// See `cluster_info` for network layer definitions.
    /// # Arguments
    /// * `sock` - Socket to send from.
    /// * `exit` - Boolean to signal system exit.
    /// * `cluster_info` - ClusterInfo structure
    /// * `window` - Cache of Shreds that we have broadcast
    /// * `receiver` - Receive channel for Shreds to be retransmitted to all the layer 1 nodes.
    /// * `exit_sender` - Set to true when this service exits, allows rest of Tpu to exit cleanly.
    /// Otherwise, when a Tpu closes, it only closes the stages that come after it. The stages
    /// that come before could be blocked on a receive, and never notice that they need to
    /// exit. Now, if any stage of the Tpu closes, it will lead to closing the WriteStage (b/c
    /// WriteStage is the last stage in the pipeline), which will then close Broadcast service,
    /// which will then close FetchStage in the Tpu, and then the rest of the Tpu,
    /// completing the cycle.
    #[allow(clippy::too_many_arguments)]
    fn new(
        socks: Vec<UdpSocket>,
        cluster_info: Arc<RwLock<ClusterInfo>>,
        receiver: Receiver<WorkingBankEntry>,
        retransmit_slots_receiver: RetransmitSlotsReceiver,
        exit_sender: &Arc<AtomicBool>,
        blockstore: &Arc<Blockstore>,
        broadcast_stage_run: impl BroadcastRun + Send + 'static + Clone,
    ) -> Self {
        let btree = blockstore.clone();
        let exit = exit_sender.clone();
        let (socket_sender, socket_receiver) = channel();
        let (blockstore_sender, blockstore_receiver) = channel();
        let (retransmit_cache_sender, retransmit_cache_receiver) = unbounded();
        let bs_run = broadcast_stage_run.clone();

        let socket_sender_ = socket_sender.clone();
        let thread_hdl = Builder::new()
            .name("solana-broadcaster".to_string())
            .spawn(move || {
                let _finalizer = Finalizer::new(exit);
                Self::run(
                    &btree,
                    &receiver,
                    &socket_sender_,
                    &blockstore_sender,
                    &retransmit_cache_sender,
                    bs_run,
                )
            })
            .unwrap();
        let mut thread_hdls = vec![thread_hdl];
        let socket_receiver = Arc::new(Mutex::new(socket_receiver));
        for sock in socks.into_iter() {
            let socket_receiver = socket_receiver.clone();
            let bs_transmit = broadcast_stage_run.clone();
            let cluster_info = cluster_info.clone();
            let t = Builder::new()
                .name("solana-broadcaster-transmit".to_string())
                .spawn(move || loop {
                    let res = bs_transmit.transmit(&socket_receiver, &cluster_info, &sock);
                    let res = Self::handle_error(res, "solana-broadcaster-transmit");
                    if let Some(res) = res {
                        return res;
                    }
                })
                .unwrap();
            thread_hdls.push(t);
        }
        let blockstore_receiver = Arc::new(Mutex::new(blockstore_receiver));
        for _ in 0..NUM_INSERT_THREADS {
            let blockstore_receiver = blockstore_receiver.clone();
            let bs_record = broadcast_stage_run.clone();
            let btree = blockstore.clone();
            let t = Builder::new()
                .name("solana-broadcaster-record".to_string())
                .spawn(move || loop {
                    let res = bs_record.record(&blockstore_receiver, &btree);
                    let res = Self::handle_error(res, "solana-broadcaster-record");
                    if let Some(res) = res {
                        return res;
                    }
                })
                .unwrap();
            thread_hdls.push(t);
        }

        let blockstore = blockstore.clone();
        let retransmit_thread = Builder::new()
            .name("solana-broadcaster-retransmit".to_string())
            .spawn(move || loop {
                // Cache of most recently transmitted MAX_UNCONFIRMED_SLOTS number of
                // leader blocks
                let mut transmit_shreds_cache = SlotTransmitShredsCache::new(MAX_UNCONFIRMED_SLOTS);
                // `unfinished_retransmit_slots_cache` is the set of blocks
                // we got a retransmit signal for from ReplayStage, but didn't 
                // have all the shreds in blockstore to retransmit, due to 
                // arbitrary latency between the broadcast thread and the thread
                // writing to blockstore.
                let mut unfinished_retransmit_slots_cache =
                    SlotTransmitShredsCache::new(MAX_UNCONFIRMED_SLOTS);
                let mut updates = HashSet::new();

                // Update the cache with the newest shreds
                let res = Self::handle_error(
                    transmit_shreds_cache
                        .update_retransmit_cache(&retransmit_cache_receiver, &mut updates),
                    "solana-broadcaster-retransmit-update_retransmit_cache",
                );
                if let Some(res) = res {
                    return res;
                }

                // Retry any unfinished retransmits
                let res = Self::handle_error(
                    Self::retry_unfinished_retransmit_slots(
                        &blockstore,
                        updates,
                        &mut transmit_shreds_cache,
                        &mut unfinished_retransmit_slots_cache,
                        &socket_sender,
                    ),
                    "solana-broadcaster-retransmit-retry_unfinished_retransmit_slots",
                );
                if let Some(res) = res {
                    return res;
                }

                // Check for new retransmit signals from ReplayStage
                let res = Self::handle_error(
                    Self::check_retransmit_signals(
                        &mut transmit_shreds_cache,
                        &mut unfinished_retransmit_slots_cache,
                        &blockstore,
                        &retransmit_slots_receiver,
                        &socket_sender,
                    ),
                    "solana-broadcaster-retransmit-check_retransmit_signals",
                );
                if let Some(res) = res {
                    return res;
                }
            })
            .unwrap();

        thread_hdls.push(retransmit_thread);
        Self { thread_hdls }
    }

    pub fn retry_unfinished_retransmit_slots(
        blockstore: &Blockstore,
        updates: HashMap<Slot, TransmitShreds>,
        transmit_shreds_cache: &mut SlotTransmitShredsCache,
        unfinished_retransmit_slots_cache: &mut SlotTransmitShredsCache,
        socket_sender: &Sender<TransmitShreds>,
    ) {
        for (updated_slot, transmit_shreds) in updates {
            if transmit_shreds.is_empty() {
                continue;
            }
            unfinished_retransmit_slots_cache
                .get(updated_slot)
                .map(|cached_entry| {
                    // Add any new updates to the cache. Note that writes to blockstore
                    // are done atomically by the insertion thread in batches of
                    // exactly the 'transmit_shreds` read here, so it's sufficent to
                    // check if the first index in each batch of `transmit_shreds`
                    // is greater than the last index in the current cache. If so,
                    // this implies we are missing the entire batch of updates in
                    // `transmit_shreds`, and should send them to be retransmitted.
                    let first_new_shred_index = transmit_shreds.1[0].index();
                    if transmit_shreds.1[0].is_data()
                        && first_new_shred_index
                            >= cached_entry
                                .last_data_shred()
                                .map(|shred| shred.index())
                                .unwrap_or(0)
                    {
                        // Update the cache so we don't fetch these updates again
                        unfinished_retransmit_slots_cache.push(updated_slot, transmit_shreds);
                        // Send the new updates
                        socket_sender.send(transmit_shreds);
                    } else if transmit_shreds.1[0].is_code()
                        &&first_new_shred_index
                            >= cached_entry
                                .last_coding_shred()
                                .map(|shred| shred.index())
                                .unwrap_or(0)
                    {
                        // Update the cache so we don't fetch these updates again
                        unfinished_retransmit_slots_cache.push(updated_slot, transmit_shreds);
                        // Send the new updates
                        socket_sender.send(transmit_shreds);
                    }
                });
        }

        // Fetch potential updates from blockstore, necessary for slots that
        // ReplayStage sent a retransmit signal for after that slot was already
        // removed from the `transmit_shreds_cache` (so no updates coming from broadcast thread),
        // but before updates had been written to blockstore
        let updates = unfinished_retransmit_slots_cache.update_cache_from_blockstore(blockstore);
        for (slot, cached_updates) in updates {
            // If we got all the shreds, remove this slot's entries
            // from `unfinished_retransmit_slots_cache`, as we now have all
            // the shreds needed for retransmit
            if cached_updates.contains_last_shreds() {
                unfinished_retransmit_slots_cache.remove_slot(slot);
            }
            let all_transmit_shreds = cached_updates.to_transmit_shreds();

            for transmit_shreds in all_transmit_shreds {
                socket_sender.send(transmit_shreds)?;
            }
        }
    }

    pub fn check_retransmit_signals(
        transmit_shreds_cache: &mut SlotTransmitShredsCache,
        unfinished_retransmit_slots_cache: &mut SlotTransmitShredsCache,
        blockstore: &Blockstore,
        retransmit_slots_receiver: &RetransmitSlotsReceiver,
        socket_sender: &Sender<TransmitShreds>,
    ) -> Result<()> {
        let timer = Duration::from_millis(100);

        // Check for a retransmit signal
        let mut retransmit_slots = retransmit_slots_receiver.recv_timeout(timer)?;
        while let Ok(new_retransmit_slots) = retransmit_slots_receiver.try_recv() {
            retransmit_slots.extend(new_retransmit_slots);
        }

        for (_, bank) in retransmit_slots.iter() {
            let cached_shreds = transmit_shreds_cache.get_or_update(bank, blockstore);
            let all_transmit_shreds = cached_shreds.to_transmit_shreds();
            for transmit_shreds in all_transmit_shreds {
                // If the cached shreds are misssing any shreds (broadcast
                // hasn't written them to blockstore yet), addd this slot
                // to the `unfinished_retransmit_slots_cache` so we can retry broadcasting
                // the missing shreds later.
                if !cached_shreds.contains_last_shreds() {
                    unfinished_retransmit_slots_cache.push(bank.slot(), transmit_shreds.clone());
                }
                socket_sender.send(transmit_shreds)?;
            }
        }

        Ok(())
    }

    pub fn join(self) -> thread::Result<BroadcastStageReturnType> {
        for thread_hdl in self.thread_hdls.into_iter() {
            let _ = thread_hdl.join();
        }
        Ok(BroadcastStageReturnType::ChannelDisconnected)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        cluster_info::{ClusterInfo, Node},
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
    };
    use solana_ledger::{
        entry::create_ticks,
        {blockstore::Blockstore, get_tmp_ledger_path},
    };
    use solana_runtime::bank::Bank;
    use solana_sdk::{
        hash::Hash,
        pubkey::Pubkey,
        signature::{Keypair, Signer},
    };
    use std::{
        path::Path,
        sync::atomic::AtomicBool,
        sync::mpsc::channel,
        sync::{Arc, RwLock},
        thread::sleep,
    };

    struct MockBroadcastStage {
        blockstore: Arc<Blockstore>,
        broadcast_service: BroadcastStage,
        bank: Arc<Bank>,
    }

    fn setup_dummy_broadcast_service(
        leader_pubkey: &Pubkey,
        ledger_path: &Path,
        entry_receiver: Receiver<WorkingBankEntry>,
        retransmit_slots_receiver: RetransmitSlotsReceiver,
    ) -> MockBroadcastStage {
        // Make the database ledger
        let blockstore = Arc::new(Blockstore::open(ledger_path).unwrap());

        // Make the leader node and scheduler
        let leader_info = Node::new_localhost_with_pubkey(leader_pubkey);

        // Make a node to broadcast to
        let buddy_keypair = Keypair::new();
        let broadcast_buddy = Node::new_localhost_with_pubkey(&buddy_keypair.pubkey());

        // Fill the cluster_info with the buddy's info
        let mut cluster_info = ClusterInfo::new_with_invalid_keypair(leader_info.info.clone());
        cluster_info.insert_info(broadcast_buddy.info);
        let cluster_info = Arc::new(RwLock::new(cluster_info));

        let exit_sender = Arc::new(AtomicBool::new(false));

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Arc::new(Bank::new(&genesis_config));

        let leader_keypair = cluster_info.read().unwrap().keypair.clone();
        // Start up the broadcast stage
        let broadcast_service = BroadcastStage::new(
            leader_info.sockets.broadcast,
            cluster_info,
            entry_receiver,
            retransmit_slots_receiver,
            &exit_sender,
            &blockstore,
            StandardBroadcastRun::new(leader_keypair, 0),
        );

        MockBroadcastStage {
            blockstore,
            broadcast_service,
            bank,
        }
    }

    #[test]
    fn test_broadcast_ledger() {
        solana_logger::setup();
        let ledger_path = get_tmp_ledger_path!();

        {
            // Create the leader scheduler
            let leader_keypair = Keypair::new();

            let (entry_sender, entry_receiver) = channel();
            let (retransmit_slots_sender, retransmit_slots_receiver) = unbounded();
            let broadcast_service = setup_dummy_broadcast_service(
                &leader_keypair.pubkey(),
                &ledger_path,
                entry_receiver,
                retransmit_slots_receiver,
            );
            let start_tick_height;
            let max_tick_height;
            let ticks_per_slot;
            let slot;
            println!("here");
            {
                let bank = broadcast_service.bank.clone();
                start_tick_height = bank.tick_height();
                max_tick_height = bank.max_tick_height();
                ticks_per_slot = bank.ticks_per_slot();
                slot = bank.slot();
                let ticks = create_ticks(max_tick_height - start_tick_height, 0, Hash::default());
                for (i, tick) in ticks.into_iter().enumerate() {
                    entry_sender
                        .send((bank.clone(), (tick, i as u64 + 1)))
                        .expect("Expect successful send to broadcast service");
                }
            }

            println!("sleeping");
            sleep(Duration::from_millis(2000));

            trace!(
                "[broadcast_ledger] max_tick_height: {}, start_tick_height: {}, ticks_per_slot: {}",
                max_tick_height,
                start_tick_height,
                ticks_per_slot,
            );

            let blockstore = broadcast_service.blockstore;
            let (entries, _, _) = blockstore
                .get_slot_entries_with_shred_info(slot, 0)
                .expect("Expect entries to be present");
            assert_eq!(entries.len(), max_tick_height as usize);

            drop(entry_sender);
            println!("joining");
            broadcast_service
                .broadcast_service
                .join()
                .expect("Expect successful join of broadcast service");
        }

        Blockstore::destroy(&ledger_path).expect("Expected successful database destruction");
    }
}
