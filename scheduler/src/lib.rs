#![feature(map_first_last)]

use {
    atomic_enum::atomic_enum,
    crossbeam_channel::{bounded, unbounded},
    log::*,
    rand::Rng,
    sha2::{Digest, Sha256},
    solana_entry::entry::Entry,
    solana_measure::measure::Measure,
    solana_metrics::datapoint_info,
    solana_sdk::{
        hash::Hash,
        pubkey::Pubkey,
        transaction::{SanitizedTransaction, TransactionAccountLocks, VersionedTransaction},
    },
};

#[derive(Debug)]
pub struct ExecutionEnvironment {
    lock_attempts: Vec<LockAttempt>,
    //accounts: Vec<i8>,
    pub cu: usize,
    pub task: Task,
}

impl ExecutionEnvironment {
    //fn new(cu: usize) -> Self {
    //    Self {
    //        cu,
    //        ..Self::default()
    //    }
    //}

    //fn abort() {
    //  pass AtomicBool into InvokeContext??
    //}
}

#[derive(Debug)]
struct LockAttempt {
    address: Pubkey,
    is_success: bool,
    requested_usage: RequestedUsage,
}

impl LockAttempt {
    fn is_success(&self) -> bool {
        self.is_success
    }

    fn is_failed(&self) -> bool {
        !self.is_success()
    }

    fn success(address: Pubkey, requested_usage: RequestedUsage) -> Self {
        Self {
            address,
            is_success: true,
            requested_usage,
        }
    }

    fn failure(address: Pubkey, requested_usage: RequestedUsage) -> Self {
        Self {
            address,
            is_success: false,
            requested_usage,
        }
    }
}

type UsageCount = usize;
const SOLE_USE_COUNT: UsageCount = 1;

#[derive(PartialEq)]
enum CurrentUsage {
    Unused,
    // weight to abort running tx?
    // also sum all readonly weights to subvert to write lock with greater weight?
    Readonly(UsageCount),
    Writable,
}

