//! The `banking_stage` processes Transaction messages. It is intended to be used
//! to contruct a software pipeline. The stage uses all available CPU cores and
//! can do its processing in parallel with signature verification on the GPU.

use crate::cluster_info::ClusterInfo;
use crate::entry::Entry;
use crate::leader_confirmation_service::LeaderConfirmationService;
use crate::packet::Packets;
use crate::packet::SharedPackets;
use crate::poh_recorder::{PohRecorder, PohRecorderError, WorkingBank};
#[cfg(test)]
use crate::poh_service::{PohService, PohServiceConfig};
use crate::result::{Error, Result};
use crate::service::Service;
use crate::sigverify_stage::VerifiedPackets;
use bincode::deserialize;
use solana_metrics::counter::Counter;
use solana_runtime::bank::{self, Bank, BankError};
use solana_sdk::timing::{self, duration_as_us, MAX_RECENT_TICK_HASHES};
use solana_sdk::transaction::Transaction;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, Builder, JoinHandle};
use std::time::Duration;
use std::time::Instant;
use sys_info;

pub type UnprocessedPackets = Vec<(SharedPackets, usize)>; // `usize` is the index of the first unprocessed packet in `SharedPackets`

// number of threads is 1 until mt bank is ready
pub const NUM_THREADS: u32 = 10;

/// Stores the stage's thread handle and output receiver.
pub struct BankingStage {
    bank_thread_hdls: Vec<JoinHandle<Result<()>>>,
    exit: Arc<AtomicBool>,
    lcs_handle: JoinHandle<()>,
}

pub type TpuBankEntries = (Arc<Bank>, Vec<(Entry, u64)>);

