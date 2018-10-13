//! The `poh_recorder` module provides an object for synchronizing with Proof of History.
//! It synchronizes PoH, bank's register_entry_id and the ledger
//!
use bank::Bank;
use entry::Entry;
use hash::Hash;
use poh::Poh;
use result::Result;
use solana_program_interface::pubkey::Pubkey;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use transaction::Transaction;

#[derive(Clone)]
pub struct PohRecorder {
    poh: Arc<Mutex<Poh>>,
    bank: Arc<Bank>,
    sender: Sender<Vec<Entry>>,
    my_id: Pubkey,
}

impl PohRecorder {
    /// A recorder to synchronize PoH with the following data structures
    /// * bank - the LastId's queue is updated on `tick` and `record` events
    /// * sender - the Entry channel that outputs to the ledger
    pub fn new(
        bank: Arc<Bank>,
        sender: Sender<Vec<Entry>>,
        last_entry_id: Hash,
        my_id: Pubkey,
    ) -> Self {
        let poh = Arc::new(Mutex::new(Poh::new(last_entry_id)));
        PohRecorder {
            poh,
            bank,
            sender,
            my_id,
        }
    }

    pub fn hash(&self) {
        // TODO: amortize the cost of this lock by doing the loop in here for
        // some min amount of hashes
        let mut poh = self.poh.lock().unwrap();
        poh.hash()
    }

    pub fn tick(&self) -> Result<()> {
        // Register and send the entry out while holding the lock.
        // This guarantees PoH order and Entry production and banks LastId queue is the same
        let mut poh = self.poh.lock().unwrap();
        let tick = poh.tick();
        self.bank.register_entry_id(&tick.id);
        let entry = Entry {
            num_hashes: tick.num_hashes,
            id: tick.id,
            transactions: vec![],
            leader_id: self.my_id,
        };
        self.sender.send(vec![entry])?;
        Ok(())
    }

    pub fn record(&self, mixin: Hash, txs: Vec<Transaction>) -> Result<()> {
        // Register and send the entry out while holding the lock.
        // This guarantees PoH order and Entry production and banks LastId queue is the same.
        let mut poh = self.poh.lock().unwrap();
        let tick = poh.record(mixin);
        assert!(!txs.is_empty(), "Entries without transactions are used to track real-time passing in the ledger and can only be generated with PohRecorder::tick function");
        let entry = Entry {
            num_hashes: tick.num_hashes,
            id: tick.id,
            transactions: txs,
            leader_id: self.my_id,
        };
        self.sender.send(vec![entry])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hash::hash;
    use mint::Mint;
    use solana_program_interface::pubkey::Pubkey;
    use std::sync::mpsc::channel;
    use std::sync::Arc;
    use system_transaction::test_tx;

    #[test]
    fn test_poh() {
        let mint = Mint::new(1);
        let bank = Arc::new(Bank::new(&mint));
        let last_id = bank.last_id();
        let (entry_sender, entry_receiver) = channel();
        let poh_recorder = PohRecorder::new(bank, entry_sender, last_id, Pubkey::default());

        //send some data
        let h1 = hash(b"hello world!");
        let tx = test_tx();
        assert!(poh_recorder.record(h1, vec![tx]).is_ok());
        assert!(poh_recorder.tick().is_ok());

        //get some events
        let _ = entry_receiver.recv().unwrap();
        let _ = entry_receiver.recv().unwrap();

        //make sure it handles channel close correctly
        drop(entry_receiver);
        assert!(poh_recorder.tick().is_err());
    }
}