impl CurrentUsage {
    fn renew(requested_usage: RequestedUsage) -> Self {
        match requested_usage {
            RequestedUsage::Readonly => CurrentUsage::Readonly(SOLE_USE_COUNT),
            RequestedUsage::Writable => CurrentUsage::Writable,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum RequestedUsage {
    Readonly,
    Writable,
}

struct Page {
    current_usage: CurrentUsage,
    contended_unique_weights: std::collections::BTreeSet<UniqueWeight>,
    //next_scheduled_task // reserved_task guaranteed_task
    //loaded account
}

type AddressBookMap = std::collections::BTreeMap<Pubkey, Page>;

// needs ttl mechanism and prune
#[derive(Default)]
pub struct AddressBook {
    book: AddressBookMap,
    newly_uncontended_addresses: std::collections::BTreeSet<Pubkey>,
}

impl AddressBook {
    #[inline(never)]
    fn attempt_lock_address(
        &mut self,
        from_runnable: bool,
        unique_weight: &UniqueWeight,
        address: Pubkey,
        requested_usage: RequestedUsage,
    ) -> LockAttempt {
        use std::collections::btree_map::Entry;

        match self.book.entry(address) {
            // unconditional success if it's initial access
            Entry::Vacant(book_entry) => {
                book_entry.insert(Page {
                    current_usage: CurrentUsage::renew(requested_usage),
                    contended_unique_weights: Default::default(),
                });
                LockAttempt::success(address, requested_usage)
            }
            Entry::Occupied(mut book_entry) => {
                let mut page = book_entry.get_mut();

                match &mut page.current_usage {
                    CurrentUsage::Unused => {
                        page.current_usage = CurrentUsage::renew(requested_usage);
                        LockAttempt::success(address, requested_usage)
                    }
                    CurrentUsage::Readonly(ref mut count) => match &requested_usage {
                        RequestedUsage::Readonly => {
                            *count += 1;
                            LockAttempt::success(address, requested_usage)
                        }
                        RequestedUsage::Writable => {
                            if from_runnable {
                                Self::remember_new_address_contention(page, unique_weight);
                            }
                            LockAttempt::failure(address, requested_usage)
                        }
                    },
                    CurrentUsage::Writable => match &requested_usage {
                        RequestedUsage::Readonly | RequestedUsage::Writable => {
                            if from_runnable {
                                Self::remember_new_address_contention(page, unique_weight);
                            }
                            LockAttempt::failure(address, requested_usage)
                        }
                    },
                }
            }
        }
    }

    #[inline(never)]
    fn remember_new_address_contention(page: &mut Page, unique_weight: &UniqueWeight) {
        page.contended_unique_weights.insert(*unique_weight);
    }

    #[inline(never)]
    fn forget_address_contention(&mut self, unique_weight: &UniqueWeight, address: &Pubkey) {
        use std::collections::btree_map::Entry;

        match self.book.entry(*address) {
            Entry::Vacant(_book_entry) => unreachable!(),
            Entry::Occupied(mut entry) => {
                let page = entry.get_mut();
                page.contended_unique_weights.remove(unique_weight);
            }
        }
    }

    fn ensure_unlock(&mut self, attempt: &LockAttempt) {
        if attempt.is_success() {
            self.unlock(attempt);
        }
    }

    #[inline(never)]
    fn unlock(&mut self, attempt: &LockAttempt) -> bool {
        debug_assert!(attempt.is_success());

        use std::collections::btree_map::Entry;
        let mut newly_uncontended = false;
        let mut still_queued = false;

        match self.book.entry(attempt.address) {
            Entry::Occupied(mut book_entry) => {
                let mut page = book_entry.get_mut();

                match &mut page.current_usage {
                    CurrentUsage::Readonly(ref mut count) => match &attempt.requested_usage {
                        RequestedUsage::Readonly => {
                            if *count == SOLE_USE_COUNT {
                                newly_uncontended = true;
                            } else {
                                *count -= 1;
                            }
                        }
                        RequestedUsage::Writable => unreachable!(),
                    },
                    CurrentUsage::Writable => match &attempt.requested_usage {
                        RequestedUsage::Writable => {
                            newly_uncontended = true;
                        }
                        RequestedUsage::Readonly => unreachable!(),
                    },
                    CurrentUsage::Unused => unreachable!(),
                }

                if newly_uncontended {
                    page.current_usage = CurrentUsage::Unused;
                    if !page.contended_unique_weights.is_empty() {
                        still_queued = true;
                    }
                }
            }
            Entry::Vacant(_book_entry) => {
                unreachable!()
            }
        }

        still_queued
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Weight {
    // naming: Sequence Ordering?
    pub ix: usize, // index in ledger entry?
                   // gas fee
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct UniqueWeight {
    // naming: Sequence Ordering?
    weight: Weight,
    // we can't use Transaction::message_hash because it's manipulatable to be favorous to the tx
    // submitter
    //unique_key: Hash, // tie breaker? random noise? also for unique identification of txes?
    // fee?
}
/*
pub type Weight = usize;
pub type UniqueWeight = usize;
*/

struct Bundle {
    // what about bundle1{tx1a, tx2} and bundle2{tx1b, tx2}?
}

#[derive(Debug)]
pub struct Task {
    pub tx: Box<SanitizedTransaction>, // actually should be Bundle
}

// RunnableQueue, ContendedQueue?
#[derive(Default)]
pub struct TaskQueue {
    tasks: std::collections::BTreeMap<UniqueWeight, Task>,
}

impl TaskQueue {
    #[inline(never)]
    fn add_to_schedule(&mut self, unique_weight: UniqueWeight, task: Task) {
        //trace!("TaskQueue::add(): {:?}", unique_weight);
        let pre_existed = self.tasks.insert(unique_weight, task);
        debug_assert!(pre_existed.is_none()); //, "identical shouldn't exist: {:?}", unique_weight);
    }

    #[inline(never)]
    fn entry_to_execute(
        &mut self,
        unique_weight: UniqueWeight,
    ) -> std::collections::btree_map::OccupiedEntry<'_, UniqueWeight, Task> {
        use std::collections::btree_map::Entry;

        let queue_entry = self.tasks.entry(unique_weight);
        match queue_entry {
            Entry::Occupied(queue_entry) => queue_entry,
            Entry::Vacant(_queue_entry) => unreachable!(),
        }
    }

    #[inline(never)]
    fn heaviest_entry_to_execute(
        &mut self,
    ) -> Option<std::collections::btree_map::OccupiedEntry<'_, UniqueWeight, Task>> {
        self.tasks.last_entry()
    }
}

#[inline(never)]
fn attempt_lock_for_execution<'a>(
    from_runnable: bool,
    address_book: &mut AddressBook,
    unique_weight: &UniqueWeight,
    message_hash: &'a Hash,
    locks: &'a TransactionAccountLocks,
) -> (bool, Vec<LockAttempt>) {
    // no short-cuircuit; we at least all need to add to the contended queue
    let writable_lock_iter = locks
        .writable
        .iter()
        .map(|address| (address, RequestedUsage::Writable));
    let readonly_lock_iter = locks
        .readonly
        .iter()
        .map(|address| (address, RequestedUsage::Readonly));
    let chained_iter = writable_lock_iter.chain(readonly_lock_iter);

    let mut all_succeeded_so_far = true;
    let lock_attempts = chained_iter
        .map(|(&&address, usage)| {
            let attempt =
                address_book.attempt_lock_address(from_runnable, unique_weight, address, usage);
            if all_succeeded_so_far && attempt.is_failed() {
                all_succeeded_so_far = false;
            }
            attempt
        })
        .collect::<Vec<_>>();

    (all_succeeded_so_far, lock_attempts)
}

pub enum Incoming {
    FromPrevious((Weight, Box<SanitizedTransaction>)),
    FromExecute(Box<ExecutionEnvironment>),
}

pub struct ScheduleStage {}

impl ScheduleStage {
    fn push_to_queue(
        (weight, tx): (Weight, Box<SanitizedTransaction>),
        runnable_queue: &mut TaskQueue,
    ) {
        // manage randomness properly for future scheduling determinism
        //let mut rng = rand::thread_rng();

        //let ix = 23;
        //let tx = bank
        //    .verify_transaction(
        //        tx,
        //        solana_sdk::transaction::TransactionVerificationMode::FullVerification,
        //    )
        //    .unwrap();
        //tx.foo();

        runnable_queue.add_to_schedule(
            UniqueWeight {
                weight,
                //unique_key: solana_sdk::hash::new_rand(&mut rng),
            },
            Task { tx },
        );
    }

    #[inline(never)]
    fn get_newly_u_u_w<'a>(
        address: &'a Pubkey,
        address_book: &'a AddressBook,
    ) -> &'a std::collections::BTreeSet<UniqueWeight> {
        &address_book
            .book
            .get(address)
            .unwrap()
            .contended_unique_weights
    }

    #[inline(never)]
    fn get_weight_from_contended(address_book: &AddressBook) -> Option<UniqueWeight> {
        let mut heaviest_weight: Option<UniqueWeight> = None;
        //trace!("n u a len(): {}", address_book.newly_uncontended_addresses.len());
        for address in address_book.newly_uncontended_addresses.iter() {
            let newly_uncontended_unique_weights = Self::get_newly_u_u_w(address, &address_book);
            if let Some(&weight) = newly_uncontended_unique_weights.last() {
                if let Some(current_heaviest_weight) = heaviest_weight {
                    if weight > current_heaviest_weight {
                        heaviest_weight = Some(weight);
                    }
                } else {
                    heaviest_weight = Some(weight);
                }
            }
        }
        heaviest_weight
    }

    #[inline(never)]
    fn select_next_task<'a>(
        runnable_queue: &'a mut TaskQueue,
        contended_queue: &'a mut TaskQueue,
        address_book: &mut AddressBook,
    ) -> Option<(
        Option<&'a mut TaskQueue>,
        std::collections::btree_map::OccupiedEntry<'a, UniqueWeight, Task>,
    )> {
        match (
            runnable_queue.heaviest_entry_to_execute(),
            Self::get_weight_from_contended(address_book),
        ) {
            (Some(heaviest_runnable_entry), None) => {
                Some((Some(contended_queue), heaviest_runnable_entry))
            }
            (None, Some(weight_from_contended)) => {
                Some((None, contended_queue.entry_to_execute(weight_from_contended)))
            }
            (Some(heaviest_runnable_entry), Some(weight_from_contended)) => {
                let weight_from_runnable = heaviest_runnable_entry.key();

                if weight_from_runnable > &weight_from_contended {
                    Some((Some(contended_queue), heaviest_runnable_entry))
                } else if &weight_from_contended > weight_from_runnable {
                    Some((None, contended_queue.entry_to_execute(weight_from_contended)))
                } else {
                    unreachable!(
                        "identical unique weights shouldn't exist in both runnable and contended"
                    )
                }
            }
            (None, None) => None,
        }
    }

