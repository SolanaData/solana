//! The `system_transaction` module provides functionality for creating system transactions.

use bincode::serialize;
use hash::Hash;
use signature::{Keypair, KeypairUtil};
use solana_program_interface::pubkey::Pubkey;
use system_program::SystemProgram;
use transaction::{Instruction, Transaction};

pub trait SystemTransaction {
    fn system_create(
        from_keypair: &Keypair,
        to: Pubkey,
        last_id: Hash,
        tokens: i64,
        space: u64,
        program_id: Pubkey,
        fee: i64,
    ) -> Self;

    fn system_assign(from_keypair: &Keypair, last_id: Hash, program_id: Pubkey, fee: i64) -> Self;

    fn system_new(from_keypair: &Keypair, to: Pubkey, tokens: i64, last_id: Hash) -> Self;

    fn system_move(
        from_keypair: &Keypair,
        to: Pubkey,
        tokens: i64,
        last_id: Hash,
        fee: i64,
    ) -> Self;

    fn system_load(
        from_keypair: &Keypair,
        last_id: Hash,
        fee: i64,
        program_id: Pubkey,
        name: String,
    ) -> Self;
    fn system_move_many(
        from_keypair: &Keypair,
        moves: &[(Pubkey, i64)],
        last_id: Hash,
        fee: i64,
    ) -> Self;
}

impl SystemTransaction for Transaction {
    /// Create and sign new SystemProgram::CreateAccount transaction
    fn system_create(
        from_keypair: &Keypair,
        to: Pubkey,
        last_id: Hash,
        tokens: i64,
        space: u64,
        program_id: Pubkey,
        fee: i64,
    ) -> Self {
        let create = SystemProgram::CreateAccount {
            tokens, //TODO, the tokens to allocate might need to be higher then 0 in the future
            space,
            program_id,
        };
        let userdata = serialize(&create).unwrap();
        Transaction::new(
            from_keypair,
            &[to],
            SystemProgram::id(),
            userdata,
            last_id,
            fee,
        )
    }
    /// Create and sign new SystemProgram::Assign transaction
    fn system_assign(from_keypair: &Keypair, last_id: Hash, program_id: Pubkey, fee: i64) -> Self {
        let assign = SystemProgram::Assign { program_id };
        let userdata = serialize(&assign).unwrap();
        Transaction::new(
            from_keypair,
            &[],
            SystemProgram::id(),
            userdata,
            last_id,
            fee,
        )
    }
    /// Create and sign new SystemProgram::CreateAccount transaction with some defaults
    fn system_new(from_keypair: &Keypair, to: Pubkey, tokens: i64, last_id: Hash) -> Self {
        Transaction::system_create(from_keypair, to, last_id, tokens, 0, Pubkey::default(), 0)
    }
    /// Create and sign new SystemProgram::Move transaction
    fn system_move(
        from_keypair: &Keypair,
        to: Pubkey,
        tokens: i64,
        last_id: Hash,
        fee: i64,
    ) -> Self {
        let move_tokens = SystemProgram::Move { tokens };
        let userdata = serialize(&move_tokens).unwrap();
        Transaction::new(
            from_keypair,
            &[to],
            SystemProgram::id(),
            userdata,
            last_id,
            fee,
        )
    }
    /// Create and sign new SystemProgram::Load transaction
    fn system_load(
        from_keypair: &Keypair,
        last_id: Hash,
        fee: i64,
        program_id: Pubkey,
        name: String,
    ) -> Self {
        let load = SystemProgram::Load { program_id, name };
        let userdata = serialize(&load).unwrap();
        Transaction::new(
            from_keypair,
            &[],
            SystemProgram::id(),
            userdata,
            last_id,
            fee,
        )
    }
    fn system_move_many(from: &Keypair, moves: &[(Pubkey, i64)], last_id: Hash, fee: i64) -> Self {
        let instructions: Vec<_> = moves
            .iter()
            .enumerate()
            .map(|(i, (_, amount))| {
                let spend = SystemProgram::Move { tokens: *amount };
                Instruction {
                    program_ids_index: 0,
                    userdata: serialize(&spend).unwrap(),
                    accounts: vec![(0, false), (i as u8 + 1, false)],
                }
            }).collect();
        let to_keys: Vec<_> = moves.iter().map(|(to_key, _)| *to_key).collect();

        Transaction::new_with_instructions(
            from,
            &to_keys,
            last_id,
            fee,
            vec![SystemProgram::id()],
            instructions,
        )
    }
}

