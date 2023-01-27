use {
    crate::{
        duplicate_shred::{into_shreds, DuplicateShred, Error},
        duplicate_shred_listener::DuplicateShredHandlerTrait,
    },
    log::*,
    solana_ledger::{blockstore::Blockstore, leader_schedule_cache::LeaderScheduleCache},
    solana_runtime::bank_forks::BankForks,
    solana_sdk::{clock::Slot, pubkey::Pubkey},
    std::{
        cmp::Reverse,
        collections::{BinaryHeap, HashMap, HashSet},
        sync::{Arc, RwLock},
    },
};

const CLEANUP_EVERY_N_LOOPS: usize = 10;
// Normally num_chunks is 3, because there are two shreds (each is one packet)
// and meta data. So we discard anything larger than 3 chunks.
const MAX_NUM_CHUNKS: u8 = 3;
// We only allow each pubkey to send proofs for 5 slots, because normally there
// is only 1 person sending out duplicate proofs, 1 person is leader for 4 slots,
// so we allow 5 here to limit the chunk map size.
const ALLOWED_SLOTS_PER_PUBKEY: usize = 5;
// We limit the pubkey for each slot to be 100 for now, when this limit is reached,
// we drop 10% of pubkeys with lowest stakes.
const MAX_PUBKEY_PER_SLOT: usize = 100;

struct ProofChunkMap {
    num_chunks: u8,
    wallclock: u64,
    chunks: HashMap<u8, DuplicateShred>,
}

impl ProofChunkMap {
    fn new(num_chunks: u8, wallclock: u64) -> Self {
        Self {
            num_chunks,
            chunks: HashMap::new(),
            wallclock,
        }
    }
}

// Group received chunks by peer pubkey, when we receive an invalid proof,
// set the value to Frozen so we don't accept future proofs with the same key.
type SlotChunkMap = HashMap<Pubkey, ProofChunkMap>;

enum SlotStatus {
    // When a valid proof has been inserted, we change the entry for that slot to Frozen
    // to indicate we no longer accept proofs for this slot.
    Frozen,
    UnfinishedProof(SlotChunkMap),
}
pub struct DuplicateShredHandler {
    // Because we use UDP for packet transfer, we can normally only send ~1500 bytes
    // in each packet. We send both shreds and meta data in duplicate shred proof, and
    // each shred is normally 1 packet(1500 bytes), so the whole proof is larger than
    // 1 packet and it needs to be cut down as chunks for transfer. So we need to piece
    // together the chunks into the original proof before anything useful is done.
    chunk_map: HashMap<Slot, SlotStatus>,
    // We don't want bad guys to inflate the chunk map, so we limit the number of
    // pending proofs from each pubkey to ALLOWED_SLOTS_PER_PUBKEY.
    validator_pending_proof_map: HashMap<Pubkey, HashSet<Slot>>,
    // Cache last root to reduce read lock.
    last_root: Slot,
    // remember the last root slot cleaned, clear anything between last_root and last_root_cleaned.
    last_root_cleaned: Slot,
    blockstore: Arc<Blockstore>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    bank_forks: Arc<RwLock<BankForks>>,
    // Cache information from root bank so we could function correctly without reading roots.
    cached_on_slot: Slot,
    cached_staked_nodes: Option<Arc<HashMap<Pubkey, u64>>>,
    cached_slots_in_epoch: u64,
    // Because cleanup could potentially be very expensive, only clean up when clean up
    // count is 0
    cleanup_count: usize,
}

impl DuplicateShredHandlerTrait for DuplicateShredHandler {
    // Here we are sending data one by one rather than in a batch because in the future
    // we may send different type of CrdsData to different senders.
    fn handle(&mut self, shred_data: DuplicateShred) {
        self.cache_root_info();
        if let Err(error) = self.handle_shred_data(shred_data) {
            error!("handle packet: {:?}", error)
        }
        if self.cleanup_count.saturating_sub(1) == 0 {
            self.cleanup_old_slots();
            self.cleanup_count = CLEANUP_EVERY_N_LOOPS;
        }
    }
}

impl DuplicateShredHandler {
    pub fn new(
        blockstore: Arc<Blockstore>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        bank_forks: Arc<RwLock<BankForks>>,
    ) -> Self {
        Self {
            chunk_map: HashMap::new(),
            validator_pending_proof_map: HashMap::new(),
            last_root: 0,
            last_root_cleaned: 0,
            cached_on_slot: 0,
            cached_staked_nodes: None,
            cached_slots_in_epoch: 100,
            blockstore,
            leader_schedule_cache,
            bank_forks,
            cleanup_count: CLEANUP_EVERY_N_LOOPS,
        }
    }