    #[inline(never)]
    fn pop_from_queue_then_lock(
        runnable_queue: &mut TaskQueue,
        contended_queue: &mut TaskQueue,
        address_book: &mut AddressBook,
    ) -> Option<(UniqueWeight, Task, Vec<LockAttempt>)> {
        for (reborrowed_contended_queue, query_entry) in
            Self::select_next_task(runnable_queue, contended_queue, address_book)
        {
            let from_runnable = reborrowed_contended_queue.is_some();
            let next_task = query_entry.get();
            let message_hash = next_task.tx.message_hash();
            let locks = next_task.tx.get_account_locks().unwrap();

            // plumb message_hash into StatusCache or implmenent our own for duplicate tx
            // detection?

            let (is_success, lock_attempts) = attempt_lock_for_execution(
                from_runnable,
                address_book,
                &query_entry.key(),
                &message_hash,
                &locks,
            );

            if !is_success {
                //trace!("ensure_unlock_for_failed_execution(): {:?} {}", (&unique_weight, from_runnable), next_task.tx.signature());
                Self::ensure_unlock_for_failed_execution(
                    address_book,
                    lock_attempts,
                    from_runnable,
                );
                if from_runnable {
                    reborrowed_contended_queue
                        .unwrap()
                        .add_to_schedule(*query_entry.key(), query_entry.remove());
                }
                continue;
            }

            return Some((*query_entry.key(), query_entry.remove(), lock_attempts));
        }

        None
    }