impl BankingStage {
    /// Create the stage using `bank`. Exit when `verified_receiver` is dropped.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        cluster_info: &Arc<RwLock<ClusterInfo>>,
        bank_receiver: Receiver<Arc<Bank>>,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
        verified_receiver: Receiver<VerifiedPackets>,
    ) -> (Self, Receiver<TpuBankEntries>) {
        let (entry_sender, entry_receiver) = channel();
        let verified_receiver = Arc::new(Mutex::new(verified_receiver));

        // Single thread to generate entries from many banks.
        // This thread talks to poh_service and broadcasts the entries once they have been recorded.
        // Once an entry has been recorded, its last_id is registered with the bank.
        let exit = Arc::new(AtomicBool::new(false));

        // Single thread to compute confirmation
        let (leader_confirmation_service, lcs_handle) =
            LeaderConfirmationService::new(exit.clone());
        let (waiter, notifier) = sync_channel(Self::num_threads() as usize);
        // Many banks that process transactions in parallel.
        let mut bank_thread_hdls: Vec<JoinHandle<Result<()>>> = (0..Self::num_threads())
            .map(|_| {
                let verified_receiver = verified_receiver.clone();
                let poh_recorder = poh_recorder.clone();
                let cluster_info = cluster_info.clone();
                let waiter = waiter.clone();
                Builder::new()
                    .name("solana-banking-stage-tx".to_string())
                    .spawn(move || loop {
                        waiter.send(())?;
                        let bank = poh_recorder.lock().unwrap().bank();
                        if let Some(bank) = bank {
                            debug!("banking-stage-tx bank {}", bank.slot());
                            let packets =
                                Self::process_loop(&bank, &verified_receiver, &poh_recorder);
                            if let Some(leader) = cluster_info.read().unwrap().leader_data() {
                                Self::forward_unprocessed_packets(&leader.tpu, packets)?;
                            }
                        }
                    })
                    .unwrap()
            })
            .collect();

        let cluster_info = cluster_info.clone();
        let poh_recorder = poh_recorder.clone();
        let bank_thread_hdl: JoinHandle<Result<()>> = Builder::new()
            .name("solana-banking-working_bank".to_string())
            .spawn(move || loop {
                let bank =
                    Self::forward_until_bank(&cluster_info, &bank_receiver, &verified_receiver)?;
                let max_tick_height = (bank.slot() + 1) * bank.ticks_per_slot() - 1;
                let working_bank = WorkingBank {
                    bank: bank.clone(),
                    sender: entry_sender.clone(),
                    min_tick_height: bank.tick_height(),
                    max_tick_height,
                };

                info!(
                    "new working bank {} {} {}",
                    working_bank.min_tick_height,
                    working_bank.max_tick_height,
                    poh_recorder.lock().unwrap().poh.tick_height
                );
                leader_confirmation_service.set_bank(bank.clone());
                poh_recorder.lock().unwrap().set_working_bank(working_bank);
                for _ in 0..Self::num_threads() {
                    notifier.recv()?;
                }
            })
            .unwrap();
        bank_thread_hdls.push(bank_thread_hdl);
        (
            Self {
                bank_thread_hdls,
                exit,
                lcs_handle,
            },
            entry_receiver,
        )
    }

    fn forward_until_bank(
        cluster_info: &Arc<RwLock<ClusterInfo>>,
        bank_receiver: &Receiver<Arc<Bank>>,
        verified_receiver: &Arc<Mutex<Receiver<VerifiedPackets>>>,
    ) -> Result<Arc<Bank>> {
        loop {
            let result = bank_receiver.recv_timeout(Duration::from_millis(100));
            match result {
                Err(RecvTimeoutError::Timeout) => (),
                _ => {
                    let bank = result?;
                    debug!("got bank {}", bank.slot());
                    return Ok(bank);
                }
            }
            while let Ok(mms) = verified_receiver.lock().unwrap().try_recv() {
                if let Some(leader) = cluster_info.read().unwrap().leader_data() {
                    let _ = Self::forward_verified_packets(&leader.tpu, mms);
                }
                let result = bank_receiver.try_recv();
                if result.is_ok() {
                    let bank = result?;
                    return Ok(bank);
                }
            }
        }
    }

    fn forward_verified_packets(
        tpu: &std::net::SocketAddr,
        verified_packets: VerifiedPackets,
    ) -> std::io::Result<()> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        for (packets, sigverify) in verified_packets {
            let packets = packets.read().unwrap();
            for (packet, valid) in packets.packets.iter().zip(&sigverify) {
                if *valid == 0 {
                    continue;
                }
                socket.send_to(&packet.data[..packet.meta.size], tpu)?;
            }
        }
        Ok(())
    }

    fn forward_unprocessed_packets(
        tpu: &std::net::SocketAddr,
        unprocessed_packets: UnprocessedPackets,
    ) -> std::io::Result<()> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        for (packets, start_index) in unprocessed_packets {
            let packets = packets.read().unwrap();
            for packet in packets.packets.iter().skip(start_index) {
                socket.send_to(&packet.data[..packet.meta.size], tpu)?;
            }
        }
        Ok(())
    }

    pub fn process_loop(
        bank: &Bank,
        verified_receiver: &Arc<Mutex<Receiver<VerifiedPackets>>>,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
    ) -> UnprocessedPackets {
        let mut unprocessed_packets: UnprocessedPackets = vec![];
        loop {
            match Self::process_packets(&bank, &verified_receiver, &poh_recorder) {
                Err(Error::RecvTimeoutError(RecvTimeoutError::Timeout)) => (),
                Ok(more_unprocessed_packets) => {
                    unprocessed_packets.extend(more_unprocessed_packets);
                }
                Err(err) => {
                    debug!("solana-banking-stage-tx: exit due to {:?}", err);
                    break;
                }
            }
        }
        unprocessed_packets
    }

    pub fn num_threads() -> u32 {
        sys_info::cpu_num().unwrap_or(NUM_THREADS)
    }

    /// Convert the transactions from a blob of binary data to a vector of transactions
    fn deserialize_transactions(p: &Packets) -> Vec<Option<Transaction>> {
        p.packets
            .iter()
            .map(|x| deserialize(&x.data[0..x.meta.size]).ok())
            .collect()
    }

    fn record_transactions(
        txs: &[Transaction],
        results: &[bank::Result<()>],
        poh: &Arc<Mutex<PohRecorder>>,
    ) -> Result<()> {
        let processed_transactions: Vec<_> = results
            .iter()
            .zip(txs.iter())
            .filter_map(|(r, x)| match r {
                Ok(_) => Some(x.clone()),
                Err(BankError::ProgramError(index, err)) => {
                    info!("program error {:?}, {:?}", index, err);
                    Some(x.clone())
                }
                Err(ref e) => {
                    debug!("process transaction failed {:?}", e);
                    None
                }
            })
            .collect();
        debug!("processed: {} ", processed_transactions.len());
        // unlock all the accounts with errors which are filtered by the above `filter_map`
        if !processed_transactions.is_empty() {
            let hash = Transaction::hash(&processed_transactions);
            // record and unlock will unlock all the successfull transactions
            poh.lock().unwrap().record(hash, processed_transactions)?;
        }
        Ok(())
    }

    pub fn process_and_record_transactions(
        bank: &Bank,
        txs: &[Transaction],
        poh: &Arc<Mutex<PohRecorder>>,
    ) -> Result<()> {
        let now = Instant::now();
        // Once accounts are locked, other threads cannot encode transactions that will modify the
        // same account state
        let lock_results = bank.lock_accounts(txs);
        let lock_time = now.elapsed();

        let now = Instant::now();
        // Use a shorter maximum age when adding transactions into the pipeline.  This will reduce
        // the likelihood of any single thread getting starved and processing old ids.
        // TODO: Banking stage threads should be prioritized to complete faster then this queue
        // expires.
        let (loaded_accounts, results) =
            bank.load_and_execute_transactions(txs, lock_results, MAX_RECENT_TICK_HASHES / 2);
        let load_execute_time = now.elapsed();

        let record_time = {
            let now = Instant::now();
            Self::record_transactions(txs, &results, poh)?;
            now.elapsed()
        };

        let commit_time = {
            let now = Instant::now();
            bank.commit_transactions(txs, &loaded_accounts, &results);
            now.elapsed()
        };

        let now = Instant::now();
        // Once the accounts are new transactions can enter the pipeline to process them
        bank.unlock_accounts(&txs, &results);
        let unlock_time = now.elapsed();
        debug!(
            "lock: {}us load_execute: {}us record: {}us commit: {}us unlock: {}us txs_len: {}",
            duration_as_us(&lock_time),
            duration_as_us(&load_execute_time),
            duration_as_us(&record_time),
            duration_as_us(&commit_time),
            duration_as_us(&unlock_time),
            txs.len(),
        );
        Ok(())
    }

    /// Sends transactions to the bank.
    ///
    /// Returns the number of transactions successfully processed by the bank, which may be less
    /// than the total number if max PoH height was reached and the bank halted
    fn process_transactions(
        bank: &Bank,
        transactions: &[Transaction],
        poh: &Arc<Mutex<PohRecorder>>,
    ) -> Result<(usize)> {
        let mut chunk_start = 0;
        while chunk_start != transactions.len() {
            let chunk_end = chunk_start + Entry::num_will_fit(&transactions[chunk_start..]);

            let result = Self::process_and_record_transactions(
                bank,
                &transactions[chunk_start..chunk_end],
                poh,
            );
            trace!("process_transcations: {:?}", result);
            if let Err(Error::PohRecorderError(PohRecorderError::MaxHeightReached)) = result {
                info!("process transactions: max height reached");
                break;
            }
            result?;
            chunk_start = chunk_end;
        }
        Ok(chunk_start)
    }

    /// Process the incoming packets
    pub fn process_packets(
        bank: &Bank,
        verified_receiver: &Arc<Mutex<Receiver<VerifiedPackets>>>,
        poh: &Arc<Mutex<PohRecorder>>,
    ) -> Result<UnprocessedPackets> {
        let recv_start = Instant::now();
        let mms = verified_receiver
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_millis(100))?;
        let mut reqs_len = 0;
        let mms_len = mms.len();
        info!(
            "@{:?} process start stalled for: {:?}ms batches: {}",
            timing::timestamp(),
            timing::duration_as_ms(&recv_start.elapsed()),
            mms.len(),
        );
        inc_new_counter_info!("banking_stage-entries_received", mms_len);
        let count = mms.iter().map(|x| x.1.len()).sum();
        let proc_start = Instant::now();
        let mut new_tx_count = 0;

        let mut unprocessed_packets = vec![];
        let mut bank_shutdown = false;
        for (msgs, vers) in mms {
            if bank_shutdown {
                unprocessed_packets.push((msgs, 0));
                continue;
            }

            let transactions = Self::deserialize_transactions(&msgs.read().unwrap());
            reqs_len += transactions.len();

            debug!("transactions received {}", transactions.len());
            let (verified_transactions, verified_transaction_index): (Vec<_>, Vec<_>) =
                transactions
                    .into_iter()
                    .zip(vers)
                    .zip(0..)
                    .filter_map(|((tx, ver), index)| match tx {
                        None => None,
                        Some(tx) => {
                            if tx.verify_refs() && ver != 0 {
                                Some((tx, index))
                            } else {
                                None
                            }
                        }
                    })
                    .unzip();

            debug!("verified transactions {}", verified_transactions.len());

            let processed = Self::process_transactions(bank, &verified_transactions, poh)?;
            if processed < verified_transactions.len() {
                bank_shutdown = true;
                // Collect any unprocessed transactions in this batch for forwarding
                unprocessed_packets.push((msgs, verified_transaction_index[processed]));
            }
            new_tx_count += processed;
        }

        inc_new_counter_info!(
            "banking_stage-time_ms",
            timing::duration_as_ms(&proc_start.elapsed()) as usize
        );
        let total_time_s = timing::duration_as_s(&proc_start.elapsed());
        let total_time_ms = timing::duration_as_ms(&proc_start.elapsed());
        info!(
            "@{:?} done processing transaction batches: {} time: {:?}ms reqs: {} reqs/s: {}",
            timing::timestamp(),
            mms_len,
            total_time_ms,
            reqs_len,
            (reqs_len as f32) / (total_time_s)
        );
        inc_new_counter_info!("banking_stage-process_packets", count);
        inc_new_counter_info!("banking_stage-process_transactions", new_tx_count);

        Ok(unprocessed_packets)
    }

    pub fn exit(&self) {
        self.exit.store(true, Ordering::Relaxed);
    }
}