    fn cache_root_info(&mut self) {
        let last_root = self.blockstore.last_root();
        if self.cached_staked_nodes.is_none() || last_root != self.last_root {
            self.last_root = last_root;
            // The stuff we need only changes per epoch, so we only need to cache
            // once per epoch, and it's okay if we use old data in a few slots.
            if self.cached_staked_nodes.is_some()
                && last_root < self.cached_slots_in_epoch + self.cached_on_slot
            {
                return;
            }
            let root_bank = self.bank_forks.read().unwrap().root_bank();
            self.cached_on_slot = root_bank.slot();
            let epoch_info = root_bank.get_epoch_info();
            self.cached_staked_nodes = root_bank.epoch_staked_nodes(epoch_info.epoch);
            self.cached_slots_in_epoch = epoch_info.slots_in_epoch;
        }
    }

    fn handle_shred_data(&mut self, data: DuplicateShred) -> Result<(), Error> {
        if self.should_insert_chunk(&data) {
            let slot = data.slot;
            match self.insert_chunk(data) {
                Err(error) => return Err(error),
                Ok(Some(chunks)) => {
                    self.verify_and_apply_proof(slot, chunks)?;
                    // We stored the duplicate proof in this slot, no need to accept any future proof.
                    self.mark_slot_proof_received(slot);
                }
                _ => (),
            }
        }
        Ok(())
    }

    fn should_insert_chunk(&mut self, data: &DuplicateShred) -> bool {
        let slot = data.slot;
        // Do not insert if this slot is rooted or too far away in the future or has a proof already.
        if slot <= self.last_root
            || slot > self.last_root + self.cached_slots_in_epoch
            || self.blockstore.has_duplicate_shreds_in_slot(slot)
        {
            return false;
        }
        // Discard all proofs with abnormal num_chunks.
        if data.num_chunks() == 0 || data.num_chunks() > MAX_NUM_CHUNKS {
            return false;
        }
        // Only allow limited unfinished proofs per pubkey to reject attackers.
        if let Some(current_slots_set) = self.validator_pending_proof_map.get(&data.from) {
            if !current_slots_set.contains(&slot)
                && current_slots_set.len() >= ALLOWED_SLOTS_PER_PUBKEY
            {
                return false;
            }
        }
        // Also skip frozen slots or slots with a different proof than me.
        match self.chunk_map.get(&slot) {
            Some(SlotStatus::Frozen) => false,
            Some(SlotStatus::UnfinishedProof(slot_map)) => match slot_map.get(&data.from) {
                None => self.check_has_enough_stake_and_cleanup(&slot, &data.from),
                Some(proof_chunkmap) => proof_chunkmap.wallclock == data.wallclock,
            },
            None => true,
        }
    }

    fn check_has_enough_stake_and_cleanup(&mut self, slot: &Slot, newkey: &Pubkey) -> bool {
        match self.chunk_map.get_mut(slot) {
            None => true,
            Some(SlotStatus::Frozen) => false,
            Some(SlotStatus::UnfinishedProof(chunk_map)) => {
                if chunk_map.len() < MAX_PUBKEY_PER_SLOT {
                    return true;
                }
                match &mut self.cached_staked_nodes {
                    Some(cached_staked_nodes) => {
                        let newkey_stake =
                            cached_staked_nodes.get(newkey).copied().unwrap_or_default();
                        // Ignore new pubkey without any stake.
                        if newkey_stake == 0u64 {
                            return false;
                        }
                        let mut heap = BinaryHeap::new();
                        // Push new pubkey in first so it's compared with those in the table.
                        heap.push(Reverse::<(u64, Pubkey)>((newkey_stake, *newkey)));
                        for oldkey in chunk_map.keys() {
                            let oldkey_stake =
                                cached_staked_nodes.get(oldkey).copied().unwrap_or_default();
                            heap.push(Reverse::<(u64, Pubkey)>((oldkey_stake, *oldkey)));
                        }
                        let mut newkey_popped = false;
                        for _ in 0..(MAX_PUBKEY_PER_SLOT / 10) {
                            if let Some(Reverse::<(u64, Pubkey)>((_, key))) = heap.pop() {
                                if &key == newkey {
                                    newkey_popped = true;
                                } else {
                                    chunk_map.remove(&key);
                                }
                            }
                        }
                        // If new key is popped off heap, then we should not insert this key because its
                        // stake is too low, return false in this case.
                        !newkey_popped
                    }
                    None => false,
                }
            }
        }
    }

