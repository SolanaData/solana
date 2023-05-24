//! The 64-bit aggregate range proof instruction.

#[cfg(not(target_os = "solana"))]
use {
    crate::{
        encryption::pedersen::{PedersenCommitment, PedersenOpening},
        errors::ProofError,
        range_proof::RangeProof,
    },
    std::convert::TryInto,
};
use {
    crate::{
        instruction::{aggregate_range_proof::AggregateRangeProofContext, ProofType, ZkProofData},
        zk_token_elgamal::pod,
    },
    bytemuck::{Pod, Zeroable},
};

#[cfg(not(target_os = "solana"))]
const RANGE_PROOF_256_AGGREGATE_BIT_LENGTH: usize = 256;

/// The instruction data that is needed for the
/// `ProofInstruction::VerifyAggregateRangeProof256` instruction.
///
/// It includes the cryptographic proof as well as the context data information needed to verify
/// the proof.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct AggregateRangeProof256Data {
    /// The context data for an aggregated range proof
    pub context: AggregateRangeProofContext,

    /// The aggregated range proof
    pub proof: pod::RangeProof256,
}

#[cfg(not(target_os = "solana"))]
impl AggregateRangeProof256Data {
    pub fn new(
        commitments: Vec<&PedersenCommitment>,
        amounts: Vec<u64>,
        bit_lengths: Vec<usize>,
        openings: Vec<&PedersenOpening>,
    ) -> Result<Self, ProofError> {
        // the sum of the bit lengths must be 64
        let aggregate_bit_length = bit_lengths
            .iter()
            .try_fold(0_usize, |acc, &x| acc.checked_add(x))
            .ok_or(ProofError::Generation)?;
        if aggregate_bit_length != RANGE_PROOF_256_AGGREGATE_BIT_LENGTH {
            return Err(ProofError::Generation);
        }

        let context =
            AggregateRangeProofContext::new(&commitments, &amounts, &bit_lengths, &openings)?;

        let mut transcript = context.new_transcript();
        let proof = RangeProof::new(amounts, bit_lengths, openings, &mut transcript).try_into()?;

        Ok(Self { context, proof })
    }
}

impl ZkProofData<AggregateRangeProofContext> for AggregateRangeProof256Data {
    const PROOF_TYPE: ProofType = ProofType::AggregateRangeProof64;

    fn context_data(&self) -> &AggregateRangeProofContext {
        &self.context
    }

    #[cfg(not(target_os = "solana"))]
    fn verify_proof(&self) -> Result<(), ProofError> {
        let (commitments, bit_lengths) = self.context.try_into()?;
        let mut transcript = self.context_data().new_transcript();
        let proof: RangeProof = self.proof.try_into()?;

        proof
            .verify(commitments.iter().collect(), bit_lengths, &mut transcript)
            .map_err(|e| e.into())
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::{
            encryption::pedersen::Pedersen,
            errors::{ProofType, ProofVerificationError},
        },
    };

    #[test]
    fn test_aggregate_range_proof_256_instruction_correctness() {
        let amount_1 = 4294967295_u64;
        let amount_2 = 77_u64;
        let amount_3 = 99_u64;
        let amount_4 = 99_u64;
        let amount_5 = 11_u64;
        let amount_6 = 33_u64;
        let amount_7 = 99_u64;
        let amount_8 = 99_u64;

        let (commitment_1, opening_1) = Pedersen::new(amount_1);
        let (commitment_2, opening_2) = Pedersen::new(amount_2);
        let (commitment_3, opening_3) = Pedersen::new(amount_3);
        let (commitment_4, opening_4) = Pedersen::new(amount_4);
        let (commitment_5, opening_5) = Pedersen::new(amount_5);
        let (commitment_6, opening_6) = Pedersen::new(amount_6);
        let (commitment_7, opening_7) = Pedersen::new(amount_7);
        let (commitment_8, opening_8) = Pedersen::new(amount_8);

        let proof_data = AggregateRangeProof256Data::new(
            vec![
                &commitment_1,
                &commitment_2,
                &commitment_3,
                &commitment_4,
                &commitment_5,
                &commitment_6,
                &commitment_7,
                &commitment_8,
            ],
            vec![
                amount_1, amount_2, amount_3, amount_4, amount_5, amount_6, amount_7, amount_8,
            ],
            vec![32, 32, 32, 32, 32, 32, 32, 32],
            vec![
                &opening_1, &opening_2, &opening_3, &opening_4, &opening_5, &opening_6, &opening_7,
                &opening_8,
            ],
        )
        .unwrap();

        assert!(proof_data.verify_proof().is_ok());

        let amount_1 = 4294967296_u64; // not representable as an 8-bit number
        let amount_2 = 77_u64;
        let amount_3 = 99_u64;
        let amount_4 = 99_u64;
        let amount_5 = 11_u64;
        let amount_6 = 33_u64;
        let amount_7 = 99_u64;
        let amount_8 = 99_u64;

        let (commitment_1, opening_1) = Pedersen::new(amount_1);
        let (commitment_2, opening_2) = Pedersen::new(amount_2);
        let (commitment_3, opening_3) = Pedersen::new(amount_3);
        let (commitment_4, opening_4) = Pedersen::new(amount_4);
        let (commitment_5, opening_5) = Pedersen::new(amount_5);
        let (commitment_6, opening_6) = Pedersen::new(amount_6);
        let (commitment_7, opening_7) = Pedersen::new(amount_7);
        let (commitment_8, opening_8) = Pedersen::new(amount_8);

        let proof_data = AggregateRangeProof256Data::new(
            vec![
                &commitment_1,
                &commitment_2,
                &commitment_3,
                &commitment_4,
                &commitment_5,
                &commitment_6,
                &commitment_7,
                &commitment_8,
            ],
            vec![
                amount_1, amount_2, amount_3, amount_4, amount_5, amount_6, amount_7, amount_8,
            ],
            vec![32, 32, 32, 32, 32, 32, 32, 32],
            vec![
                &opening_1, &opening_2, &opening_3, &opening_4, &opening_5, &opening_6, &opening_7,
                &opening_8,
            ],
        )
        .unwrap();

        assert_eq!(
            proof_data.verify_proof().unwrap_err(),
            ProofError::VerificationError(
                ProofType::RangeProof,
                ProofVerificationError::AlgebraicRelation
            ),
        );
    }
}