pub fn test_tx() -> Transaction {
    let keypair1 = Keypair::new();
    let pubkey1 = keypair1.pubkey();
    let zero = Hash::default();
    Transaction::system_new(&keypair1, pubkey1, 42, zero)
}

#[cfg(test)]
pub fn memfind<A: Eq>(a: &[A], b: &[A]) -> Option<usize> {
    assert!(a.len() >= b.len());
    let end = a.len() - b.len() + 1;
    for i in 0..end {
        if a[i..i + b.len()] == b[..] {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use bincode::{deserialize, serialize};
    use packet::PACKET_DATA_SIZE;
    use transaction::{PUB_KEY_OFFSET, SIGNED_DATA_OFFSET, SIG_OFFSET};

    #[test]
    fn test_layout() {
        let tx = test_tx();
        let sign_data = tx.get_sign_data();
        let tx_bytes = serialize(&tx).unwrap();
        assert_eq!(memfind(&tx_bytes, &sign_data), Some(SIGNED_DATA_OFFSET));
        assert_eq!(memfind(&tx_bytes, &tx.signature.as_ref()), Some(SIG_OFFSET));
        assert_eq!(
            memfind(&tx_bytes, &tx.account_keys[0].as_ref()),
            Some(PUB_KEY_OFFSET)
        );
        assert!(tx.verify_signature());
    }

    #[test]
    fn test_userdata_layout() {
        let mut tx0 = test_tx();
        tx0.instructions[0].userdata = vec![1, 2, 3];
        let sign_data0a = tx0.get_sign_data();
        let tx_bytes = serialize(&tx0).unwrap();
        assert!(tx_bytes.len() < PACKET_DATA_SIZE);
        assert_eq!(memfind(&tx_bytes, &sign_data0a), Some(SIGNED_DATA_OFFSET));
        assert_eq!(
            memfind(&tx_bytes, &tx0.signature.as_ref()),
            Some(SIG_OFFSET)
        );
        assert_eq!(
            memfind(&tx_bytes, &tx0.account_keys[0].as_ref()),
            Some(PUB_KEY_OFFSET)
        );
        let tx1 = deserialize(&tx_bytes).unwrap();
        assert_eq!(tx0, tx1);
        assert_eq!(tx1.instructions[0].userdata, vec![1, 2, 3]);

        tx0.instructions[0].userdata = vec![1, 2, 4];
        let sign_data0b = tx0.get_sign_data();
        assert_ne!(sign_data0a, sign_data0b);
    }
    #[test]
    fn test_move_many() {
        let from = Keypair::new();
        let t1 = Keypair::new();
        let t2 = Keypair::new();
        let moves = vec![(t1.pubkey(), 1), (t2.pubkey(), 2)];

        let tx = Transaction::system_move_many(&from, &moves, Default::default(), 0);
        assert_eq!(tx.account_keys[0], from.pubkey());
        assert_eq!(tx.account_keys[1], t1.pubkey());
        assert_eq!(tx.account_keys[2], t2.pubkey());
        assert_eq!(tx.instructions.len(), 2);
        assert_eq!(tx.instructions[0].accounts, vec![(0, true), (1, false)]);
        assert_eq!(tx.instructions[1].accounts, vec![(0, true), (2, false)]);
    }

    #[test]
    fn test_move_attack() {
        let from = Keypair::new();
        let to = Keypair::new().pubkey();
        let mut tx = Transaction::system_move(&from, to, 1, Default::default(), 0);
        assert!(tx.verify_refs());

        tx.instructions[0].accounts[0].0 = 1; // <-- attack! Attempt to spend `to` tokens instead of `from` tokens.
        tx.sign(&from);
        assert!(tx.verify_signature());
        assert!(!tx.verify_refs());

        // Note: here's how to sneak by verify_refs(). The engine itself needs to reject debits from unsigned accounts.
        tx.instructions[0].accounts[0].1 = false;
        assert!(tx.verify_refs());
    }
}
