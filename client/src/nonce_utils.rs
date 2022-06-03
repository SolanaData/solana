use {
    crate::rpc_client::RpcClient,
    solana_sdk::{
        account::{Account, ReadableAccount},
        account_utils::StateMut,
        commitment_config::CommitmentConfig,
        nonce::{
            state::{Data, Versions},
            State,
        },
        pubkey::Pubkey,
        system_program,
    },
};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("invalid account owner")]
    InvalidAccountOwner,
    #[error("invalid account data")]
    InvalidAccountData,
    #[error("unexpected account data size")]
    UnexpectedDataSize,
    #[error("query hash does not match stored hash")]
    InvalidHash,
    #[error("query authority does not match account authority")]
    InvalidAuthority,
    #[error("invalid state for requested operation")]
    InvalidStateForOperation,
    #[error("client error: {0}")]
    Client(String),
}

pub fn get_account(rpc_client: &RpcClient, nonce_pubkey: &Pubkey) -> Result<Account, Error> {
    get_account_with_commitment(rpc_client, nonce_pubkey, CommitmentConfig::default())
}

pub fn get_account_with_commitment(
    rpc_client: &RpcClient,
    nonce_pubkey: &Pubkey,
    commitment: CommitmentConfig,
) -> Result<Account, Error> {
    rpc_client
        .get_account_with_commitment(nonce_pubkey, commitment)
        .map_err(|e| Error::Client(format!("{}", e)))
        .and_then(|result| {
            result
                .value
                .ok_or_else(|| Error::Client(format!("AccountNotFound: pubkey={}", nonce_pubkey)))
        })
        .and_then(|a| account_identity_ok(&a).map(|()| a))
}

pub fn account_identity_ok<T: ReadableAccount>(account: &T) -> Result<(), Error> {
    if account.owner() != &system_program::id() {
        Err(Error::InvalidAccountOwner)
    } else if account.data().is_empty() {
        Err(Error::UnexpectedDataSize)
    } else {
        Ok(())
    }
}

pub fn state_from_account<T: ReadableAccount + StateMut<Versions>>(
    account: &T,
) -> Result<State, Error> {
    account_identity_ok(account)?;
    StateMut::<Versions>::state(account)
        .map_err(|_| Error::InvalidAccountData)
        .map(|v| v.convert_to_current())
}

<<<<<<< HEAD
=======
/// Deserialize the state data of a durable transaction nonce account.
///
/// # Errors
///
/// Returns an error if the account is not owned by the system program or
/// contains no data. Returns an error if the account state is uninitialized or
/// fails to deserialize.
///
/// # Examples
///
/// Create and sign a transaction with a durable nonce:
///
/// ```no_run
/// use solana_client::{
///     rpc_client::RpcClient,
///     nonce_utils,
/// };
/// use solana_sdk::{
///     message::Message,
///     pubkey::Pubkey,
///     signature::{Keypair, Signer},
///     system_instruction,
///     transaction::Transaction,
/// };
/// use std::path::Path;
/// use anyhow::Result;
/// # use anyhow::anyhow;
///
/// fn create_transfer_tx_with_nonce(
///     client: &RpcClient,
///     nonce_account_pubkey: &Pubkey,
///     payer: &Keypair,
///     receiver: &Pubkey,
///     amount: u64,
///     tx_path: &Path,
/// ) -> Result<()> {
///
///     let instr_transfer = system_instruction::transfer(
///         &payer.pubkey(),
///         receiver,
///         amount,
///     );
///
///     // In this example, `payer` is `nonce_account_pubkey`'s authority
///     let instr_advance_nonce_account = system_instruction::advance_nonce_account(
///         nonce_account_pubkey,
///         &payer.pubkey(),
///     );
///
///     // The `advance_nonce_account` instruction must be the first issued in
///     // the transaction.
///     let message = Message::new(
///         &[
///             instr_advance_nonce_account,
///             instr_transfer
///         ],
///         Some(&payer.pubkey()),
///     );
///
///     let mut tx = Transaction::new_unsigned(message);
///
///     // Sign the tx with nonce_account's `blockhash` instead of the
///     // network's latest blockhash.
///     let nonce_account = client.get_account(nonce_account_pubkey)?;
///     let nonce_data = nonce_utils::data_from_account(&nonce_account)?;
///     let blockhash = nonce_data.blockhash();
///
///     tx.try_sign(&[payer], blockhash)?;
///
///     // Save the signed transaction locally for later submission.
///     save_tx_to_file(&tx_path, &tx)?;
///
///     Ok(())
/// }
/// #
/// # fn save_tx_to_file(path: &Path, tx: &Transaction) -> Result<()> {
/// #     Ok(())
/// # }
/// #
/// # let client = RpcClient::new(String::new());
/// # let nonce_account_pubkey = Pubkey::new_unique();
/// # let payer = Keypair::new();
/// # let receiver = Pubkey::new_unique();
/// # create_transfer_tx_with_nonce(&client, &nonce_account_pubkey, &payer, &receiver, 1024, Path::new("new_tx"))?;
/// #
/// # Ok::<(), anyhow::Error>(())
/// ```
>>>>>>> 5ee157f43 (separates durable nonce and blockhash domains)
pub fn data_from_account<T: ReadableAccount + StateMut<Versions>>(
    account: &T,
) -> Result<Data, Error> {
    account_identity_ok(account)?;
    state_from_account(account).and_then(|ref s| data_from_state(s).map(|d| d.clone()))
}

pub fn data_from_state(state: &State) -> Result<&Data, Error> {
    match state {
        State::Uninitialized => Err(Error::InvalidStateForOperation),
        State::Initialized(data) => Ok(data),
    }
}