impl Service for BankingStage {
    type JoinReturnType = ();

    fn join(self) -> thread::Result<()> {
        for bank_thread_hdl in self.bank_thread_hdls {
            let _ = bank_thread_hdl.join()?;
        }
        self.exit.store(true, Ordering::Relaxed);
        self.lcs_handle.join()?;
        Ok(())
    }
}

#[cfg(test)]
pub fn create_test_recorder(bank: &Arc<Bank>) -> (Arc<Mutex<PohRecorder>>, PohService) {
    let exit = Arc::new(AtomicBool::new(false));
    let poh_recorder = Arc::new(Mutex::new(PohRecorder::new(
        bank.tick_height(),
        bank.last_id(),
    )));
    let poh_service = PohService::new(
        poh_recorder.clone(),
        &PohServiceConfig::default(),
        exit.clone(),
    );
    (poh_recorder, poh_service)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_info::Node;
    use crate::entry::EntrySlice;
    use crate::packet::to_packets;
    use solana_sdk::genesis_block::GenesisBlock;
    use solana_sdk::native_program::ProgramError;
    use solana_sdk::signature::{Keypair, KeypairUtil};
    use solana_sdk::system_transaction::SystemTransaction;
    use std::thread::sleep;

    #[test]
    fn test_banking_stage_shutdown1() {
        let (genesis_block, _mint_keypair) = GenesisBlock::new(2);
        let bank = Arc::new(Bank::new(&genesis_block));
        let (verified_sender, verified_receiver) = channel();
        let (bank_sender, bank_receiver) = channel();
        let (poh_recorder, poh_service) = create_test_recorder(&bank);
        let cluster_info = ClusterInfo::new(Node::new_localhost().info);
        let cluster_info = Arc::new(RwLock::new(cluster_info));
        let (banking_stage, _entry_receiver) = BankingStage::new(
            &cluster_info,
            bank_receiver,
            &poh_recorder,
            verified_receiver,
        );
        drop(verified_sender);
        drop(bank_sender);
        banking_stage.join().unwrap();
        poh_service.close().unwrap();
    }

    #[test]
    fn test_banking_stage_tick() {
        solana_logger::setup();
        let (genesis_block, _mint_keypair) = GenesisBlock::new(2);
        let bank = Arc::new(Bank::new(&genesis_block));
        let start_hash = bank.last_id();
        let (bank_sender, bank_receiver) = channel();
        let (verified_sender, verified_receiver) = channel();
        let (poh_recorder, poh_service) = create_test_recorder(&bank);
        let cluster_info = ClusterInfo::new(Node::new_localhost().info);
        let cluster_info = Arc::new(RwLock::new(cluster_info));
        let (banking_stage, entry_receiver) = BankingStage::new(
            &cluster_info,
            bank_receiver,
            &poh_recorder,
            verified_receiver,
        );
        trace!("sending bank");
        bank_sender.send(bank.clone()).expect("send");
        sleep(Duration::from_millis(500));
        drop(verified_sender);
        drop(bank_sender);

        trace!("getting entries");
        let entries: Vec<_> = entry_receiver
            .iter()
            .flat_map(|x| x.1.into_iter().map(|e| e.0))
            .collect();
        trace!("done");
        assert!(entries.len() != 0);
        assert!(entries.verify(&start_hash));
        assert_eq!(entries[entries.len() - 1].hash, bank.last_id());
        banking_stage.join().unwrap();
        poh_service.close().unwrap();
    }

    #[test]
    fn test_banking_stage_entries_only() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(2);
        let bank = Arc::new(Bank::new(&genesis_block));
        let start_hash = bank.last_id();
        let (verified_sender, verified_receiver) = channel();
        let (poh_recorder, poh_service) = create_test_recorder(&bank);
        let (bank_sender, bank_receiver) = channel();
        let cluster_info = ClusterInfo::new(Node::new_localhost().info);
        let cluster_info = Arc::new(RwLock::new(cluster_info));
        let (banking_stage, entry_receiver) = BankingStage::new(
            &cluster_info,
            bank_receiver,
            &poh_recorder,
            verified_receiver,
        );
        bank_sender.send(bank).expect("sending bank");

        // good tx
        let keypair = mint_keypair;
        let tx = SystemTransaction::new_account(&keypair, keypair.pubkey(), 1, start_hash, 0);

        // good tx, but no verify
        let tx_no_ver =
            SystemTransaction::new_account(&keypair, keypair.pubkey(), 1, start_hash, 0);

        // bad tx, AccountNotFound
        let keypair = Keypair::new();
        let tx_anf = SystemTransaction::new_account(&keypair, keypair.pubkey(), 1, start_hash, 0);

        // send 'em over
        let packets = to_packets(&[tx, tx_no_ver, tx_anf]);

        // glad they all fit
        assert_eq!(packets.len(), 1);
        verified_sender // tx, no_ver, anf
            .send(vec![(packets[0].clone(), vec![1u8, 0u8, 1u8])])
            .unwrap();

        drop(verified_sender);
        drop(bank_sender);

        //receive entries + ticks
        let entries: Vec<Vec<Entry>> = entry_receiver
            .iter()
            .map(|x| x.1.into_iter().map(|e| e.0).collect())
            .collect();

        assert!(entries.len() >= 1);

        let mut last_id = start_hash;
        entries.iter().for_each(|entries| {
            assert_eq!(entries.len(), 1);
            assert!(entries.verify(&last_id));
            last_id = entries.last().unwrap().hash;
        });
        drop(entry_receiver);
        banking_stage.join().unwrap();
        poh_service.close().unwrap();
    }

    #[test]
    fn test_banking_stage_entryfication() {
        // In this attack we'll demonstrate that a verifier can interpret the ledger
        // differently if either the server doesn't signal the ledger to add an
        // Entry OR if the verifier tries to parallelize across multiple Entries.
        let (genesis_block, mint_keypair) = GenesisBlock::new(2);
        let bank = Arc::new(Bank::new(&genesis_block));
        let (verified_sender, verified_receiver) = channel();
        let (poh_recorder, poh_service) = create_test_recorder(&bank);
        let (bank_sender, bank_receiver) = channel();
        let cluster_info = ClusterInfo::new(Node::new_localhost().info);
        let cluster_info = Arc::new(RwLock::new(cluster_info));
        let (banking_stage, entry_receiver) = BankingStage::new(
            &cluster_info,
            bank_receiver,
            &poh_recorder,
            verified_receiver,
        );
        bank_sender.send(bank).expect("sending bank");

        // Process a batch that includes a transaction that receives two tokens.
        let alice = Keypair::new();
        let tx = SystemTransaction::new_account(
            &mint_keypair,
            alice.pubkey(),
            2,
            genesis_block.hash(),
            0,
        );

        let packets = to_packets(&[tx]);
        verified_sender
            .send(vec![(packets[0].clone(), vec![1u8])])
            .unwrap();

        // Process a second batch that spends one of those tokens.
        let tx = SystemTransaction::new_account(
            &alice,
            mint_keypair.pubkey(),
            1,
            genesis_block.hash(),
            0,
        );
        let packets = to_packets(&[tx]);
        verified_sender
            .send(vec![(packets[0].clone(), vec![1u8])])
            .unwrap();
        drop(verified_sender);
        drop(bank_sender);
        banking_stage.join().unwrap();

        // Collect the ledger and feed it to a new bank.
        let entries: Vec<_> = entry_receiver
            .iter()
            .flat_map(|x| x.1.into_iter().map(|e| e.0))
            .collect();
        // same assertion as running through the bank, really...
        assert!(entries.len() >= 2);

        // Assert the user holds one token, not two. If the stage only outputs one
        // entry, then the second transaction will be rejected, because it drives
        // the account balance below zero before the credit is added.
        let bank = Bank::new(&genesis_block);
        for entry in entries {
            bank.process_transactions(&entry.transactions)
                .iter()
                .for_each(|x| assert_eq!(*x, Ok(())));
        }
        assert_eq!(bank.get_balance(&alice.pubkey()), 1);
        poh_service.close().unwrap();
    }

    // Test that when the max_tick_height is reached, the banking stage exits
    #[test]
    fn test_max_tick_height_shutdown() {
        solana_logger::setup();
        let (genesis_block, _mint_keypair) = GenesisBlock::new(2);
        let bank = Arc::new(Bank::new(&genesis_block));
        let (verified_sender, verified_receiver) = channel();
        let max_tick_height = 10;
        let (poh_recorder, poh_service) = create_test_recorder(&bank);
        let (bank_sender, bank_receiver) = channel();
        let cluster_info = ClusterInfo::new(Node::new_localhost().info);
        let cluster_info = Arc::new(RwLock::new(cluster_info));
        let (banking_stage, _entry_receiver) = BankingStage::new(
            &cluster_info,
            bank_receiver,
            &poh_recorder,
            verified_receiver,
        );
        bank_sender.send(bank.clone()).expect("sending bank");

        loop {
            let bank_tick_height = bank.tick_height();
            if bank_tick_height >= max_tick_height {
                break;
            }
            sleep(Duration::from_millis(10));
        }

        drop(verified_sender);
        drop(bank_sender);
        banking_stage.join().unwrap();
        poh_service.close().unwrap();
    }

    #[test]
    fn test_bank_record_transactions() {
        let (genesis_block, mint_keypair) = GenesisBlock::new(10_000);
        let bank = Arc::new(Bank::new(&genesis_block));
        let (entry_sender, entry_receiver) = channel();
        let working_bank = WorkingBank {
            bank: bank.clone(),
            sender: entry_sender,
            min_tick_height: bank.tick_height(),
            max_tick_height: std::u64::MAX,
        };

        let poh_recorder = Arc::new(Mutex::new(PohRecorder::new(
            bank.tick_height(),
            bank.last_id(),
        )));
        poh_recorder.lock().unwrap().set_working_bank(working_bank);
        let pubkey = Keypair::new().pubkey();

        let transactions = vec![
            SystemTransaction::new_move(&mint_keypair, pubkey, 1, genesis_block.hash(), 0),
            SystemTransaction::new_move(&mint_keypair, pubkey, 1, genesis_block.hash(), 0),
        ];

        let mut results = vec![Ok(()), Ok(())];
        BankingStage::record_transactions(&transactions, &results, &poh_recorder).unwrap();
        let (_, entries) = entry_receiver.recv().unwrap();
        assert_eq!(entries[0].0.transactions.len(), transactions.len());

        // ProgramErrors should still be recorded
        results[0] = Err(BankError::ProgramError(
            1,
            ProgramError::ResultWithNegativeTokens,
        ));
        BankingStage::record_transactions(&transactions, &results, &poh_recorder).unwrap();
        let (_, entries) = entry_receiver.recv().unwrap();
        assert_eq!(entries[0].0.transactions.len(), transactions.len());

        // Other BankErrors should not be recorded
        results[0] = Err(BankError::AccountNotFound);
        BankingStage::record_transactions(&transactions, &results, &poh_recorder).unwrap();
        let (_, entries) = entry_receiver.recv().unwrap();
        assert_eq!(entries[0].0.transactions.len(), transactions.len() - 1);
    }

    #[test]
    fn test_bank_process_and_record_transactions() {
        solana_logger::setup();
        let (genesis_block, mint_keypair) = GenesisBlock::new(10_000);
        let bank = Arc::new(Bank::new(&genesis_block));
        let pubkey = Keypair::new().pubkey();

        let transactions = vec![SystemTransaction::new_move(
            &mint_keypair,
            pubkey,
            1,
            genesis_block.hash(),
            0,
        )];

        let (entry_sender, entry_receiver) = channel();
        let working_bank = WorkingBank {
            bank: bank.clone(),
            sender: entry_sender,
            min_tick_height: bank.tick_height(),
            max_tick_height: bank.tick_height() + 1,
        };
        let poh_recorder = Arc::new(Mutex::new(PohRecorder::new(
            bank.tick_height(),
            bank.last_id(),
        )));
        poh_recorder.lock().unwrap().set_working_bank(working_bank);

        BankingStage::process_and_record_transactions(&bank, &transactions, &poh_recorder).unwrap();
        poh_recorder.lock().unwrap().tick();

        let mut need_tick = true;
        // read entries until I find mine, might be ticks...
        while let Ok((_, entries)) = entry_receiver.recv() {
            for (entry, _) in entries {
                if !entry.is_tick() {
                    trace!("got entry");
                    assert_eq!(entry.transactions.len(), transactions.len());
                    assert_eq!(bank.get_balance(&pubkey), 1);
                    need_tick = false;
                } else {
                    break;
                }
            }
        }

        assert_eq!(need_tick, false);

        let transactions = vec![SystemTransaction::new_move(
            &mint_keypair,
            pubkey,
            2,
            genesis_block.hash(),
            0,
        )];

        assert_matches!(
            BankingStage::process_and_record_transactions(&bank, &transactions, &poh_recorder,),
            Err(Error::PohRecorderError(PohRecorderError::MaxHeightReached))
        );

        assert_eq!(bank.get_balance(&pubkey), 1);
    }
}
