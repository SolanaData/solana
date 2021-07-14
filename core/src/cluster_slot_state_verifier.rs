use crate::{
    fork_choice::ForkChoice, heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice,
    progress_map::ProgressMap,
};
use solana_ledger::blockstore::Blockstore;
use solana_sdk::{clock::Slot, hash::Hash};
use std::collections::{BTreeMap, BTreeSet, HashSet};

pub(crate) type DuplicateSlotsTracker = BTreeSet<Slot>;
pub(crate) type DuplicateSlotsToRepair = HashSet<(Slot, Hash)>;
pub(crate) type GossipDuplicateConfirmedSlots = BTreeMap<Slot, Hash>;
type SlotStateHandler = fn(SlotStateHandlerArgs) -> Vec<ResultingStateChange>;

struct SlotStateHandlerArgs<'a> {
    slot: Slot,
    bank_frozen_hash: Hash,
    cluster_duplicate_confirmed_hash: Option<&'a Hash>,
    _is_slot_duplicate: bool,
    is_dead: bool,
}

#[derive(PartialEq, Debug)]
pub enum SlotStateUpdate {
    BankFrozen,
    DuplicateConfirmed,
    Dead,
    Duplicate,
}

#[derive(PartialEq, Debug)]
pub enum ResultingStateChange {
    // Bank was frozen
    BankFrozen(Hash),
    // Hash of our current frozen version of the slot
    MarkSlotDuplicate(Hash),
    // Hash of the cluster confirmed slot that is not equivalent
    // to our frozen version of the slot
    RepairDuplicateConfirmedVersion(Hash),
    // Hash of our current frozen version of the slot
    DuplicateConfirmedSlotMatchesCluster(Hash),
}

impl SlotStateUpdate {
    fn to_handler(&self) -> SlotStateHandler {
        match self {
            SlotStateUpdate::Dead => on_dead_slot,
            SlotStateUpdate::BankFrozen => on_frozen_slot,
            SlotStateUpdate::DuplicateConfirmed => on_duplicate_confirmed,
            SlotStateUpdate::Duplicate => on_duplicate,
        }
    }
}

fn check_duplicate_confirmed_hash_against_frozen_hash(
    state_changes: &mut Vec<ResultingStateChange>,
    slot: Slot,
    cluster_duplicate_confirmed_hash: Option<&Hash>,
    bank_frozen_hash: Hash,
    is_dead: bool,
) {
    if let Some(cluster_duplicate_confirmed_hash) = cluster_duplicate_confirmed_hash {
        // If the cluster duplicate_confirmed some version of this slot, then
        // confirm our version agrees with the cluster,
        if *cluster_duplicate_confirmed_hash != bank_frozen_hash {
            if is_dead {
                // If the cluster duplicate confirmed some version of this slot, then
                // there's another version of our dead slot
                warn!(
                    "Cluster duplicate_confirmed slot {} with hash {}, but we marked slot dead",
                    slot, cluster_duplicate_confirmed_hash
                );
            } else {
                // The duplicate confirmed slot hash does not match our frozen hash.
                // Modify fork choice rule to exclude our version from being voted
                // on and also repair the correct version
                warn!(
                    "Cluster duplicate_confirmed slot {} with hash {}, but we froze slot with hash {}",
                    slot, cluster_duplicate_confirmed_hash, bank_frozen_hash
                );
            }
            state_changes.push(ResultingStateChange::MarkSlotDuplicate(bank_frozen_hash));
            state_changes.push(ResultingStateChange::RepairDuplicateConfirmedVersion(
                *cluster_duplicate_confirmed_hash,
            ));
        } else {
            // If the versions match, then add the slot to the candidate
            // set to account for the case where it was removed earlier
            // by the `on_duplicate_slot()` handler
            state_changes.push(ResultingStateChange::DuplicateConfirmedSlotMatchesCluster(
                bank_frozen_hash,
            ));
        }
    }
}

fn on_dead_slot(args: SlotStateHandlerArgs) -> Vec<ResultingStateChange> {
    let SlotStateHandlerArgs {
        slot,
        bank_frozen_hash,
        cluster_duplicate_confirmed_hash,
        is_dead,
        ..
    } = args;

    assert!(is_dead);
    // Bank should not have been frozen if the slot was marked dead
    assert_eq!(bank_frozen_hash, Hash::default());

    let mut state_changes = vec![];
    check_duplicate_confirmed_hash_against_frozen_hash(
        &mut state_changes,
        slot,
        cluster_duplicate_confirmed_hash,
        bank_frozen_hash,
        is_dead,
    );

    state_changes
}

fn on_frozen_slot(args: SlotStateHandlerArgs) -> Vec<ResultingStateChange> {
    let SlotStateHandlerArgs {
        slot,
        bank_frozen_hash,
        cluster_duplicate_confirmed_hash,
        is_dead,
        ..
    } = args;
    // If a slot is marked frozen, the bank hash should not be default,
    // and the slot should not be dead
    assert!(bank_frozen_hash != Hash::default());
    assert!(!is_dead);

    let mut state_changes = vec![ResultingStateChange::BankFrozen(bank_frozen_hash)];
    check_duplicate_confirmed_hash_against_frozen_hash(
        &mut state_changes,
        slot,
        cluster_duplicate_confirmed_hash,
        bank_frozen_hash,
        is_dead,
    );

    state_changes
}

