pub mod batched_range_proof;
pub mod ciphertext_ciphertext_equality;
pub mod ciphertext_commitment_equality;
pub mod pubkey_validity;
pub mod range_proof;
pub mod transfer;
pub mod withdraw;
pub mod zero_balance;

#[cfg(not(target_os = "solana"))]
use crate::errors::ProofError;
use num_derive::{FromPrimitive, ToPrimitive};
pub use {
    batched_range_proof::{
        batched_range_proof_u128::BatchedRangeProofU128Data,
        batched_range_proof_u256::BatchedRangeProofU256Data,
        batched_range_proof_u64::BatchedRangeProofU64Data, BatchedRangeProofContext,
    },
    bytemuck::Pod,
    ciphertext_ciphertext_equality::{
        CiphertextCiphertextEqualityProofContext, CiphertextCiphertextEqualityProofData,
    },
    ciphertext_commitment_equality::{
        CiphertextCommitmentEqualityProofContext, CiphertextCommitmentEqualityProofData,
    },
    pubkey_validity::{PubkeyValidityData, PubkeyValidityProofContext},
    range_proof::{RangeProofContext, RangeProofU64Data},
    transfer::{
        FeeParameters, TransferData, TransferProofContext, TransferWithFeeData,
        TransferWithFeeProofContext,
    },
    withdraw::{WithdrawData, WithdrawProofContext},
    zero_balance::{ZeroBalanceProofContext, ZeroBalanceProofData},
};

#[derive(Clone, Copy, Debug, FromPrimitive, ToPrimitive, PartialEq, Eq)]
#[repr(u8)]
pub enum ProofType {
    /// Empty proof type used to distinguish if a proof context account is initialized
    Uninitialized,
    ZeroBalance,
    Withdraw,
    CiphertextCiphertextEquality,
    Transfer,
    TransferWithFee,
    PubkeyValidity,
    RangeProofU64,
    BatchedRangeProofU64,
    BatchedRangeProofU128,
    BatchedRangeProofU256,
    CiphertextCommitmentEquality,
}

pub trait ZkProofData<T: Pod> {
    const PROOF_TYPE: ProofType;

    fn context_data(&self) -> &T;

    #[cfg(not(target_os = "solana"))]
    fn verify_proof(&self) -> Result<(), ProofError>;
}