    fn mark_slot_proof_received(&mut self, slot: u64) {
        self.chunk_map.insert(slot, SlotStatus::Frozen);
        for (_, current_slots_set) in self.validator_pending_proof_map.iter_mut() {
            current_slots_set.remove(&slot);
        }
    }

    fn insert_chunk(&mut self, data: DuplicateShred) -> Result<Option<Vec<DuplicateShred>>, Error> {
        if let SlotStatus::UnfinishedProof(slot_chunk_map) = self
            .chunk_map
            .entry(data.slot)
            .or_insert_with(|| SlotStatus::UnfinishedProof(HashMap::new()))
        {
            let proof_chunk_map = slot_chunk_map
                .entry(data.from)
                .or_insert_with(|| ProofChunkMap::new(data.num_chunks(), data.wallclock));

            if proof_chunk_map.wallclock > data.wallclock {
                proof_chunk_map.num_chunks = data.num_chunks();
                proof_chunk_map.wallclock = data.wallclock;
                proof_chunk_map.chunks.clear();
            }
            let num_chunks = data.num_chunks();
            let chunk_index = data.chunk_index();
            let slot = data.slot;
            let from = data.from;
            if num_chunks == proof_chunk_map.num_chunks
                && chunk_index < num_chunks
                && !proof_chunk_map.chunks.contains_key(&chunk_index)
            {
                proof_chunk_map.chunks.insert(chunk_index, data);
                if proof_chunk_map.chunks.len() >= proof_chunk_map.num_chunks.into() {
                    let mut result: Vec<DuplicateShred> = Vec::new();
                    for i in 0..num_chunks {
                        result.push(proof_chunk_map.chunks.remove(&i).unwrap())
                    }
                    return Ok(Some(result));
                }
            }
            self.validator_pending_proof_map
                .entry(from)
                .or_default()
                .insert(slot);
        }
        Ok(None)
    }

    fn verify_and_apply_proof(&self, slot: Slot, chunks: Vec<DuplicateShred>) -> Result<(), Error> {
        if slot <= self.last_root || self.blockstore.has_duplicate_shreds_in_slot(slot) {
            return Ok(());
        }
        let (shred1, shred2) = into_shreds(chunks, |slot| {
            self.leader_schedule_cache.slot_leader_at(slot, None)
        })?;
        self.blockstore
            .store_duplicate_slot(slot, shred1.into_payload(), shred2.into_payload())?;
        Ok(())
    }

    fn cleanup_old_slots(&mut self) {
        if self.last_root_cleaned != self.last_root {
            self.chunk_map.retain(|k, _| k > &self.last_root);
            for (_, slots_sets) in self.validator_pending_proof_map.iter_mut() {
                slots_sets.retain(|k| k > &self.last_root);
            }
            self.last_root_cleaned = self.last_root;
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            cluster_info::DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
            duplicate_shred::{from_shred, tests::new_rand_shred, DuplicateShred, Error},
            duplicate_shred_listener::DuplicateShredHandlerTrait,
        },
        solana_ledger::{
            genesis_utils::{create_genesis_config_with_leader, GenesisConfigInfo},
            get_tmp_ledger_path_auto_delete,
            shred::Shredder,
        },
        solana_runtime::{bank::Bank, bank_forks::BankForks},
        solana_sdk::{
            signature::{Keypair, Signer},
            timing::timestamp,
        },
        std::sync::Arc,
    };