fn on_duplicate_confirmed(args: SlotStateHandlerArgs) -> Vec<ResultingStateChange> {
    let SlotStateHandlerArgs {
        slot,
        bank_frozen_hash,
        cluster_duplicate_confirmed_hash,
        is_dead,
        ..
    } = args;

    assert!(cluster_duplicate_confirmed_hash.is_some());
    if !is_dead && bank_frozen_hash == Hash::default() {
        return vec![];
    }

    let mut state_changes = vec![];
    check_duplicate_confirmed_hash_against_frozen_hash(
        &mut state_changes,
        slot,
        cluster_duplicate_confirmed_hash,
        bank_frozen_hash,
        is_dead,
    );

    state_changes
}

fn on_duplicate(args: SlotStateHandlerArgs) -> Vec<ResultingStateChange> {
    let SlotStateHandlerArgs {
        bank_frozen_hash,
        cluster_duplicate_confirmed_hash,
        is_dead,
        ..
    } = args;

    if !is_dead && bank_frozen_hash == Hash::default() {
        return vec![];
    }

    if let Some(cluster_duplicate_confirmed_hash) = cluster_duplicate_confirmed_hash {
        // If the cluster duplicate_confirmed some version of this slot, then
        // confirm our version agrees with the cluster,
        if *cluster_duplicate_confirmed_hash != bank_frozen_hash {
            // If we detected a duplicate, but have not yet seen any version
            // of the slot duplicate_confirmed (i.e. block above did not execute), then
            // remove the slot from fork choice until we get confirmation.

            // If we get here, we either detected duplicate from
            // 1) WindowService
            // 2) A gossip duplicate_confirmed version that didn't match our frozen
            // version.
            // In both cases, mark the progress map for this slot as duplicate
            return vec![ResultingStateChange::MarkSlotDuplicate(bank_frozen_hash)];
        }
    }

    vec![]
}

fn get_cluster_duplicate_confirmed_hash<'a>(
    slot: Slot,
    gossip_duplicate_confirmed_hash: Option<&'a Hash>,
    local_frozen_hash: &'a Hash,
    is_local_replay_duplicate_confirmed: bool,
) -> Option<&'a Hash> {
    let local_duplicate_confirmed_hash = if is_local_replay_duplicate_confirmed {
        // If local replay has duplicate_confirmed this slot, this slot must have
        // descendants with votes for this slot, hence this slot must be
        // frozen.
        assert!(*local_frozen_hash != Hash::default());
        Some(local_frozen_hash)
    } else {
        None
    };

    match (
        local_duplicate_confirmed_hash,
        gossip_duplicate_confirmed_hash,
    ) {
        (Some(local_duplicate_confirmed_hash), Some(gossip_duplicate_confirmed_hash)) => {
            if local_duplicate_confirmed_hash != gossip_duplicate_confirmed_hash {
                error!(
                    "For slot {}, the gossip duplicate confirmed hash {}, is not equal
                to the confirmed hash we replayed: {}",
                    slot, gossip_duplicate_confirmed_hash, local_duplicate_confirmed_hash
                );
            }
            Some(local_frozen_hash)
        }
        (Some(local_frozen_hash), None) => Some(local_frozen_hash),
        _ => gossip_duplicate_confirmed_hash,
    }
}