    #[inline(never)]
    fn apply_successful_lock_before_execution(
        address_book: &mut AddressBook,
        unique_weight: UniqueWeight,
        lock_attempts: &Vec<LockAttempt>,
    ) {
        for l in lock_attempts {
            // ensure to remove remaining refs of this unique_weight
            address_book.forget_address_contention(&unique_weight, &l.address);

            // revert because now contended again
            address_book.newly_uncontended_addresses.remove(&l.address);
        }
    }

    #[inline(never)]
    fn ensure_unlock_for_failed_execution(
        address_book: &mut AddressBook,
        lock_attempts: Vec<LockAttempt>,
        from_runnable: bool,
    ) {
        for l in lock_attempts {
            address_book.ensure_unlock(&l);

            // revert because now contended again
            if !from_runnable {
                address_book.newly_uncontended_addresses.remove(&l.address);
            }

            // todo: mem::forget and panic in LockAttempt::drop()
        }
    }

    #[inline(never)]
    fn unlock_after_execution(address_book: &mut AddressBook, lock_attempts: Vec<LockAttempt>) {
        for l in lock_attempts {
            let newly_uncontended_while_queued = address_book.unlock(&l);
            if newly_uncontended_while_queued {
                address_book.newly_uncontended_addresses.insert(l.address);
            }

            // todo: mem::forget and panic in LockAttempt::drop()
        }
    }

