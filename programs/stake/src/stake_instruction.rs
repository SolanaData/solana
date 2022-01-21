#[deprecated(
    since = "1.7.2",
    note = "Please use `solana_sdk::stake::instruction` or `solana_program::stake::instruction` instead"
)]
pub use solana_sdk::stake::instruction::*;
use {
    crate::{config, stake_state::StakeAccount},
    log::*,
    solana_sdk::{
        feature_set,
        instruction::InstructionError,
        keyed_account::{from_keyed_account, get_signers, keyed_account_at_index},
        process_instruction::InvokeContext,
        program_utils::limited_deserialize,
        pubkey::Pubkey,
        stake::{
            instruction::StakeInstruction,
            program::id,
            state::{Authorized, Lockup},
        },
        sysvar::{clock::Clock, rent::Rent, stake_history::StakeHistory},
    },
};

pub fn process_instruction(
    _program_id: &Pubkey,
    data: &[u8],
    invoke_context: &mut dyn InvokeContext,
) -> Result<(), InstructionError> {
    let keyed_accounts = invoke_context.get_keyed_accounts()?;

    trace!("process_instruction: {:?}", data);
    trace!("keyed_accounts: {:?}", keyed_accounts);

    let signers = get_signers(keyed_accounts);

    let me = &keyed_account_at_index(keyed_accounts, 0)?;

    if me.owner()? != id() {
        if invoke_context.is_feature_active(&feature_set::check_program_owner::id()) {
            return Err(InstructionError::InvalidAccountOwner);
        } else {
            return Err(InstructionError::IncorrectProgramId);
        }
    }

    match limited_deserialize(data)? {
        StakeInstruction::Initialize(authorized, lockup) => me.initialize(
            &authorized,
            &lockup,
            &from_keyed_account::<Rent>(keyed_account_at_index(keyed_accounts, 1)?)?,
        ),
        StakeInstruction::Authorize(authorized_pubkey, stake_authorize) => {
            let require_custodian_for_locked_stake_authorize = invoke_context.is_feature_active(
                &feature_set::require_custodian_for_locked_stake_authorize::id(),
            );

            if require_custodian_for_locked_stake_authorize {
                let clock =
                    from_keyed_account::<Clock>(keyed_account_at_index(keyed_accounts, 1)?)?;
                let _current_authority = keyed_account_at_index(keyed_accounts, 2)?;
                let custodian = keyed_account_at_index(keyed_accounts, 3)
                    .ok()
                    .map(|ka| ka.unsigned_key());

                me.authorize(
                    &signers,
                    &authorized_pubkey,
                    stake_authorize,
                    require_custodian_for_locked_stake_authorize,
                    &clock,
                    custodian,
                )
            } else {
                me.authorize(
                    &signers,
                    &authorized_pubkey,
                    stake_authorize,
                    require_custodian_for_locked_stake_authorize,
                    &Clock::default(),
                    None,
                )
            }
        }
        StakeInstruction::AuthorizeWithSeed(args) => {
            let authority_base = keyed_account_at_index(keyed_accounts, 1)?;
            let require_custodian_for_locked_stake_authorize = invoke_context.is_feature_active(
                &feature_set::require_custodian_for_locked_stake_authorize::id(),
            );

            if require_custodian_for_locked_stake_authorize {
                let clock =
                    from_keyed_account::<Clock>(keyed_account_at_index(keyed_accounts, 2)?)?;
                let custodian = keyed_account_at_index(keyed_accounts, 3)
                    .ok()
                    .map(|ka| ka.unsigned_key());

                me.authorize_with_seed(
                    authority_base,
                    &args.authority_seed,
                    &args.authority_owner,
                    &args.new_authorized_pubkey,
                    args.stake_authorize,
                    require_custodian_for_locked_stake_authorize,
                    &clock,
                    custodian,
                )
            } else {
                me.authorize_with_seed(
                    authority_base,
                    &args.authority_seed,
                    &args.authority_owner,
                    &args.new_authorized_pubkey,
                    args.stake_authorize,
                    require_custodian_for_locked_stake_authorize,
                    &Clock::default(),
                    None,
                )
            }
        }
        StakeInstruction::DelegateStake => {
            let can_reverse_deactivation =
                invoke_context.is_feature_active(&feature_set::stake_program_v4::id());
            let vote = keyed_account_at_index(keyed_accounts, 1)?;

            me.delegate(
                vote,
                &from_keyed_account::<Clock>(keyed_account_at_index(keyed_accounts, 2)?)?,
                &from_keyed_account::<StakeHistory>(keyed_account_at_index(keyed_accounts, 3)?)?,
                &config::from_keyed_account(keyed_account_at_index(keyed_accounts, 4)?)?,
                &signers,
                can_reverse_deactivation,
            )
        }
        StakeInstruction::Split(lamports) => {
            let split_stake = &keyed_account_at_index(keyed_accounts, 1)?;
            me.split(lamports, split_stake, &signers)
        }
        StakeInstruction::Merge => {
            let source_stake = &keyed_account_at_index(keyed_accounts, 1)?;
            let can_merge_expired_lockups =
                invoke_context.is_feature_active(&feature_set::stake_program_v4::id());
            me.merge(
                invoke_context,
                source_stake,
                &from_keyed_account::<Clock>(keyed_account_at_index(keyed_accounts, 2)?)?,
                &from_keyed_account::<StakeHistory>(keyed_account_at_index(keyed_accounts, 3)?)?,
                &signers,
                can_merge_expired_lockups,
            )
        }
        StakeInstruction::Withdraw(lamports) => {
            let to = &keyed_account_at_index(keyed_accounts, 1)?;
            me.withdraw(
                lamports,
                to,
                &from_keyed_account::<Clock>(keyed_account_at_index(keyed_accounts, 2)?)?,
                &from_keyed_account::<StakeHistory>(keyed_account_at_index(keyed_accounts, 3)?)?,
                keyed_account_at_index(keyed_accounts, 4)?,
                keyed_account_at_index(keyed_accounts, 5).ok(),
                invoke_context.is_feature_active(&feature_set::stake_program_v4::id()),
            )
        }
        StakeInstruction::Deactivate => me.deactivate(
            &from_keyed_account::<Clock>(keyed_account_at_index(keyed_accounts, 1)?)?,
            &signers,
        ),
        StakeInstruction::SetLockup(lockup) => {
            let clock = if invoke_context.is_feature_active(&feature_set::stake_program_v4::id()) {
                Some(invoke_context.get_sysvar_cache().get_clock()?)
            } else {
                None
            };
            me.set_lockup(&lockup, &signers, clock.as_deref())
        }
        StakeInstruction::InitializeChecked => {
            if invoke_context.is_feature_active(&feature_set::vote_stake_checked_instructions::id())
            {
                let authorized = Authorized {
                    staker: *keyed_account_at_index(keyed_accounts, 2)?.unsigned_key(),
                    withdrawer: *keyed_account_at_index(keyed_accounts, 3)?
                        .signer_key()
                        .ok_or(InstructionError::MissingRequiredSignature)?,
                };

                me.initialize(
                    &authorized,
                    &Lockup::default(),
                    &from_keyed_account::<Rent>(keyed_account_at_index(keyed_accounts, 1)?)?,
                )
            } else {
                Err(InstructionError::InvalidInstructionData)
            }
        }
        StakeInstruction::AuthorizeChecked(stake_authorize) => {
            if invoke_context.is_feature_active(&feature_set::vote_stake_checked_instructions::id())
            {
                let clock =
                    from_keyed_account::<Clock>(keyed_account_at_index(keyed_accounts, 1)?)?;
                let _current_authority = keyed_account_at_index(keyed_accounts, 2)?;
                let authorized_pubkey = &keyed_account_at_index(keyed_accounts, 3)?
                    .signer_key()
                    .ok_or(InstructionError::MissingRequiredSignature)?;
                let custodian = keyed_account_at_index(keyed_accounts, 4)
                    .ok()
                    .map(|ka| ka.unsigned_key());

                me.authorize(
                    &signers,
                    authorized_pubkey,
                    stake_authorize,
                    true,
                    &clock,
                    custodian,
                )
            } else {
                Err(InstructionError::InvalidInstructionData)
            }
        }
        StakeInstruction::AuthorizeCheckedWithSeed(args) => {
            if invoke_context.is_feature_active(&feature_set::vote_stake_checked_instructions::id())
            {
                let authority_base = keyed_account_at_index(keyed_accounts, 1)?;
                let clock =
                    from_keyed_account::<Clock>(keyed_account_at_index(keyed_accounts, 2)?)?;
                let authorized_pubkey = &keyed_account_at_index(keyed_accounts, 3)?
                    .signer_key()
                    .ok_or(InstructionError::MissingRequiredSignature)?;
                let custodian = keyed_account_at_index(keyed_accounts, 4)
                    .ok()
                    .map(|ka| ka.unsigned_key());

                me.authorize_with_seed(
                    authority_base,
                    &args.authority_seed,
                    &args.authority_owner,
                    authorized_pubkey,
                    args.stake_authorize,
                    true,
                    &clock,
                    custodian,
                )
            } else {
                Err(InstructionError::InvalidInstructionData)
            }
        }
        StakeInstruction::SetLockupChecked(lockup_checked) => {
            if invoke_context.is_feature_active(&feature_set::vote_stake_checked_instructions::id())
            {
                let custodian = if let Ok(custodian) = keyed_account_at_index(keyed_accounts, 2) {
                    Some(
                        *custodian
                            .signer_key()
                            .ok_or(InstructionError::MissingRequiredSignature)?,
                    )
                } else {
                    None
                };

                let lockup = LockupArgs {
                    unix_timestamp: lockup_checked.unix_timestamp,
                    epoch: lockup_checked.epoch,
                    custodian,
                };
                let clock = Some(invoke_context.get_sysvar_cache().get_clock()?);
                me.set_lockup(&lockup, &signers, clock.as_deref())
            } else {
                Err(InstructionError::InvalidInstructionData)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::stake_state::{Meta, StakeState},
        bincode::serialize,
        solana_sdk::{
            account::{self, Account, AccountSharedData, WritableAccount},
            instruction::{AccountMeta, Instruction},
            keyed_account::KeyedAccount,
            process_instruction::MockInvokeContext,
            rent::Rent,
            stake::{
                config as stake_config,
                instruction::{self, LockupArgs},
                state::{Authorized, Lockup, StakeAuthorize},
            },
            sysvar::{self, stake_history::StakeHistory},
            sysvar_cache::SysvarCache,
        },
        std::{borrow::Cow, cell::RefCell, str::FromStr},
    };

    fn create_default_account() -> RefCell<AccountSharedData> {
        RefCell::new(AccountSharedData::default())
    }

    fn create_default_stake_account() -> RefCell<AccountSharedData> {
        RefCell::new(AccountSharedData::from(Account {
            owner: id(),
            ..Account::default()
        }))
    }

    fn invalid_stake_state_pubkey() -> Pubkey {
        Pubkey::from_str("BadStake11111111111111111111111111111111111").unwrap()
    }

    fn invalid_vote_state_pubkey() -> Pubkey {
        Pubkey::from_str("BadVote111111111111111111111111111111111111").unwrap()
    }

    fn spoofed_stake_state_pubkey() -> Pubkey {
        Pubkey::from_str("SpoofedStake1111111111111111111111111111111").unwrap()
    }

    fn spoofed_stake_program_id() -> Pubkey {
        Pubkey::from_str("Spoofed111111111111111111111111111111111111").unwrap()
    }

    fn process_instruction(instruction: &Instruction) -> Result<(), InstructionError> {
        let accounts: Vec<_> = instruction
            .accounts
            .iter()
            .map(|meta| {
                RefCell::new(if sysvar::clock::check_id(&meta.pubkey) {
                    account::create_account_shared_data_for_test(&sysvar::clock::Clock::default())
                } else if sysvar::rewards::check_id(&meta.pubkey) {
                    account::create_account_shared_data_for_test(&sysvar::rewards::Rewards::new(
                        0.0,
                    ))
                } else if sysvar::stake_history::check_id(&meta.pubkey) {
                    account::create_account_shared_data_for_test(&StakeHistory::default())
                } else if stake_config::check_id(&meta.pubkey) {
                    config::create_account(0, &stake_config::Config::default())
                } else if sysvar::rent::check_id(&meta.pubkey) {
                    account::create_account_shared_data_for_test(&Rent::default())
                } else if meta.pubkey == invalid_stake_state_pubkey() {
                    AccountSharedData::from(Account {
                        owner: id(),
                        ..Account::default()
                    })
                } else if meta.pubkey == invalid_vote_state_pubkey() {
                    AccountSharedData::from(Account {
                        owner: solana_vote_program::id(),
                        ..Account::default()
                    })
                } else if meta.pubkey == spoofed_stake_state_pubkey() {
                    AccountSharedData::from(Account {
                        owner: spoofed_stake_program_id(),
                        ..Account::default()
                    })
                } else {
                    AccountSharedData::from(Account {
                        owner: id(),
                        ..Account::default()
                    })
                })
            })
            .collect();

        {
            let keyed_accounts: Vec<_> = instruction
                .accounts
                .iter()
                .zip(accounts.iter())
                .map(|(meta, account)| KeyedAccount::new(&meta.pubkey, meta.is_signer, account))
                .collect();

            let mut invoke_context = MockInvokeContext::new(keyed_accounts);
            let mut sysvar_cache = SysvarCache::default();
            sysvar_cache.set_clock(Clock::default());
            invoke_context.sysvar_cache = Cow::Owned(sysvar_cache);
            super::process_instruction(&Pubkey::default(), &instruction.data, &mut invoke_context)
        }
    }

    #[test]
    fn test_stake_process_instruction() {
        assert_eq!(
            process_instruction(&instruction::initialize(
                &Pubkey::default(),
                &Authorized::default(),
                &Lockup::default()
            )),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(&instruction::authorize(
                &Pubkey::default(),
                &Pubkey::default(),
                &Pubkey::default(),
                StakeAuthorize::Staker,
                None,
            )),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(
                &instruction::split(
                    &Pubkey::default(),
                    &Pubkey::default(),
                    100,
                    &invalid_stake_state_pubkey(),
                )[2]
            ),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(
                &instruction::merge(
                    &Pubkey::default(),
                    &invalid_stake_state_pubkey(),
                    &Pubkey::default(),
                )[0]
            ),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(
                &instruction::split_with_seed(
                    &Pubkey::default(),
                    &Pubkey::default(),
                    100,
                    &invalid_stake_state_pubkey(),
                    &Pubkey::default(),
                    "seed"
                )[1]
            ),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(&instruction::delegate_stake(
                &Pubkey::default(),
                &Pubkey::default(),
                &invalid_vote_state_pubkey(),
            )),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(&instruction::withdraw(
                &Pubkey::default(),
                &Pubkey::default(),
                &solana_sdk::pubkey::new_rand(),
                100,
                None,
            )),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(&instruction::deactivate_stake(
                &Pubkey::default(),
                &Pubkey::default()
            )),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(&instruction::set_lockup(
                &Pubkey::default(),
                &LockupArgs::default(),
                &Pubkey::default()
            )),
            Err(InstructionError::InvalidAccountData),
        );
    }

    #[test]
    fn test_spoofed_stake_accounts() {
        assert_eq!(
            process_instruction(&instruction::initialize(
                &spoofed_stake_state_pubkey(),
                &Authorized::default(),
                &Lockup::default()
            )),
            Err(InstructionError::InvalidAccountOwner),
        );
        assert_eq!(
            process_instruction(&instruction::authorize(
                &spoofed_stake_state_pubkey(),
                &Pubkey::default(),
                &Pubkey::default(),
                StakeAuthorize::Staker,
                None,
            )),
            Err(InstructionError::InvalidAccountOwner),
        );
        assert_eq!(
            process_instruction(
                &instruction::split(
                    &spoofed_stake_state_pubkey(),
                    &Pubkey::default(),
                    100,
                    &Pubkey::default(),
                )[2]
            ),
            Err(InstructionError::InvalidAccountOwner),
        );
        assert_eq!(
            process_instruction(
                &instruction::split(
                    &Pubkey::default(),
                    &Pubkey::default(),
                    100,
                    &spoofed_stake_state_pubkey(),
                )[2]
            ),
            Err(InstructionError::IncorrectProgramId),
        );
        assert_eq!(
            process_instruction(
                &instruction::merge(
                    &spoofed_stake_state_pubkey(),
                    &Pubkey::default(),
                    &Pubkey::default(),
                )[0]
            ),
            Err(InstructionError::InvalidAccountOwner),
        );
        assert_eq!(
            process_instruction(
                &instruction::merge(
                    &Pubkey::default(),
                    &spoofed_stake_state_pubkey(),
                    &Pubkey::default(),
                )[0]
            ),
            Err(InstructionError::IncorrectProgramId),
        );
        assert_eq!(
            process_instruction(
                &instruction::split_with_seed(
                    &spoofed_stake_state_pubkey(),
                    &Pubkey::default(),
                    100,
                    &Pubkey::default(),
                    &Pubkey::default(),
                    "seed"
                )[1]
            ),
            Err(InstructionError::InvalidAccountOwner),
        );
        assert_eq!(
            process_instruction(&instruction::delegate_stake(
                &spoofed_stake_state_pubkey(),
                &Pubkey::default(),
                &Pubkey::default(),
            )),
            Err(InstructionError::InvalidAccountOwner),
        );
        assert_eq!(
            process_instruction(&instruction::withdraw(
                &spoofed_stake_state_pubkey(),
                &Pubkey::default(),
                &solana_sdk::pubkey::new_rand(),
                100,
                None,
            )),
            Err(InstructionError::InvalidAccountOwner),
        );
        assert_eq!(
            process_instruction(&instruction::deactivate_stake(
                &spoofed_stake_state_pubkey(),
                &Pubkey::default()
            )),
            Err(InstructionError::InvalidAccountOwner),
        );
        assert_eq!(
            process_instruction(&instruction::set_lockup(
                &spoofed_stake_state_pubkey(),
                &LockupArgs::default(),
                &Pubkey::default()
            )),
            Err(InstructionError::InvalidAccountOwner),
        );
    }

    #[test]
    fn test_stake_process_instruction_decode_bail() {
        // these will not call stake_state, have bogus contents

        // gets the "is_empty()" check
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::Initialize(
                    Authorized::default(),
                    Lockup::default()
                ))
                .unwrap(),
                &mut MockInvokeContext::new(vec![])
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );

        // no account for rent
        let stake_address = Pubkey::default();
        let stake_account = create_default_stake_account();
        let keyed_accounts = vec![KeyedAccount::new(&stake_address, false, &stake_account)];
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::Initialize(
                    Authorized::default(),
                    Lockup::default()
                ))
                .unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );

        // rent fails to deserialize
        let stake_address = Pubkey::default();
        let stake_account = create_default_stake_account();
        let rent_address = sysvar::rent::id();
        let rent_account = create_default_account();
        let keyed_accounts = vec![
            KeyedAccount::new(&stake_address, false, &stake_account),
            KeyedAccount::new(&rent_address, false, &rent_account),
        ];
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::Initialize(
                    Authorized::default(),
                    Lockup::default()
                ))
                .unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::InvalidArgument),
        );

        // fails to deserialize stake state
        let stake_address = Pubkey::default();
        let stake_account = create_default_stake_account();
        let rent_address = sysvar::rent::id();
        let rent_account = RefCell::new(account::create_account_shared_data_for_test(
            &Rent::default(),
        ));
        let keyed_accounts = vec![
            KeyedAccount::new(&stake_address, false, &stake_account),
            KeyedAccount::new(&rent_address, false, &rent_account),
        ];
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::Initialize(
                    Authorized::default(),
                    Lockup::default()
                ))
                .unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::InvalidAccountData),
        );

        // gets the first check in delegate, wrong number of accounts
        let stake_address = Pubkey::default();
        let stake_account = create_default_stake_account();
        let keyed_accounts = vec![KeyedAccount::new(&stake_address, false, &stake_account)];
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::DelegateStake).unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );

        // gets the sub-check for number of args
        let stake_address = Pubkey::default();
        let stake_account = create_default_stake_account();
        let keyed_accounts = vec![KeyedAccount::new(&stake_address, false, &stake_account)];
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::DelegateStake).unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );

        // gets the check non-deserialize-able account in delegate_stake
        let stake_address = Pubkey::default();
        let stake_account = create_default_stake_account();
        let vote_address = Pubkey::default();
        let mut bad_vote_account = create_default_account();
        bad_vote_account
            .get_mut()
            .set_owner(solana_vote_program::id());
        let clock_address = sysvar::clock::id();
        let clock_account = RefCell::new(account::create_account_shared_data_for_test(
            &sysvar::clock::Clock::default(),
        ));
        let stake_history_address = sysvar::stake_history::id();
        let stake_history_account = RefCell::new(account::create_account_shared_data_for_test(
            &sysvar::stake_history::StakeHistory::default(),
        ));
        let config_address = stake_config::id();
        let config_account =
            RefCell::new(config::create_account(0, &stake_config::Config::default()));
        let keyed_accounts = vec![
            KeyedAccount::new(&stake_address, true, &stake_account),
            KeyedAccount::new(&vote_address, false, &bad_vote_account),
            KeyedAccount::new(&clock_address, false, &clock_account),
            KeyedAccount::new(&stake_history_address, false, &stake_history_account),
            KeyedAccount::new(&config_address, false, &config_account),
        ];
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::DelegateStake).unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::InvalidAccountData),
        );

        // Tests 3rd keyed account is of correct type (Clock instead of rewards) in withdraw
        let stake_address = Pubkey::default();
        let stake_account = create_default_stake_account();
        let vote_address = Pubkey::default();
        let vote_account = create_default_account();
        let rewards_address = sysvar::rewards::id();
        let rewards_account = RefCell::new(account::create_account_shared_data_for_test(
            &sysvar::rewards::Rewards::new(0.0),
        ));
        let stake_history_address = sysvar::stake_history::id();
        let stake_history_account = RefCell::new(account::create_account_shared_data_for_test(
            &StakeHistory::default(),
        ));
        let keyed_accounts = vec![
            KeyedAccount::new(&stake_address, false, &stake_account),
            KeyedAccount::new(&vote_address, false, &vote_account),
            KeyedAccount::new(&rewards_address, false, &rewards_account),
            KeyedAccount::new(&stake_history_address, false, &stake_history_account),
        ];
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::Withdraw(42)).unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::InvalidArgument),
        );

        // Tests correct number of accounts are provided in withdraw
        let stake_address = Pubkey::default();
        let stake_account = create_default_stake_account();
        let keyed_accounts = vec![KeyedAccount::new(&stake_address, false, &stake_account)];
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::Withdraw(42)).unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );

        // Tests 2nd keyed account is of correct type (Clock instead of rewards) in deactivate
        let stake_address = Pubkey::default();
        let stake_account = create_default_stake_account();
        let rewards_address = sysvar::rewards::id();
        let rewards_account = RefCell::new(account::create_account_shared_data_for_test(
            &sysvar::rewards::Rewards::new(0.0),
        ));
        let keyed_accounts = vec![
            KeyedAccount::new(&stake_address, false, &stake_account),
            KeyedAccount::new(&rewards_address, false, &rewards_account),
        ];
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::Deactivate).unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::InvalidArgument),
        );

        // Tests correct number of accounts are provided in deactivate
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::Deactivate).unwrap(),
                &mut MockInvokeContext::new(vec![])
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );
    }

    #[test]
    fn test_stake_checked_instructions() {
        let stake_address = Pubkey::new_unique();
        let staker = Pubkey::new_unique();
        let withdrawer = Pubkey::new_unique();

        // Test InitializeChecked with non-signing withdrawer
        let mut instruction =
            initialize_checked(&stake_address, &Authorized { staker, withdrawer });
        instruction.accounts[3] = AccountMeta::new_readonly(withdrawer, false);
        assert_eq!(
            process_instruction(&instruction),
            Err(InstructionError::MissingRequiredSignature),
        );

        // Test InitializeChecked with withdrawer signer
        let stake_account = AccountSharedData::new_ref(
            1_000_000_000,
            std::mem::size_of::<crate::stake_state::StakeState>(),
            &id(),
        );
        let rent_address = sysvar::rent::id();
        let rent_account = RefCell::new(account::create_account_shared_data_for_test(
            &Rent::default(),
        ));
        let staker_account = create_default_account();
        let withdrawer_account = create_default_account();

        let keyed_accounts = vec![
            KeyedAccount::new(&stake_address, false, &stake_account),
            KeyedAccount::new(&rent_address, false, &rent_account),
            KeyedAccount::new(&staker, false, &staker_account),
            KeyedAccount::new(&withdrawer, true, &withdrawer_account),
        ];

        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::InitializeChecked).unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(()),
        );

        // Test AuthorizeChecked with non-signing authority
        let authorized_address = Pubkey::new_unique();
        let mut instruction = authorize_checked(
            &stake_address,
            &authorized_address,
            &staker,
            StakeAuthorize::Staker,
            None,
        );
        instruction.accounts[3] = AccountMeta::new_readonly(staker, false);
        assert_eq!(
            process_instruction(&instruction),
            Err(InstructionError::MissingRequiredSignature),
        );

        let mut instruction = authorize_checked(
            &stake_address,
            &authorized_address,
            &withdrawer,
            StakeAuthorize::Withdrawer,
            None,
        );
        instruction.accounts[3] = AccountMeta::new_readonly(withdrawer, false);
        assert_eq!(
            process_instruction(&instruction),
            Err(InstructionError::MissingRequiredSignature),
        );

        // Test AuthorizeChecked with authority signer
        let stake_account = AccountSharedData::new_ref_data_with_space(
            42,
            &StakeState::Initialized(Meta::auto(&authorized_address)),
            std::mem::size_of::<StakeState>(),
            &id(),
        )
        .unwrap();
        let clock_address = sysvar::clock::id();
        let clock_account = RefCell::new(account::create_account_shared_data_for_test(
            &Clock::default(),
        ));
        let authorized_account = create_default_account();
        let new_authorized_account = create_default_account();

        let keyed_accounts = vec![
            KeyedAccount::new(&stake_address, false, &stake_account),
            KeyedAccount::new(&clock_address, false, &clock_account),
            KeyedAccount::new(&authorized_address, true, &authorized_account),
            KeyedAccount::new(&staker, true, &new_authorized_account),
        ];

        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::AuthorizeChecked(StakeAuthorize::Staker)).unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(()),
        );

        let keyed_accounts = vec![
            KeyedAccount::new(&stake_address, false, &stake_account),
            KeyedAccount::new(&clock_address, false, &clock_account),
            KeyedAccount::new(&authorized_address, true, &authorized_account),
            KeyedAccount::new(&withdrawer, true, &new_authorized_account),
        ];

        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::AuthorizeChecked(
                    StakeAuthorize::Withdrawer
                ))
                .unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(()),
        );

        // Test AuthorizeCheckedWithSeed with non-signing authority
        let authorized_owner = Pubkey::new_unique();
        let seed = "test seed";
        let address_with_seed =
            Pubkey::create_with_seed(&authorized_owner, seed, &authorized_owner).unwrap();
        let mut instruction = authorize_checked_with_seed(
            &stake_address,
            &authorized_owner,
            seed.to_string(),
            &authorized_owner,
            &staker,
            StakeAuthorize::Staker,
            None,
        );
        instruction.accounts[3] = AccountMeta::new_readonly(staker, false);
        assert_eq!(
            process_instruction(&instruction),
            Err(InstructionError::MissingRequiredSignature),
        );

        let mut instruction = authorize_checked_with_seed(
            &stake_address,
            &authorized_owner,
            seed.to_string(),
            &authorized_owner,
            &staker,
            StakeAuthorize::Withdrawer,
            None,
        );
        instruction.accounts[3] = AccountMeta::new_readonly(staker, false);
        assert_eq!(
            process_instruction(&instruction),
            Err(InstructionError::MissingRequiredSignature),
        );

        // Test AuthorizeCheckedWithSeed with authority signer
        let stake_account = AccountSharedData::new_ref_data_with_space(
            42,
            &StakeState::Initialized(Meta::auto(&address_with_seed)),
            std::mem::size_of::<StakeState>(),
            &id(),
        )
        .unwrap();
        let keyed_accounts = vec![
            KeyedAccount::new(&address_with_seed, false, &stake_account),
            KeyedAccount::new(&authorized_owner, true, &authorized_account),
            KeyedAccount::new(&clock_address, false, &clock_account),
            KeyedAccount::new(&staker, true, &new_authorized_account),
        ];

        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::AuthorizeCheckedWithSeed(
                    AuthorizeCheckedWithSeedArgs {
                        stake_authorize: StakeAuthorize::Staker,
                        authority_seed: seed.to_string(),
                        authority_owner: authorized_owner,
                    }
                ))
                .unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(()),
        );

        let keyed_accounts = vec![
            KeyedAccount::new(&address_with_seed, false, &stake_account),
            KeyedAccount::new(&authorized_owner, true, &authorized_account),
            KeyedAccount::new(&clock_address, false, &clock_account),
            KeyedAccount::new(&withdrawer, true, &new_authorized_account),
        ];

        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::AuthorizeCheckedWithSeed(
                    AuthorizeCheckedWithSeedArgs {
                        stake_authorize: StakeAuthorize::Withdrawer,
                        authority_seed: seed.to_string(),
                        authority_owner: authorized_owner,
                    }
                ))
                .unwrap(),
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(()),
        );

        // Test SetLockupChecked with non-signing lockup custodian
        let custodian = Pubkey::new_unique();
        let mut instruction = set_lockup_checked(
            &stake_address,
            &LockupArgs {
                unix_timestamp: None,
                epoch: Some(1),
                custodian: Some(custodian),
            },
            &withdrawer,
        );
        instruction.accounts[2] = AccountMeta::new_readonly(custodian, false);
        assert_eq!(
            process_instruction(&instruction),
            Err(InstructionError::MissingRequiredSignature),
        );

        // Test SetLockupChecked with lockup custodian signer
        let stake_account = AccountSharedData::new_ref_data_with_space(
            42,
            &StakeState::Initialized(Meta::auto(&withdrawer)),
            std::mem::size_of::<StakeState>(),
            &id(),
        )
        .unwrap();
        let custodian_account = create_default_account();

        let keyed_accounts = vec![
            KeyedAccount::new(&stake_address, false, &stake_account),
            KeyedAccount::new(&withdrawer, true, &withdrawer_account),
            KeyedAccount::new(&custodian, true, &custodian_account),
        ];

        let mut invoke_context = MockInvokeContext::new(keyed_accounts);
        let mut sysvar_cache = SysvarCache::default();
        sysvar_cache.set_clock(Clock::default());
        invoke_context.sysvar_cache = Cow::Owned(sysvar_cache);

        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &serialize(&StakeInstruction::SetLockupChecked(LockupCheckedArgs {
                    unix_timestamp: None,
                    epoch: Some(1),
                }))
                .unwrap(),
                &mut invoke_context
            ),
            Ok(()),
        );
    }
}