    fn create_duplicate_proof(
        keypair: Arc<Keypair>,
        sender_pubkey: Option<Pubkey>,
        slot: u64,
        expected_error: Option<Error>,
        chunk_size: usize,
    ) -> Result<Box<dyn Iterator<Item = DuplicateShred>>, Error> {
        let my_keypair = match expected_error {
            Some(Error::InvalidSignature) => Arc::new(Keypair::new()),
            _ => keypair,
        };
        let mut rng = rand::thread_rng();
        let shredder = Shredder::new(slot, slot - 1, 0, 0).unwrap();
        let next_shred_index = 353;
        let shred1 = new_rand_shred(&mut rng, next_shred_index, &shredder, &my_keypair);
        let shredder1 = Shredder::new(slot + 1, slot, 0, 0).unwrap();
        let shred2 = match expected_error {
            Some(Error::SlotMismatch) => {
                new_rand_shred(&mut rng, next_shred_index, &shredder1, &my_keypair)
            }
            Some(Error::ShredIndexMismatch) => {
                new_rand_shred(&mut rng, next_shred_index + 1, &shredder, &my_keypair)
            }
            Some(Error::InvalidDuplicateShreds) => shred1.clone(),
            _ => new_rand_shred(&mut rng, next_shred_index, &shredder, &my_keypair),
        };
        let sender = match sender_pubkey {
            Some(pubkey) => pubkey,
            None => my_keypair.pubkey(),
        };
        let chunks = from_shred(
            shred1,
            sender,
            shred2.payload().clone(),
            None::<fn(Slot) -> Option<Pubkey>>,
            timestamp(), // wallclock
            chunk_size,  // max_size
        )?;
        Ok(Box::new(chunks))
    }

    #[test]
    fn test_handle_mixed_entries() {
        solana_logger::setup();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let my_keypair = Arc::new(Keypair::new());
        let my_pubkey = my_keypair.pubkey();
        let genesis_config_info = create_genesis_config_with_leader(10_000, &my_pubkey, 10_000);
        let GenesisConfigInfo { genesis_config, .. } = genesis_config_info;
        let bank_forks = BankForks::new(Bank::new_for_tests(&genesis_config));
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(
            &bank_forks.working_bank(),
        ));
        let mut duplicate_shred_handler = DuplicateShredHandler::new(
            blockstore.clone(),
            leader_schedule_cache,
            Arc::new(RwLock::new(bank_forks)),
        );
        let chunks = create_duplicate_proof(
            my_keypair.clone(),
            None,
            1,
            None,
            DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
        )
        .unwrap();
        let chunks1 = create_duplicate_proof(
            my_keypair.clone(),
            None,
            2,
            None,
            DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
        )
        .unwrap();
        assert!(!blockstore.has_duplicate_shreds_in_slot(1));
        assert!(!blockstore.has_duplicate_shreds_in_slot(2));
        // Test that two proofs are mixed together, but we can store the proofs fine.
        for (chunk1, chunk2) in chunks.zip(chunks1) {
            duplicate_shred_handler.handle(chunk1);
            duplicate_shred_handler.handle(chunk2);
        }
        assert!(blockstore.has_duplicate_shreds_in_slot(1));
        assert!(blockstore.has_duplicate_shreds_in_slot(2));