fn apply_state_changes(
    slot: Slot,
    fork_choice: &mut HeaviestSubtreeForkChoice,
    duplicate_slots_to_repair: &mut DuplicateSlotsToRepair,
    blockstore: &Blockstore,
    state_changes: Vec<ResultingStateChange>,
) {
    // Handle cases where the bank is frozen, but not duplicate confirmed
    // yet.
    let mut not_duplicate_confirmed_frozen_hash = None;
    for state_change in state_changes {
        match state_change {
            ResultingStateChange::BankFrozen(bank_frozen_hash) => {
                if !fork_choice
                    .is_duplicate_confirmed(&(slot, bank_frozen_hash))
                    .expect("frozen bank must exist in fork choice")
                {
                    not_duplicate_confirmed_frozen_hash = Some(bank_frozen_hash);
                }
            }
            ResultingStateChange::MarkSlotDuplicate(bank_frozen_hash) => {
                fork_choice.mark_fork_invalid_candidate(&(slot, bank_frozen_hash));
            }
            ResultingStateChange::RepairDuplicateConfirmedVersion(
                cluster_duplicate_confirmed_hash,
            ) => {
                duplicate_slots_to_repair.insert((slot, cluster_duplicate_confirmed_hash));
            }
            ResultingStateChange::DuplicateConfirmedSlotMatchesCluster(bank_frozen_hash) => {
                not_duplicate_confirmed_frozen_hash = None;
                // When we detect that our frozen slot matches the cluster version (note this
                // will catch both bank frozen first -> confirmation, or confirmation first ->
                // bank frozen), mark all the newly duplicate confirmed slots in blockstore
                let new_duplicate_confirmed_slot_hashes =
                    fork_choice.mark_fork_valid_candidate(&(slot, bank_frozen_hash));
                blockstore
                    .set_duplicate_confirmed_slots_and_hashes(
                        new_duplicate_confirmed_slot_hashes.into_iter(),
                    )
                    .unwrap();
            }
        }
    }

    if let Some(frozen_hash) = not_duplicate_confirmed_frozen_hash {
        blockstore.insert_bank_hash(slot, frozen_hash, false);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn check_slot_agrees_with_cluster(
    slot: Slot,
    root: Slot,
    blockstore: &Blockstore,
    bank_frozen_hash: Option<Hash>,
    duplicate_slots_tracker: &mut DuplicateSlotsTracker,
    gossip_duplicate_confirmed_slots: &GossipDuplicateConfirmedSlots,
    progress: &ProgressMap,
    fork_choice: &mut HeaviestSubtreeForkChoice,
    duplicate_slots_to_repair: &mut HashSet<(Slot, Hash)>,
    slot_state_update: SlotStateUpdate,
) {
    info!(
        "check_slot_agrees_with_cluster()
        slot: {},
        root: {},
        bank_frozen_hash: {:?},
        update: {:?}",
        slot, root, bank_frozen_hash, slot_state_update
    );

    if slot <= root {
        return;
    }

    // Needs to happen before the bank_frozen_hash.is_none() check below to account for duplicate
    // signals arriving before the bank is constructed in replay.
    if matches!(slot_state_update, SlotStateUpdate::Duplicate) {
        // If this slot has already been processed before, return
        if !duplicate_slots_tracker.insert(slot) {
            return;
        }
    }

    if bank_frozen_hash.is_none() {
        // If the bank doesn't even exist in BankForks yet,
        // then there's nothing to do as replay of the slot
        // hasn't even started
        return;
    }

    let bank_frozen_hash = bank_frozen_hash.unwrap();
    let gossip_duplicate_confirmed_hash = gossip_duplicate_confirmed_slots.get(&slot);

    // If the bank hasn't been frozen yet, then we haven't duplicate confirmed a local version
    // this slot through replay yet.
    let is_local_replay_duplicate_confirmed = fork_choice
        .is_duplicate_confirmed(&(slot, bank_frozen_hash))
        .unwrap_or(false);
    let cluster_duplicate_confirmed_hash = get_cluster_duplicate_confirmed_hash(
        slot,
        gossip_duplicate_confirmed_hash,
        &bank_frozen_hash,
        is_local_replay_duplicate_confirmed,
    );
    let is_slot_duplicate = duplicate_slots_tracker.contains(&slot);
    let is_dead = progress.is_dead(slot).expect("If the frozen hash exists, then the slot must exist in bank forks and thus in progress map");

    info!(
        "check_slot_agrees_with_cluster() state
        is_local_replay_duplicate_confirmed: {:?},
        cluster_duplicate_confirmed_hash: {:?},
        is_slot_duplicate: {:?},
        is_dead: {:?}",
        is_local_replay_duplicate_confirmed,
        cluster_duplicate_confirmed_hash,
        is_slot_duplicate,
        is_dead,
    );

    let state_handler = slot_state_update.to_handler();
    let args = SlotStateHandlerArgs {
        slot,
        bank_frozen_hash,
        cluster_duplicate_confirmed_hash,
        _is_slot_duplicate: is_slot_duplicate,
        is_dead,
    };
    let state_changes = state_handler(args);
    apply_state_changes(
        slot,
        fork_choice,
        duplicate_slots_to_repair,
        blockstore,
        state_changes,
    );
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::replay_stage::tests::setup_forks_from_tree;
    use solana_runtime::bank_forks::BankForks;
    use std::{
        collections::{HashMap, HashSet},
        sync::{Arc, RwLock},
    };
    use trees::tr;

    struct InitialState {
        heaviest_subtree_fork_choice: HeaviestSubtreeForkChoice,
        progress: ProgressMap,
        descendants: HashMap<Slot, HashSet<Slot>>,
        bank_forks: Arc<RwLock<BankForks>>,
        blockstore: Blockstore,
    }

    fn setup() -> InitialState {
        // Create simple fork 0 -> 1 -> 2 -> 3
        let forks = tr(0) / (tr(1) / (tr(2) / tr(3)));
        let (vote_simulator, blockstore) = setup_forks_from_tree(forks, 1, None);

        let descendants = vote_simulator
            .bank_forks
            .read()
            .unwrap()
            .descendants()
            .clone();

        InitialState {
            heaviest_subtree_fork_choice: vote_simulator.heaviest_subtree_fork_choice,
            progress: vote_simulator.progress,
            descendants,
            bank_forks: vote_simulator.bank_forks,
            blockstore,
        }
    }

    #[test]
    fn test_frozen_duplicate() {
        // Common state
        let slot = 0;
        let cluster_duplicate_confirmed_hash = None;
        let is_dead = false;

        // Slot is not detected as duplicate yet
        let mut is_slot_duplicate = false;

        // Simulate freezing the bank, add a
        // new non-default hash, should return
        // just a frozen state transition
        let bank_frozen_hash = Hash::new_unique();
        assert_eq!(
            on_frozen_slot(SlotStateHandlerArgs {
                slot,
                bank_frozen_hash,
                cluster_duplicate_confirmed_hash,
                is_slot_duplicate,
                is_dead,
                slot_state_update: SlotStateUpdate::BankFrozen
            }),
            vec![ResultingStateChange::BankFrozen(bank_frozen_hash)]
        );

        // Now mark the slot as duplicate, should
        // trigger marking the slot as a duplicate
        is_slot_duplicate = true;
        assert_eq!(
            on_cluster_update(SlotStateHandlerArgs {
                slot,
                bank_frozen_hash,
                cluster_duplicate_confirmed_hash,
                is_slot_duplicate,
                is_dead,
            }),
            vec![ResultingStateChange::MarkSlotDuplicate(bank_frozen_hash)]
        );
    }

    #[test]
    fn test_frozen_duplicate_confirmed() {
        // Common state
        let slot = 0;
        let is_slot_duplicate = false;
        let is_dead = false;

        // No cluster duplicate_confirmed hash yet
        let mut cluster_duplicate_confirmed_hash = None;

        // Simulate freezing the bank, add a
        // new non-default hash, should return
        // no actionable state changes
        let bank_frozen_hash = Hash::new_unique();
        assert_eq!(
            on_frozen_slot(SlotStateHandlerArgs {
                slot,
                bank_frozen_hash,
                cluster_duplicate_confirmed_hash,
                is_slot_duplicate,
                is_dead,
            }),
            vec![ResultingStateChange::BankFrozen(bank_frozen_hash)]
        );

        // Now mark the same frozen slot hash as duplicate_confirmed by the cluster,
        // should just confirm the slot
        cluster_duplicate_confirmed_hash = Some(&bank_frozen_hash);
        assert_eq!(
            on_cluster_update(SlotStateHandlerArgs {
                slot,
                bank_frozen_hash,
                cluster_duplicate_confirmed_hash,
                is_slot_duplicate,
                is_dead,
            }),
            vec![ResultingStateChange::DuplicateConfirmedSlotMatchesCluster(
                bank_frozen_hash
            ),]
        );

        // If we freeze the bank after we get a cluster duplicate confirmed hash
        // and both hashes match, then we should see both BankFrozen and
        // DuplicateConfirmedSlotMatchesCluster state transitions
        cluster_duplicate_confirmed_hash = Some(&bank_frozen_hash);
        assert_eq!(
            on_cluster_update(SlotStateHandlerArgs {
                slot,
                bank_frozen_hash,
                cluster_duplicate_confirmed_hash,
                is_slot_duplicate,
                is_dead,
            }),
            vec![
                ResultingStateChange::BankFrozen(bank_frozen_hash),
                ResultingStateChange::DuplicateConfirmedSlotMatchesCluster(bank_frozen_hash)
            ]
        );

        // If the cluster_duplicate_confirmed_hash does not match, then we
        // should trigger marking the slot as a duplicate, and also
        // try to repair correct version
        let mismatched_hash = Hash::new_unique();
        cluster_duplicate_confirmed_hash = Some(&mismatched_hash);
        assert_eq!(
            on_cluster_update(SlotStateHandlerArgs {
                slot,
                bank_frozen_hash,
                cluster_duplicate_confirmed_hash,
                is_slot_duplicate,
                is_dead,
            }),
            vec![
                ResultingStateChange::MarkSlotDuplicate(bank_frozen_hash),
                ResultingStateChange::RepairDuplicateConfirmedVersion(mismatched_hash),
            ]
        );
    }

    #[test]
    fn test_duplicate_frozen_duplicate_confirmed() {
        // Common state
        let slot = 0;
        let is_dead = false;
        let is_slot_duplicate = true;

        // Bank is not frozen yet
        let mut cluster_duplicate_confirmed_hash = None;
        let mut bank_frozen_hash = Hash::default();

        // Mark the slot as duplicate. Because our version of the slot is not
        // frozen yet, we don't know which version we have, so no action is
        // taken.
        assert!(on_cluster_update(SlotStateHandlerArgs {
            slot,
            bank_frozen_hash,
            cluster_duplicate_confirmed_hash,
            is_slot_duplicate,
            is_dead,
        })
        .is_empty());

        // Freeze the bank, should now mark the slot as duplicate since we have
        // not seen confirmation yet.
        bank_frozen_hash = Hash::new_unique();
        assert_eq!(
            on_cluster_update(SlotStateHandlerArgs {
                slot,
                bank_frozen_hash,
                cluster_duplicate_confirmed_hash,
                is_slot_duplicate,
                is_dead,
            }),
            vec![
                ResultingStateChange::BankFrozen(bank_frozen_hash),
                ResultingStateChange::MarkSlotDuplicate(bank_frozen_hash)
            ]
        );

        // If the cluster_duplicate_confirmed_hash matches, we just confirm
        // the slot
        cluster_duplicate_confirmed_hash = Some(&bank_frozen_hash);
        assert_eq!(
            on_cluster_update(SlotStateHandlerArgs {
                slot,
                bank_frozen_hash,
                cluster_duplicate_confirmed_hash,
                is_slot_duplicate,
                is_dead,
            }),
            vec![ResultingStateChange::DuplicateConfirmedSlotMatchesCluster(
                bank_frozen_hash
            ),]
        );

        // If the cluster_duplicate_confirmed_hash does not match, then we
        // should trigger marking the slot as a duplicate, and also
        // try to repair correct version
        let mismatched_hash = Hash::new_unique();
        cluster_duplicate_confirmed_hash = Some(&mismatched_hash);
        assert_eq!(
            on_cluster_update(SlotStateHandlerArgs {
                slot,
                bank_frozen_hash,
                cluster_duplicate_confirmed_hash,
                is_slot_duplicate,
                is_dead,
            }),
            vec![
                ResultingStateChange::MarkSlotDuplicate(bank_frozen_hash),
                ResultingStateChange::RepairDuplicateConfirmedVersion(mismatched_hash),
            ]
        );
    }

    #[test]
    fn test_duplicate_duplicate_confirmed() {
        let slot = 0;
        let correct_hash = Hash::new_unique();
        let cluster_duplicate_confirmed_hash = Some(&correct_hash);
        let is_dead = false;
        // Bank is not frozen yet
        let bank_frozen_hash = Hash::default();

        // Because our version of the slot is not frozen yet, then even though
        // the cluster has duplicate_confirmed a hash, we don't know which version we
        // have, so no action is taken.
        let is_slot_duplicate = true;
        assert!(on_cluster_update(SlotStateHandlerArgs {
            slot,
            bank_frozen_hash,
            cluster_duplicate_confirmed_hash,
            is_slot_duplicate,
            is_dead,
        })
        .is_empty());
    }

    #[test]
    fn test_duplicate_dead() {
        let slot = 0;
        let cluster_duplicate_confirmed_hash = None;
        // Bank is not frozen yet
        let bank_frozen_hash = Hash::default();

        // Simulate marking slot dead
        let mut is_slot_duplicate = false;
        let is_dead = true;
        assert!(on_cluster_update(SlotStateHandlerArgs {
            slot,
            bank_frozen_hash,
            cluster_duplicate_confirmed_hash,
            is_slot_duplicate,
            is_dead,
        })
        .is_empty());

        // Simulate detecting another duplicate version of this slot.
        // Even though our version of the slot is dead, the cluster has not
        // duplicate_confirmed a hash, we don't know which version we have, so no action
        // is taken.
        is_slot_duplicate = true;
        assert!(on_cluster_update(SlotStateHandlerArgs {
            slot,
            bank_frozen_hash,
            cluster_duplicate_confirmed_hash,
            is_slot_duplicate,
            is_dead,
        })
        .is_empty());
    }

    #[test]
    fn test_duplicate_confirmed_dead_before_duplicate() {
        let slot = 0;
        let correct_hash = Hash::new_unique();

        // 1) Simulate marking our version of the slot as dead,
        // cluster has not duplicate confirmed a slot yet.
        let bank_frozen_hash = Hash::default();
        let mut is_slot_duplicate = false;
        let mut is_dead = true;
        let cluster_duplicate_confirmed_hash = None;
        assert!(on_cluster_update(SlotStateHandlerArgs {
            slot,
            bank_frozen_hash,
            cluster_duplicate_confirmed_hash,
            is_slot_duplicate,
            is_dead,
        })
        .is_empty());

        // 2) Now simulate that the cluster has duplicate confirmed some version of the slot
        let cluster_duplicate_confirmed_hash = Some(&correct_hash);

        // Even if the duplicate signal hasn't come in yet,
        // we can deduce the slot is duplicate AND we have,
        // the wrong version, so should mark the slot as duplicate,
        // and repair the correct version
        assert_eq!(
            on_cluster_update(SlotStateHandlerArgs {
                slot,
                bank_frozen_hash,
                cluster_duplicate_confirmed_hash,
                is_slot_duplicate,
                is_dead,
            }),
            vec![
                ResultingStateChange::MarkSlotDuplicate(bank_frozen_hash),
                ResultingStateChange::RepairDuplicateConfirmedVersion(correct_hash),
            ]
        );

        // 3) When the SlotStateUpdate::Duplicate signal comes in, it should not trigger
        // another series of repair requests, even if the `cluster_duplicate_confirmed_hash`
        // is known.
        is_slot_duplicate = true;
        assert!(on_cluster_update(SlotStateHandlerArgs {
            slot,
            bank_frozen_hash,
            cluster_duplicate_confirmed_hash,
            is_slot_duplicate,
            is_dead,
        })
        .is_empty());

        // 4) After we dump and repair and replay another version of the slot, if that version
        // still mismatches, we should trigger another repair
        let new_bad_bank_frozen_hash = Hash::new_unique();
        is_dead = false;
        assert_eq!(
            on_frozen_slot(SlotStateHandlerArgs {
                slot,
                bank_frozen_hash: new_bad_bank_frozen_hash,
                cluster_duplicate_confirmed_hash,
                is_slot_duplicate,
                is_dead,
            }),
            vec![
                ResultingStateChange::BankFrozen(new_bad_bank_frozen_hash),
                ResultingStateChange::MarkSlotDuplicate(new_bad_bank_frozen_hash),
                ResultingStateChange::RepairDuplicateConfirmedVersion(correct_hash),
            ]
        );
    }

    #[test]
    fn test_apply_state_changes() {
        // Common state
        let InitialState {
            mut heaviest_subtree_fork_choice,
            descendants,
            bank_forks,
            blockstore,
            ..
        } = setup();

        let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();

        // MarkSlotDuplicate should mark progress map and remove
        // the slot from fork choice
        let duplicate_slot = bank_forks.read().unwrap().root() + 1;
        let duplicate_slot_hash = bank_forks
            .read()
            .unwrap()
            .get(duplicate_slot)
            .unwrap()
            .hash();
        apply_state_changes(
            duplicate_slot,
            &mut heaviest_subtree_fork_choice,
            &mut duplicate_slots_to_repair,
            &blockstore,
            vec![ResultingStateChange::MarkSlotDuplicate(duplicate_slot_hash)],
        );
        assert!(!heaviest_subtree_fork_choice
            .is_candidate(&(duplicate_slot, duplicate_slot_hash))
            .unwrap());
        for child_slot in descendants
            .get(&duplicate_slot)
            .unwrap()
            .iter()
            .chain(std::iter::once(&duplicate_slot))
        {
            assert_eq!(
                heaviest_subtree_fork_choice
                    .latest_invalid_ancestor(&(
                        *child_slot,
                        bank_forks.read().unwrap().get(*child_slot).unwrap().hash()
                    ))
                    .unwrap(),
                duplicate_slot
            );
        }
        assert!(duplicate_slots_to_repair.is_empty());

        // Simulate detecting another hash that is the correct version,
        // RepairDuplicateConfirmedVersion should add the slot to repair
        // to `duplicate_slots_to_repair`
        assert!(duplicate_slots_to_repair.is_empty());
        let correct_hash = Hash::new_unique();
        apply_state_changes(
            duplicate_slot,
            &mut heaviest_subtree_fork_choice,
            &mut duplicate_slots_to_repair,
            &blockstore,
            vec![ResultingStateChange::RepairDuplicateConfirmedVersion(
                correct_hash,
            )],
        );
        assert_eq!(duplicate_slots_to_repair.len(), 1);
        assert!(duplicate_slots_to_repair.contains(&(duplicate_slot, correct_hash)));
    }

    #[test]
    fn test_apply_state_changes_bank_frozen() {
        // Common state
        let InitialState {
            mut heaviest_subtree_fork_choice,
            bank_forks,
            blockstore,
            ..
        } = setup();

        let duplicate_slot = bank_forks.read().unwrap().root() + 1;
        let duplicate_slot_hash = bank_forks
            .read()
            .unwrap()
            .get(duplicate_slot)
            .unwrap()
            .hash();
        let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();

        // Simulate ReplayStage freezing a Bank with the given hash.
        // BankFrozen should mark it down in Blockstore.
        assert!(blockstore.get_bank_hash(duplicate_slot).is_none());
        apply_state_changes(
            duplicate_slot,
            &mut heaviest_subtree_fork_choice,
            &mut duplicate_slots_to_repair,
            &blockstore,
            vec![ResultingStateChange::BankFrozen(duplicate_slot_hash)],
        );
        assert_eq!(
            blockstore.get_bank_hash(duplicate_slot).unwrap(),
            duplicate_slot_hash
        );
        assert!(!blockstore.is_duplicate_confirmed(duplicate_slot));

        // If we freeze another version of the bank, it should overwrite the first
        // version in blockstore.
        let new_bank_hash = Hash::new_unique();
        let root_slot_hash = {
            let root_bank = bank_forks.read().unwrap().root_bank();
            (root_bank.slot(), root_bank.hash())
        };
        heaviest_subtree_fork_choice
            .add_new_leaf_slot((duplicate_slot, new_bank_hash), Some(root_slot_hash));
        apply_state_changes(
            duplicate_slot,
            &mut heaviest_subtree_fork_choice,
            &mut duplicate_slots_to_repair,
            &blockstore,
            vec![ResultingStateChange::BankFrozen(new_bank_hash)],
        );
        assert_eq!(
            blockstore.get_bank_hash(duplicate_slot).unwrap(),
            new_bank_hash
        );
        assert!(!blockstore.is_duplicate_confirmed(duplicate_slot));
    }

    fn run_test_apply_state_changes_duplicate_confirmed_matches_frozen(
        modify_state_changes: impl Fn(Hash, &mut Vec<ResultingStateChange>),
    ) {
        // Common state
        let InitialState {
            mut heaviest_subtree_fork_choice,
            descendants,
            bank_forks,
            blockstore,
            ..
        } = setup();

        let duplicate_slot = bank_forks.read().unwrap().root() + 1;
        let our_duplicate_slot_hash = bank_forks
            .read()
            .unwrap()
            .get(duplicate_slot)
            .unwrap()
            .hash();
        let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();

        // Setup and check the state that is about to change.
        assert!(blockstore.get_bank_hash(duplicate_slot).is_none());
        assert!(!blockstore.is_duplicate_confirmed(duplicate_slot));

        // DuplicateConfirmedSlotMatchesCluster should:
        // 1) Re-enable fork choice
        // 2) Set the status to duplicate confirmed in Blockstore
        let mut state_changes = vec![ResultingStateChange::DuplicateConfirmedSlotMatchesCluster(
            our_duplicate_slot_hash,
        )];
        modify_state_changes(our_duplicate_slot_hash, &mut state_changes);
        apply_state_changes(
            duplicate_slot,
            &mut heaviest_subtree_fork_choice,
            &mut duplicate_slots_to_repair,
            &blockstore,
            state_changes,
        );
        for child_slot in descendants
            .get(&duplicate_slot)
            .unwrap()
            .iter()
            .chain(std::iter::once(&duplicate_slot))
        {
            assert!(heaviest_subtree_fork_choice
                .latest_invalid_ancestor(&(
                    *child_slot,
                    bank_forks.read().unwrap().get(*child_slot).unwrap().hash()
                ))
                .is_none());
        }
        assert!(heaviest_subtree_fork_choice
            .is_candidate(&(duplicate_slot, our_duplicate_slot_hash))
            .unwrap());
        assert_eq!(
            blockstore.get_bank_hash(duplicate_slot).unwrap(),
            our_duplicate_slot_hash
        );
        assert!(blockstore.is_duplicate_confirmed(duplicate_slot));
    }

    #[test]
    fn test_apply_state_changes_duplicate_confirmed_matches_frozen() {
        run_test_apply_state_changes_duplicate_confirmed_matches_frozen(
            |_our_duplicate_slot_hash, _state_changes: &mut Vec<ResultingStateChange>| {},
        );
    }

    #[test]
    fn test_apply_state_changes_bank_frozen_and_duplicate_confirmed_matches_frozen() {
        run_test_apply_state_changes_duplicate_confirmed_matches_frozen(
            |our_duplicate_slot_hash, state_changes: &mut Vec<ResultingStateChange>| {
                state_changes.push(ResultingStateChange::BankFrozen(our_duplicate_slot_hash));
            },
        );
    }

    fn run_test_state_duplicate_then_bank_frozen(initial_bank_hash: Option<Hash>) {
        // Common state
        let InitialState {
            mut heaviest_subtree_fork_choice,
            progress,
            bank_forks,
            blockstore,
            ..
        } = setup();

        // Setup a duplicate slot state transition with the initial bank state of the duplicate slot
        // determined by `initial_bank_hash`, which can be:
        // 1) A default hash (unfrozen bank),
        // 2) None (a slot that hasn't even started replay yet).
        let root = 0;
        let mut duplicate_slots_tracker = DuplicateSlotsTracker::default();
        let gossip_duplicate_confirmed_slots = GossipDuplicateConfirmedSlots::default();
        let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();
        let duplicate_slot = 2;
        check_slot_agrees_with_cluster(
            duplicate_slot,
            root,
            &blockstore,
            initial_bank_hash,
            &mut duplicate_slots_tracker,
            &gossip_duplicate_confirmed_slots,
            &progress,
            &mut heaviest_subtree_fork_choice,
            &mut duplicate_slots_to_repair,
            SlotStateUpdate::Duplicate,
        );
        assert!(duplicate_slots_tracker.contains(&duplicate_slot));
        // Nothing should be applied yet to fork choice, since bank was not yet frozen
        for slot in 2..=3 {
            let slot_hash = bank_forks.read().unwrap().get(slot).unwrap().hash();
            assert!(heaviest_subtree_fork_choice
                .latest_invalid_ancestor(&(slot, slot_hash))
                .is_none());
        }

        // Now freeze the bank
        let frozen_duplicate_slot_hash = bank_forks
            .read()
            .unwrap()
            .get(duplicate_slot)
            .unwrap()
            .hash();
        check_slot_agrees_with_cluster(
            duplicate_slot,
            root,
            &blockstore,
            Some(frozen_duplicate_slot_hash),
            &mut duplicate_slots_tracker,
            &gossip_duplicate_confirmed_slots,
            &progress,
            &mut heaviest_subtree_fork_choice,
            &mut duplicate_slots_to_repair,
            SlotStateUpdate::BankFrozen,
        );

        // Progress map should have the correct updates, fork choice should mark duplicate
        // as unvotable
        assert!(heaviest_subtree_fork_choice
            .is_unconfirmed_duplicate(&(duplicate_slot, frozen_duplicate_slot_hash))
            .unwrap());

        // The ancestor of the duplicate slot should be the best slot now
        let (duplicate_ancestor, duplicate_parent_hash) = {
            let r_bank_forks = bank_forks.read().unwrap();
            let parent_bank = r_bank_forks.get(duplicate_slot).unwrap().parent().unwrap();
            (parent_bank.slot(), parent_bank.hash())
        };
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot(),
            (duplicate_ancestor, duplicate_parent_hash)
        );
    }

    #[test]
    fn test_state_unfrozen_bank_duplicate_then_bank_frozen() {
        run_test_state_duplicate_then_bank_frozen(Some(Hash::default()));
    }

    #[test]
    fn test_state_unreplayed_bank_duplicate_then_bank_frozen() {
        run_test_state_duplicate_then_bank_frozen(None);
    }

    #[test]
    fn test_state_ancestor_confirmed_descendant_duplicate() {
        // Common state
        let InitialState {
            mut heaviest_subtree_fork_choice,
            progress,
            bank_forks,
            blockstore,
            ..
        } = setup();

        let slot3_hash = bank_forks.read().unwrap().get(3).unwrap().hash();
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot(),
            (3, slot3_hash)
        );
        let root = 0;
        let mut duplicate_slots_tracker = DuplicateSlotsTracker::default();
        let mut gossip_duplicate_confirmed_slots = GossipDuplicateConfirmedSlots::default();

        // Mark slot 2 as duplicate confirmed
        let slot2_hash = bank_forks.read().unwrap().get(2).unwrap().hash();
        gossip_duplicate_confirmed_slots.insert(2, slot2_hash);
        check_slot_agrees_with_cluster(
            2,
            root,
            &blockstore,
            Some(slot2_hash),
            &mut duplicate_slots_tracker,
            &gossip_duplicate_confirmed_slots,
            &progress,
            &mut heaviest_subtree_fork_choice,
            &mut DuplicateSlotsToRepair::default(),
            SlotStateUpdate::DuplicateConfirmed,
        );
        assert!(heaviest_subtree_fork_choice
            .is_duplicate_confirmed(&(2, slot2_hash))
            .unwrap());
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot(),
            (3, slot3_hash)
        );
        for slot in 0..=2 {
            let slot_hash = bank_forks.read().unwrap().get(slot).unwrap().hash();
            assert!(heaviest_subtree_fork_choice
                .is_duplicate_confirmed(&(slot, slot_hash))
                .unwrap());
            assert!(heaviest_subtree_fork_choice
                .latest_invalid_ancestor(&(slot, slot_hash))
                .is_none());
        }

        // Mark 3 as duplicate, should not remove the duplicate confirmed slot 2 from
        // fork choice
        check_slot_agrees_with_cluster(
            3,
            root,
            &blockstore,
            Some(slot3_hash),
            &mut duplicate_slots_tracker,
            &gossip_duplicate_confirmed_slots,
            &progress,
            &mut heaviest_subtree_fork_choice,
            &mut DuplicateSlotsToRepair::default(),
            SlotStateUpdate::Duplicate,
        );
        assert!(duplicate_slots_tracker.contains(&3));
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot(),
            (2, slot2_hash)
        );
        for slot in 0..=3 {
            let slot_hash = bank_forks.read().unwrap().get(slot).unwrap().hash();
            if slot <= 2 {
                assert!(heaviest_subtree_fork_choice
                    .is_duplicate_confirmed(&(slot, slot_hash))
                    .unwrap());
                assert!(heaviest_subtree_fork_choice
                    .latest_invalid_ancestor(&(slot, slot_hash))
                    .is_none());
            } else {
                assert!(!heaviest_subtree_fork_choice
                    .is_duplicate_confirmed(&(slot, slot_hash))
                    .unwrap());
                assert_eq!(
                    heaviest_subtree_fork_choice
                        .latest_invalid_ancestor(&(slot, slot_hash))
                        .unwrap(),
                    3
                );
            }
        }
    }

    #[test]
    fn test_state_ancestor_duplicate_descendant_confirmed() {
        // Common state
        let InitialState {
            mut heaviest_subtree_fork_choice,
            progress,
            bank_forks,
            blockstore,
            ..
        } = setup();

        let slot3_hash = bank_forks.read().unwrap().get(3).unwrap().hash();
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot(),
            (3, slot3_hash)
        );
        let root = 0;
        let mut duplicate_slots_tracker = DuplicateSlotsTracker::default();
        let mut gossip_duplicate_confirmed_slots = GossipDuplicateConfirmedSlots::default();

        // Mark 2 as duplicate
        check_slot_agrees_with_cluster(
            2,
            root,
            &blockstore,
            Some(bank_forks.read().unwrap().get(2).unwrap().hash()),
            &mut duplicate_slots_tracker,
            &gossip_duplicate_confirmed_slots,
            &progress,
            &mut heaviest_subtree_fork_choice,
            &mut DuplicateSlotsToRepair::default(),
            SlotStateUpdate::Duplicate,
        );
        assert!(duplicate_slots_tracker.contains(&2));
        for slot in 2..=3 {
            let slot_hash = bank_forks.read().unwrap().get(slot).unwrap().hash();
            assert_eq!(
                heaviest_subtree_fork_choice
                    .latest_invalid_ancestor(&(slot, slot_hash))
                    .unwrap(),
                2
            );
        }

        let slot1_hash = bank_forks.read().unwrap().get(1).unwrap().hash();
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot(),
            (1, slot1_hash)
        );

        // Mark slot 3 as duplicate confirmed, should mark slot 2 as duplicate confirmed as well
        gossip_duplicate_confirmed_slots.insert(3, slot3_hash);
        check_slot_agrees_with_cluster(
            3,
            root,
            &blockstore,
            Some(slot3_hash),
            &mut duplicate_slots_tracker,
            &gossip_duplicate_confirmed_slots,
            &progress,
            &mut heaviest_subtree_fork_choice,
            &mut DuplicateSlotsToRepair::default(),
            SlotStateUpdate::DuplicateConfirmed,
        );
        for slot in 0..=3 {
            let slot_hash = bank_forks.read().unwrap().get(slot).unwrap().hash();
            assert!(heaviest_subtree_fork_choice
                .is_duplicate_confirmed(&(slot, slot_hash))
                .unwrap());
            assert!(heaviest_subtree_fork_choice
                .latest_invalid_ancestor(&(slot, slot_hash))
                .is_none());
        }
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot(),
            (3, slot3_hash)
        );
    }

    #[test]
    fn test_state_descendant_confirmed_ancestor_duplicate() {
        // Common state
        let InitialState {
            mut heaviest_subtree_fork_choice,
            progress,
            bank_forks,
            blockstore,
            ..
        } = setup();

        let slot3_hash = bank_forks.read().unwrap().get(3).unwrap().hash();
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot(),
            (3, slot3_hash)
        );
        let root = 0;
        let mut duplicate_slots_tracker = DuplicateSlotsTracker::default();
        let mut gossip_duplicate_confirmed_slots = GossipDuplicateConfirmedSlots::default();
        let mut duplicate_slots_to_repair = DuplicateSlotsToRepair::default();

        // Mark 3 as duplicate confirmed
        gossip_duplicate_confirmed_slots.insert(3, slot3_hash);
        check_slot_agrees_with_cluster(
            3,
            root,
            &blockstore,
            Some(slot3_hash),
            &mut duplicate_slots_tracker,
            &gossip_duplicate_confirmed_slots,
            &progress,
            &mut heaviest_subtree_fork_choice,
            &mut duplicate_slots_to_repair,
            SlotStateUpdate::DuplicateConfirmed,
        );
        let verify_all_slots_duplicate_confirmed =
            |bank_forks: &RwLock<BankForks>,
             heaviest_subtree_fork_choice: &HeaviestSubtreeForkChoice| {
                for slot in 0..=3 {
                    let slot_hash = bank_forks.read().unwrap().get(slot).unwrap().hash();
                    assert!(heaviest_subtree_fork_choice
                        .is_duplicate_confirmed(&(slot, slot_hash))
                        .unwrap());
                    assert!(heaviest_subtree_fork_choice
                        .latest_invalid_ancestor(&(slot, slot_hash))
                        .is_none());
                }
            };
        verify_all_slots_duplicate_confirmed(&bank_forks, &heaviest_subtree_fork_choice);
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot(),
            (3, slot3_hash)
        );

        // Mark ancestor 1 as duplicate, fork choice should be unaffected since
        // slot 1 was duplicate confirmed by the confirmation on its
        // descendant, 3.
        let slot1_hash = bank_forks.read().unwrap().get(1).unwrap().hash();
        check_slot_agrees_with_cluster(
            1,
            root,
            &blockstore,
            Some(slot1_hash),
            &mut duplicate_slots_tracker,
            &gossip_duplicate_confirmed_slots,
            &progress,
            &mut heaviest_subtree_fork_choice,
            &mut duplicate_slots_to_repair,
            SlotStateUpdate::Duplicate,
        );
        assert!(duplicate_slots_tracker.contains(&1));
        verify_all_slots_duplicate_confirmed(&bank_forks, &heaviest_subtree_fork_choice);
        assert_eq!(
            heaviest_subtree_fork_choice.best_overall_slot(),
            (3, slot3_hash)
        );
    }
}