    #[inline(never)]
    fn prepare_scheduled_execution(
        address_book: &mut AddressBook,
        unique_weight: UniqueWeight,
        task: Task,
        lock_attempts: Vec<LockAttempt>,
    ) -> Box<ExecutionEnvironment> {
        let mut rng = rand::thread_rng();
        // relock_before_execution() / update_address_book() / update_uncontended_addresses()?
        Self::apply_successful_lock_before_execution(address_book, unique_weight, &lock_attempts);
        // load account now from AccountsDb

        Box::new(ExecutionEnvironment {
            lock_attempts,
            task,
            cu: rng.gen_range(3, 1000),
        })
    }

    #[inline(never)]
    fn commit_result(ee: &mut ExecutionEnvironment, address_book: &mut AddressBook) {
        let lock_attempts = std::mem::take(&mut ee.lock_attempts);
        // do par()-ly?
        Self::unlock_after_execution(address_book, lock_attempts);

        // par()-ly clone updated Accounts into address book
    }

    #[inline(never)]
    fn schedule_next_execution(
        runnable_queue: &mut TaskQueue,
        contended_queue: &mut TaskQueue,
        address_book: &mut AddressBook,
    ) -> Option<Box<ExecutionEnvironment>> {
        let maybe_ee =
            Self::pop_from_queue_then_lock(runnable_queue, contended_queue, address_book)
                .map(|(uw, t, ll)| Self::prepare_scheduled_execution(address_book, uw, t, ll));
        maybe_ee
    }

    #[inline(never)]
    fn register_runnable_task(
        weighted_tx: (Weight, Box<SanitizedTransaction>),
        runnable_queue: &mut TaskQueue,
    ) {
        Self::push_to_queue(weighted_tx, runnable_queue)
    }

    pub fn run(
        max_depth: usize,
        runnable_queue: &mut TaskQueue,
        contended_queue: &mut TaskQueue,
        address_book: &mut AddressBook,
        from: &crossbeam_channel::Receiver<Incoming>,
        to_execute_substage: &crossbeam_channel::Sender<Box<ExecutionEnvironment>>,
        to_next_stage: &crossbeam_channel::Sender<Box<ExecutionEnvironment>>, // assume nonblocking
    ) {
        use crossbeam_channel::select;

        let mut depth = 0;

        loop {
            trace!("schedule_once!");

            let i = from.recv().unwrap();
            match i {
                Incoming::FromPrevious(weighted_tx) => {
                    trace!("recv from previous");

                    Self::register_runnable_task(weighted_tx, runnable_queue);
                }
                Incoming::FromExecute(mut processed_execution_environment) => {
                    trace!("recv from execute");
                    depth -= 1;

                    Self::commit_result(&mut processed_execution_environment, address_book);
                    // async-ly propagate the result to rpc subsystems
                    // to_next_stage is assumed to be non-blocking so, doesn't need to be one of select! handlers
                    to_next_stage.send(processed_execution_environment).unwrap();
                }
            }

            while depth < max_depth {
                let maybe_ee =
                    Self::schedule_next_execution(runnable_queue, contended_queue, address_book);
                if let Some(ee) = maybe_ee {
                    trace!("send to execute");
                    depth += 1;

                    to_execute_substage.send(ee).unwrap();
                } else {
                    break;
                }
            }
        }
    }
}

struct ExecuteStage {
    //bank: Bank,
}

impl ExecuteStage {}