        // Test all kinds of bad proofs.
        for error in [
            Error::InvalidSignature,
            Error::SlotMismatch,
            Error::ShredIndexMismatch,
            Error::InvalidDuplicateShreds,
        ] {
            match create_duplicate_proof(
                my_keypair.clone(),
                None,
                3,
                Some(error),
                DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
            ) {
                Err(_) => (),
                Ok(chunks) => {
                    for chunk in chunks {
                        duplicate_shred_handler.handle(chunk);
                    }
                    assert!(!blockstore.has_duplicate_shreds_in_slot(3));
                }
            }
        }
    }

    #[test]
    fn test_reject_abuses() {
        solana_logger::setup();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let my_keypair = Arc::new(Keypair::new());
        let my_pubkey = my_keypair.pubkey();
        let genesis_config_info = create_genesis_config_with_leader(10_000, &my_pubkey, 10_000);
        let GenesisConfigInfo { genesis_config, .. } = genesis_config_info;
        let bank_forks = BankForks::new(Bank::new_for_tests(&genesis_config));
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(
            &bank_forks.working_bank(),
        ));
        let bank_forks_ptr = Arc::new(RwLock::new(bank_forks));
        let mut duplicate_shred_handler =
            DuplicateShredHandler::new(blockstore.clone(), leader_schedule_cache, bank_forks_ptr);
        let mut start_slot: Slot = 1;

        // This proof will not be accepted because num_chunks is too large.
        let chunks = create_duplicate_proof(
            my_keypair.clone(),
            None,
            start_slot,
            None,
            DUPLICATE_SHRED_MAX_PAYLOAD_SIZE / 10,
        )
        .unwrap();
        for chunk in chunks {
            duplicate_shred_handler.handle(chunk);
        }
        assert!(!blockstore.has_duplicate_shreds_in_slot(start_slot));

        // This proof will be rejected because the slot is too far away in the future.
        let future_slot =
            blockstore.last_root() + duplicate_shred_handler.cached_slots_in_epoch + start_slot;
        let chunks = create_duplicate_proof(
            my_keypair.clone(),
            None,
            future_slot,
            None,
            DUPLICATE_SHRED_MAX_PAYLOAD_SIZE / 10,
        )
        .unwrap();
        for chunk in chunks {
            duplicate_shred_handler.handle(chunk);
        }
        assert!(!blockstore.has_duplicate_shreds_in_slot(future_slot));

        // Send in two proofs, the first proof showing up will be accepted, the following
        // proofs will be discarded.
        let mut chunks = create_duplicate_proof(
            my_keypair.clone(),
            None,
            start_slot,
            None,
            DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
        )
        .unwrap();
        // handle chunk 0 of the first proof.
        duplicate_shred_handler.handle(chunks.next().unwrap());
        let chunks1 = create_duplicate_proof(
            my_keypair.clone(),
            None,
            start_slot,
            None,
            DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
        )
        .unwrap();
        for chunk in chunks1 {
            duplicate_shred_handler.handle(chunk);
        }
        // The second proof will be discarded.
        assert!(!blockstore.has_duplicate_shreds_in_slot(start_slot));
        // Now send in the rest of the first proof, it will succeed.
        for chunk in chunks {
            duplicate_shred_handler.handle(chunk);
        }
        assert!(blockstore.has_duplicate_shreds_in_slot(start_slot));

        start_slot = 2;
        let mut all_chunks = vec![];
        for i in 0..ALLOWED_SLOTS_PER_PUBKEY + 1 {
            all_chunks.push(
                create_duplicate_proof(
                    my_keypair.clone(),
                    None,
                    start_slot + i as u64,
                    None,
                    DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
                )
                .unwrap(),
            )
        }
        let mut done_count = 0;
        let len = all_chunks.len();
        while done_count < len {
            done_count = 0;
            for chunk_iterator in &mut all_chunks {
                match chunk_iterator.next() {
                    Some(new_chunk) => duplicate_shred_handler.handle(new_chunk),
                    _ => done_count += 1,
                }
            }
        }
        for i in 0..ALLOWED_SLOTS_PER_PUBKEY {
            assert!(blockstore.has_duplicate_shreds_in_slot(start_slot + i as u64));
        }
        // The last proof should fail because we only allow limited entries per pubkey.
        assert!(
            !blockstore.has_duplicate_shreds_in_slot(start_slot + ALLOWED_SLOTS_PER_PUBKEY as u64)
        );

        start_slot += ALLOWED_SLOTS_PER_PUBKEY as u64 + 1;
        // Now send in MAX_PUBKEY_PER_SLOT number of incomplete proofs.
        for _ in 0..MAX_PUBKEY_PER_SLOT as u64 {
            let mut chunks = create_duplicate_proof(
                my_keypair.clone(),
                Some(Keypair::new().pubkey()),
                start_slot,
                None,
                DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
            )
            .unwrap();
            if let Some(chunk) = chunks.next() {
                duplicate_shred_handler.handle(chunk);
            }
        }
        // All proofs are incomplete, slot is not marked duplicate
        assert!(!blockstore.has_duplicate_shreds_in_slot(start_slot));
        let chunks = create_duplicate_proof(
            my_keypair.clone(),
            Some(Keypair::new().pubkey()),
            start_slot,
            None,
            DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
        )
        .unwrap();
        // Because the slot chunk map is full and the new pubkey has no stake, this proof
        // is not inserted into slot chunk map, so it cannot mark the slot duplicate.
        for chunk in chunks {
            duplicate_shred_handler.handle(chunk);
        }
        assert!(!blockstore.has_duplicate_shreds_in_slot(start_slot));
        // Now put in a proof where sender has stake.
        let chunks = create_duplicate_proof(
            my_keypair,
            None,
            start_slot,
            None,
            DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
        )
        .unwrap();
        for chunk in chunks {
            duplicate_shred_handler.handle(chunk);
        }
        // Because the sender has stake, it will replace one of the pubkeys with incomplete
        // proof, so we can mark the slot duplicate.
        assert!(blockstore.has_duplicate_shreds_in_slot(start_slot));
    }
}
