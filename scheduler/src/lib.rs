#![feature(map_first_last)]
#![feature(negative_impls)]
//#![feature(get_mut_unchecked)]

use {
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

/*
type MyRcInner<T> = std::rc::Rc<T>;
unsafe impl Send for PageRc {}
*/

type MyRcInner = std::sync::Arc<std::cell::RefCell<Page>>;

#[derive(Debug, Clone)]
pub struct PageRc(MyRcInner);
unsafe impl Sync for PageRc {}


#[derive(Debug)]
pub struct ExecutionEnvironment {
    //accounts: Vec<i8>,
    pub cu: usize,
    pub unique_weight: UniqueWeight,
    pub task: TaskInQueue,
    pub lock_attempts: Vec<LockAttempt>,
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
    //
    #[inline(never)]
    pub fn reindex(&mut self) {
        let uq = self.unique_weight;
        //self.task.trace_timestamps("in_exec(self)");
        let should_remove = self.task.contention_count > 0;
        for mut lock_attempt in self.lock_attempts.iter_mut() {
            let contended_unique_weights = lock_attempt.contended_unique_weights();
            contended_unique_weights.heaviest_task_cursor().map(|mut task_cursor| {
                let mut found = true;
                let mut removed = false;
                let mut task = task_cursor.value();
                //task.trace_timestamps("in_exec(initial list)");
                while !task.currently_contended() {
                    if task_cursor.key() == &uq {
                        assert!(should_remove);
                        removed = task_cursor.remove();
                        assert!(removed);
                    }
                    if task.already_finished() {
                        task_cursor.remove();
                    }
                    if let Some(new_cursor) = task_cursor.prev() {
                        assert!(new_cursor.key() < task_cursor.key());
                        task_cursor = new_cursor;
                        task = task_cursor.value();
                        //task.trace_timestamps("in_exec(subsequent list)");
                    } else {
                        found = false;
                        break;
                    }
                }
                if should_remove && !removed {
                    contended_unique_weights.remove_task(&uq);
                }
                found.then(|| TaskInQueue::clone(task))
            }).flatten().map(|task| {
                //task.trace_timestamps(&format!("in_exec(heaviest:{})", self.task.queue_time_label()));
                lock_attempt.heaviest_uncontended = Some(task);
                ()
            });
        }
    }
}

unsafe trait AtScheduleThread: Copy {}

impl PageRc {
    fn page_mut<AST: AtScheduleThread>(&self, _ast: AST) -> std::cell::RefMut<'_, Page> {
        //unsafe { MyRcInner::get_mut_unchecked(&mut self.0) }
        self.0.borrow_mut()
    }

    fn page_ref(&self) -> std::cell::Ref<'_, Page> {
        //<MyRcInner as std::borrow::Borrow<_>>::borrow(&self.0)
        self.0.borrow()
        //&*(self.0)
    }
}

#[derive(Clone, Debug)]
enum LockStatus {
    Succeded,
    Provisional, 
    Failed,
}

#[derive(Debug)]
pub struct LockAttempt {
    target: PageRc,
    status: LockStatus,
    requested_usage: RequestedUsage,
    //pub heaviest_uncontended: arc_swap::ArcSwapOption<Task>,
    pub heaviest_uncontended: Option<TaskInQueue>,
    //remembered: bool,
}

impl LockAttempt {
    pub fn new(target: PageRc, requested_usage: RequestedUsage) -> Self {
        Self {
            target,
            status: LockStatus::Succeded,
            requested_usage,
            heaviest_uncontended: Default::default(),
            //remembered: false,
        }
    }

    pub fn clone_for_test(&self) -> Self {
        Self {
            target: self.target.clone(),
            status: LockStatus::Succeded,
            requested_usage: self.requested_usage,
            heaviest_uncontended: Default::default(),
            //remembered: false,
        }
    }

    pub fn contended_unique_weights(&self) -> &TaskIds {
        //&self.target.page_ref().contended_unique_weights
        panic!()
    }
}

type UsageCount = usize;
const SOLE_USE_COUNT: UsageCount = 1;

#[derive(Copy, Clone, Debug, PartialEq)]
enum Usage {
    Unused,
    // weight to abort running tx?
    // also sum all readonly weights to subvert to write lock with greater weight?
    Readonly(UsageCount),
    Writable,
}

impl Usage {
    fn renew(requested_usage: RequestedUsage) -> Self {
        match requested_usage {
            RequestedUsage::Readonly => Usage::Readonly(SOLE_USE_COUNT),
            RequestedUsage::Writable => Usage::Writable,
        }
    }

    fn unused() -> Self {
        Usage::Unused
    }
}

#[derive(Clone, Copy, Debug)]
pub enum RequestedUsage {
    Readonly,
    Writable,
}

#[derive(Debug, Default)]
pub struct TaskIds {
    task_ids: crossbeam_skiplist::SkipMap<UniqueWeight, TaskInQueue>,
}

impl TaskIds {
    #[inline(never)]
    pub fn insert_task(&self, u: TaskId, task: TaskInQueue) {
        let mut is_inserted = false;
        self.task_ids.get_or_insert_with(u, || {
            is_inserted = true;
            task
        });
        assert!(is_inserted);
    }

    #[inline(never)]
    pub fn remove_task(&self, u: &TaskId) {
        let removed_entry = self.task_ids.remove(u);
        //assert!(removed_entry.is_some());
    }

    #[inline(never)]
    pub fn heaviest_task_cursor(&self) -> Option<crossbeam_skiplist::map::Entry<'_, UniqueWeight, TaskInQueue>> {
        self.task_ids.back()
    }
}

#[derive(Debug)]
pub struct Page {
    current_usage: Usage,
    next_usage: Usage,
    contended_unique_weights: TaskIds,
    provisional_task_ids: Vec<std::sync::Arc<ProvisioningTracker>>,
    //loaded account from Accounts db
    //comulative_cu for qos; i.e. track serialized cumulative keyed by addresses and bail out block
    //producing as soon as any one of cu from the executing thread reaches to the limit
}

impl Page {
    fn new(current_usage: Usage) -> Self {
        Self {
            current_usage,
            next_usage: Usage::Unused,
            contended_unique_weights: Default::default(),
            provisional_task_ids: Default::default(),
        }
    }

    fn switch_to_next_usage(&mut self) {
        self.current_usage = self.next_usage;
        self.next_usage = Usage::Unused;
    }
}

//type AddressMap = std::collections::HashMap<Pubkey, PageRc>;
type AddressMap = std::sync::Arc<dashmap::DashMap<Pubkey, PageRc>>;
type TaskId = UniqueWeight;
type WeightedTaskIds = std::collections::BTreeMap<TaskId, TaskInQueue>;
//type AddressMapEntry<'a, K, V> = std::collections::hash_map::Entry<'a, K, V>;
type AddressMapEntry<'a> = dashmap::mapref::entry::Entry<'a, Pubkey, PageRc>;

// needs ttl mechanism and prune
#[derive(Default)]
pub struct AddressBook {
    book: AddressMap,
    uncontended_task_ids: WeightedTaskIds,
    fulfilled_provisional_task_ids: WeightedTaskIds,
}

#[derive(Debug)]
struct ProvisioningTracker {
    remaining_count: usize,
    task: TaskInQueue,
}

impl ProvisioningTracker {
    fn new(remaining_count: usize, task: TaskInQueue) -> Self {
        Self { remaining_count, task }
    }

    fn is_fulfilled(&self) -> bool {
        self.remaining_count == 0
    }

    fn progress(&mut self) {
        self.remaining_count = self.remaining_count.checked_sub(1).unwrap();
    }

    fn prev_count(&self) -> usize {
        self.remaining_count + 1
    }

    fn count(&self) -> usize {
        self.remaining_count
    }
}

impl AddressBook {
    #[inline(never)]
    fn attempt_lock_address<AST: AtScheduleThread>(
        ast: AST,
        from_runnable: bool,
        prefer_immediate: bool,
        unique_weight: &UniqueWeight,
        attempt: &mut LockAttempt,
    ) {
        let LockAttempt {target, requested_usage, status/*, remembered*/, ..} = attempt;

                let mut page = target.page_mut(ast);

                let next_usage = page.next_usage;
                match page.current_usage {
                    Usage::Unused => {
                        assert_eq!(page.next_usage, Usage::Unused);
                        page.current_usage = Usage::renew(*requested_usage);
                        *status = LockStatus::Succeded;
                    }
                    Usage::Readonly(ref mut count) => match requested_usage {
                        RequestedUsage::Readonly => {
                            // prevent newer read-locks (even from runnable too)
                            match next_usage {
                                Usage::Unused => {
                                    *count += 1;
                                    *status = LockStatus::Succeded;
                                },
                                Usage::Readonly(_) | Usage::Writable => {
                                    *status = LockStatus::Failed;
                                }
                            }
                        }
                        RequestedUsage::Writable => {
                            if from_runnable || prefer_immediate {
                                *status = LockStatus::Failed;
                            } else {
                                match page.next_usage {
                                    Usage::Unused => {
                                        *status = LockStatus::Provisional;
                                        page.next_usage = Usage::renew(*requested_usage);
                                    },
                                    // support multiple readonly locks!
                                    Usage::Readonly(_) | Usage::Writable => {
                                        *status = LockStatus::Failed;
                                    },
                                }
                            }
                        }
                    },
                    Usage::Writable => {
                        if from_runnable || prefer_immediate {
                            *status = LockStatus::Failed;
                        } else {
                            match page.next_usage {
                                Usage::Unused => {
                                    *status = LockStatus::Provisional;
                                    page.next_usage = Usage::renew(*requested_usage);
                                },
                                // support multiple readonly locks!
                                Usage::Readonly(_) | Usage::Writable => {
                                    *status = LockStatus::Failed;
                                },
                            }
                        }
                    },
                }
    }

    fn reset_lock<AST: AtScheduleThread>(&mut self, ast: AST, attempt: &mut LockAttempt, after_execution: bool) -> bool {
        match attempt.status {
            LockStatus::Succeded => {
                self.unlock(ast, attempt)
            },
            LockStatus::Provisional => {
                if after_execution {
                    self.unlock(ast, attempt)
                } else {
                    self.cancel(ast, attempt);
                    false
                }
            }
            LockStatus::Failed => {
                false // do nothing
            }
        }
    }

    #[inline(never)]
    fn unlock<AST: AtScheduleThread>(&mut self, ast: AST, attempt: &mut LockAttempt) -> bool {
        //debug_assert!(attempt.is_success());

        let mut newly_uncontended = false;

        let mut page = attempt.target.page_mut(ast);

        match &mut page.current_usage {
            Usage::Readonly(ref mut count) => match &attempt.requested_usage {
                RequestedUsage::Readonly => {
                    if *count == SOLE_USE_COUNT {
                        newly_uncontended = true;
                    } else {
                        *count -= 1;
                    }
                }
                RequestedUsage::Writable => unreachable!(),
            },
            Usage::Writable => match &attempt.requested_usage {
                RequestedUsage::Writable => {
                    newly_uncontended = true;
                }
                RequestedUsage::Readonly => unreachable!(),
            },
            Usage::Unused => unreachable!(),
        }

        if newly_uncontended {
            page.current_usage = Usage::Unused;
        }

        newly_uncontended
    }

    #[inline(never)]
    fn cancel<AST: AtScheduleThread>(&mut self, ast: AST, attempt: &mut LockAttempt) {
        let mut page = attempt.target.page_mut(ast);

        match page.next_usage {
            Usage::Unused => {
                unreachable!();
            },
            // support multiple readonly locks!
            Usage::Readonly(_) | Usage::Writable => {
                page.next_usage = Usage::Unused;
            },
        }
    }

    pub fn preloader(&self) -> Preloader {
        Preloader{book: std::sync::Arc::clone(&self.book)}
    }
}

pub struct Preloader {
    book: AddressMap,
}

impl Preloader {
    #[inline(never)]
    pub fn load(&self, address: Pubkey) -> PageRc {
        PageRc::clone(&self.book.entry(address).or_insert_with(|| PageRc(MyRcInner::new(core::cell::RefCell::new(Page::new(Usage::unused()))))))
    }
}

/*
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Weight {
    // naming: Sequence Ordering?
    pub ix: u64, // index in ledger entry?
                   // gas fee
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct UniqueWeight {
    // naming: Sequence Ordering?
    weight: Weight,
    // we can't use Transaction::message_hash because it's manipulatable to be favorous to the tx
    // submitter
    //unique_key: Hash, // tie breaker? random noise? also for unique identification of txes?
    unique_key: u64, // tie breaker? random noise? also for unique identification of txes?
    // fee?
}
*/
pub type Weight = u64;
pub type UniqueWeight = u64;

struct Bundle {
    // what about bundle1{tx1a, tx2} and bundle2{tx1b, tx2}?
}

#[derive(Debug)]
pub struct Task {
    unique_weight: UniqueWeight,
    pub tx: (SanitizedTransaction, Vec<LockAttempt>), // actually should be Bundle
    pub contention_count: usize,
    pub uncontended: std::sync::atomic::AtomicUsize,
    pub sequence_time: std::sync::atomic::AtomicUsize,
    pub sequence_end_time: std::sync::atomic::AtomicUsize,
    pub queue_time: std::sync::atomic::AtomicUsize,
    pub queue_end_time: std::sync::atomic::AtomicUsize,
    pub execute_time: std::sync::atomic::AtomicUsize,
    pub commit_time: std::sync::atomic::AtomicUsize,
    pub for_indexer: Vec<LockAttempt>,
}

// sequence_time -> seq clock
// queue_time -> queue clock
// execute_time ---> exec clock
// commit_time  -+

impl Task {
    pub fn new_for_queue(unique_weight: UniqueWeight, tx: (SanitizedTransaction, Vec<LockAttempt>)) -> std::sync::Arc<Self> {
        TaskInQueue::new(Self {
            for_indexer: tx.1.iter().map(|a| a.clone_for_test()).collect(),
            unique_weight,
            tx,
            contention_count: 0,
            uncontended: Default::default(),
            sequence_time: std::sync::atomic::AtomicUsize::new(usize::max_value()),
            sequence_end_time: std::sync::atomic::AtomicUsize::new(usize::max_value()),
            queue_time: std::sync::atomic::AtomicUsize::new(usize::max_value()),
            queue_end_time: std::sync::atomic::AtomicUsize::new(usize::max_value()),
            execute_time: std::sync::atomic::AtomicUsize::new(usize::max_value()),
            commit_time: std::sync::atomic::AtomicUsize::new(usize::max_value())
        })
    }

    pub fn record_sequence_time(&self, clock: usize) {
        //self.sequence_time.store(clock, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn sequence_time(&self) -> usize {
        self.sequence_time.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn sequence_end_time(&self) -> usize {
        self.sequence_end_time.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn record_queue_time(&self, seq_clock: usize, queue_clock: usize) {
        //self.sequence_end_time.store(seq_clock, std::sync::atomic::Ordering::SeqCst);
        //self.queue_time.store(queue_clock, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn queue_time(&self) -> usize {
        self.queue_time.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn queue_end_time(&self) -> usize {
        self.queue_end_time.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn record_execute_time(&self, queue_clock: usize, execute_clock: usize) {
        //self.queue_end_time.store(queue_clock, std::sync::atomic::Ordering::SeqCst);
        //self.execute_time.store(execute_clock, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn execute_time(&self) -> usize {
        self.execute_time.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn record_commit_time(&self, execute_clock: usize) {
        //self.commit_time.store(execute_clock, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn commit_time(&self) -> usize {
        self.commit_time.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn queue_time_label(&self) -> String {
        format!("queue: [{}qT..{}qT; {}qD]", 
              self.queue_time(), self.queue_end_time(), self.queue_end_time() - self.queue_time(), 
              )
    }

    pub fn trace_timestamps(&self, prefix: &str) {
        trace!("{}: {:016x} seq: [{}sT..{}sT; {}sD], queue: [{}qT..{}qT; {}qD] exec: [{}eT..{}eT; {}eD]", 
              prefix,
              self.unique_weight,
              self.sequence_time(),
              self.sequence_end_time(),
              self.sequence_end_time() - self.sequence_time(),
              self.queue_time(), self.queue_end_time(), self.queue_end_time() - self.queue_time(), 
              self.execute_time(), self.commit_time(), self.commit_time() - self.execute_time());
    }


    pub fn clone_for_test(&self) -> Self {
        Self {
            unique_weight: self.unique_weight,
            for_indexer: self.tx.1.iter().map(|a| a.clone_for_test()).collect(),
            tx: (self.tx.0.clone(), self.tx.1.iter().map(|l| l.clone_for_test()).collect::<Vec<_>>()),
            contention_count: Default::default(),
            uncontended: Default::default(),
            sequence_time: std::sync::atomic::AtomicUsize::new(usize::max_value()),
            sequence_end_time: std::sync::atomic::AtomicUsize::new(usize::max_value()),
            queue_time: std::sync::atomic::AtomicUsize::new(usize::max_value()),
            queue_end_time: std::sync::atomic::AtomicUsize::new(usize::max_value()),
            execute_time: std::sync::atomic::AtomicUsize::new(usize::max_value()),
            commit_time: std::sync::atomic::AtomicUsize::new(usize::max_value()),
        }
    }

    pub fn currently_contended(&self) -> bool {
        self.uncontended.load(std::sync::atomic::Ordering::SeqCst) == 1
    }

    pub fn already_finished(&self) -> bool {
        self.uncontended.load(std::sync::atomic::Ordering::SeqCst) == 3
    }

    fn mark_as_contended(&self) {
        self.uncontended.store(1, std::sync::atomic::Ordering::SeqCst)
    }

    fn mark_as_uncontended(&self) {
        assert!(self.currently_contended());
        self.uncontended.store(2, std::sync::atomic::Ordering::SeqCst)
    }

    fn mark_as_finished(&self) {
        assert!(!self.already_finished() && !self.currently_contended());
        self.uncontended.store(3, std::sync::atomic::Ordering::SeqCst)
    }
}

// RunnableQueue, ContendedQueue?
#[derive(Default, Debug, Clone)]
pub struct TaskQueue {
    tasks: std::collections::BTreeMap<UniqueWeight, TaskInQueue>,
    //tasks: im::OrdMap<UniqueWeight, TaskInQueue>,
    //tasks: im::HashMap<UniqueWeight, TaskInQueue>,
    //tasks: std::sync::Arc<dashmap::DashMap<UniqueWeight, TaskInQueue>>,
}

pub type TaskInQueue = std::sync::Arc<Task>;
//type TaskQueueEntry<'a> = im::ordmap::Entry<'a, UniqueWeight, TaskInQueue>;
//type TaskQueueOccupiedEntry<'a> = im::ordmap::OccupiedEntry<'a, UniqueWeight, TaskInQueue>;
//type TaskQueueEntry<'a> = im::hashmap::Entry<'a, UniqueWeight, TaskInQueue, std::collections::hash_map::RandomState>;
//type TaskQueueOccupiedEntry<'a> = im::hashmap::OccupiedEntry<'a, UniqueWeight, TaskInQueue, std::collections::hash_map::RandomState>;
//type TaskQueueEntry<'a> = dashmap::mapref::entry::Entry<'a, UniqueWeight, TaskInQueue>;
//type TaskQueueOccupiedEntry<'a> = dashmap::mapref::entry::OccupiedEntry<'a, UniqueWeight, TaskInQueue, std::collections::hash_map::RandomState>;
type TaskQueueEntry<'a> = std::collections::btree_map::Entry<'a, UniqueWeight, TaskInQueue>;
type TaskQueueOccupiedEntry<'a> = std::collections::btree_map::OccupiedEntry<'a, UniqueWeight, TaskInQueue>;

impl TaskQueue {
    #[inline(never)]
    fn add_to_schedule(&mut self, unique_weight: UniqueWeight, task: TaskInQueue) {
        //trace!("TaskQueue::add(): {:?}", unique_weight);
        let pre_existed = self.tasks.insert(unique_weight, task);
        assert!(pre_existed.is_none()); //, "identical shouldn't exist: {:?}", unique_weight);
    }

    #[inline(never)]
    fn heaviest_entry_to_execute(
        &mut self,
    ) -> Option<TaskQueueOccupiedEntry<'_>> {
        self.tasks.last_entry()
    }

    fn task_count(&self) -> usize {
        self.tasks.len()
    }
}

#[inline(never)]
fn attempt_lock_for_execution<'a, AST: AtScheduleThread>(
    ast: AST,
    from_runnable: bool,
    prefer_immediate: bool,
    address_book: &mut AddressBook,
    unique_weight: &UniqueWeight,
    message_hash: &'a Hash,
    placeholder_attempts: &mut Vec<LockAttempt>,
) -> (usize, usize) {
    // no short-cuircuit; we at least all need to add to the contended queue
    let mut unlockable_count = 0;
    let mut provisional_count = 0;

    for attempt in placeholder_attempts.iter_mut() {
        AddressBook::attempt_lock_address(ast, from_runnable, prefer_immediate, unique_weight, attempt);
        match attempt.status {
            LockStatus::Succeded => {},
            LockStatus::Failed => {
                unlockable_count += 1;
            },
            LockStatus::Provisional => {
                provisional_count += 1;
            },
        }
    }

    (unlockable_count, provisional_count)
}

type PreprocessedTransaction = (SanitizedTransaction, Vec<LockAttempt>);

pub fn get_transaction_priority_details(tx: &SanitizedTransaction) -> u64 {
        use solana_program_runtime::compute_budget::ComputeBudget;
        let mut compute_budget = ComputeBudget::default();
        compute_budget
            .process_instructions(
                tx.message().program_instructions_iter(),
                true, // use default units per instruction
                true, // don't reject txs that use set compute unit price ix
            )
            .map(|d| d.get_priority()).unwrap_or_default()
}

pub struct ScheduleStage {}

impl ScheduleStage {
    fn push_to_runnable_queue(
        task: TaskInQueue,
        runnable_queue: &mut TaskQueue,
    ) {
        runnable_queue.add_to_schedule(
            task.unique_weight,
            task,
        );
    }

    #[inline(never)]
    fn get_heaviest_from_contended<'a>(address_book: &'a mut AddressBook) -> Option<std::collections::btree_map::OccupiedEntry<'a, UniqueWeight, TaskInQueue>> {
        address_book.uncontended_task_ids.last_entry()
    }

    #[inline(never)]
    fn select_next_task<'a>(
        runnable_queue: &'a mut TaskQueue,
        address_book: &mut AddressBook,
    ) -> Option<(
        bool,
        TaskInQueue,
    )> {
        match (
            runnable_queue.heaviest_entry_to_execute(),
            Self::get_heaviest_from_contended(address_book),
        ) {
            (Some(heaviest_runnable_entry), None) => {
                trace!("select: runnable only");
                let t = heaviest_runnable_entry.remove();
                return Some((true, t))
            }
            (None, Some(weight_from_contended)) => {
                trace!("select: contended only");
                let t = weight_from_contended.remove();
                return Some(( false, t))
            },
            (Some(heaviest_runnable_entry), Some(weight_from_contended)) => {
                let weight_from_runnable = heaviest_runnable_entry.key();
                let uw = weight_from_contended.key();

                if weight_from_runnable > uw {
                    trace!("select: runnable > contended");
                    let t = heaviest_runnable_entry.remove();
                    return Some((true, t))
                } else if uw > weight_from_runnable {
                    trace!("select: contended > runnnable");
                    let t = weight_from_contended.remove();
                    return Some(( false, t))
                } else {
                    unreachable!(
                        "identical unique weights shouldn't exist in both runnable and contended"
                    )
                }
            }
            (None, None) => {
                trace!("select: none");
                return None
            }
        }
    }

    #[inline(never)]
    fn pop_from_queue_then_lock<AST: AtScheduleThread>(
        ast: AST,
        task_sender: &crossbeam_channel::Sender<(TaskInQueue, Vec<LockAttempt>)>,
        runnable_queue: &mut TaskQueue,
        address_book: &mut AddressBook,
        contended_count: &mut usize,
        prefer_immediate: bool,
        sequence_clock: &usize,
        queue_clock: &mut usize,
        provisioning_tracker_count: &mut usize,
    ) -> Option<(UniqueWeight, TaskInQueue, Vec<LockAttempt>)> {
        if let Some(mut a) = address_book.fulfilled_provisional_task_ids.pop_last() {
            trace!("expediate pop from provisional queue [rest: {}]", address_book.fulfilled_provisional_task_ids.len());
            let next_task: &mut TaskInQueue = unsafe { panic!(); /*TaskInQueue::get_mut_unchecked(&mut a.1)*/ };

            let lock_attempts = std::mem::take(&mut next_task.tx.1);

            return Some((a.0, a.1, lock_attempts));
        }

        trace!("pop begin");
        loop {
        if let Some((from_runnable, mut arc_next_task)) = Self::select_next_task(runnable_queue, address_book) {
            trace!("pop loop iteration");
            if from_runnable {
                arc_next_task.record_queue_time(*sequence_clock, *queue_clock);
                *queue_clock = queue_clock.checked_add(1).unwrap();
            }
            let a2 = TaskInQueue::clone(&arc_next_task);
            let next_task: &mut TaskInQueue = unsafe { panic!(); /*TaskInQueue::get_mut_unchecked(&mut arc_next_task)*/ };
            let unique_weight = next_task.unique_weight;
            let message_hash = next_task.tx.0.message_hash();

            // plumb message_hash into StatusCache or implmenent our own for duplicate tx
            // detection?

            let (unlockable_count, provisional_count) = attempt_lock_for_execution(
                ast,
                from_runnable,
                prefer_immediate,
                address_book,
                &unique_weight,
                &message_hash,
                &mut next_task.tx.1,
            );

            if unlockable_count > 0 {
                //trace!("reset_lock_for_failed_execution(): {:?} {}", (&unique_weight, from_runnable), next_task.tx.0.signature());
                Self::reset_lock_for_failed_execution(
                    ast,
                    address_book,
                    &unique_weight,
                    &mut next_task.tx.1,
                    from_runnable,
                );
                let lock_count = next_task.tx.1.len();
                next_task.contention_count += 1;

                if from_runnable {
                    trace!("move to contended due to lock failure [{}/{}/{}]", unlockable_count, provisional_count, lock_count);
                    next_task.mark_as_contended();
                    *contended_count = contended_count.checked_add(1).unwrap();
                    //for lock_attempt in next_task.tx.1.iter() {
                    //    lock_attempt.contended_unique_weights().insert_task(unique_weight, TaskInQueue::clone(&a2));
                    //}
                    task_sender.send((TaskInQueue::clone(&a2), std::mem::take(&mut next_task.for_indexer))).unwrap();

                    // maybe run lightweight prune logic on contended_queue here.
                } else {
                    trace!("relock failed [{}/{}/{}]; remains in contended: {:?} contention: {}", unlockable_count, provisional_count, lock_count, &unique_weight, next_task.contention_count);
                    //address_book.uncontended_task_ids.clear();
                }

                if from_runnable {
                    continue;
                } else {
                    return None;
                }
            } else if provisional_count > 0 {
                assert!(!from_runnable);
                assert_eq!(unlockable_count, 0);
                let lock_count = next_task.tx.1.len();
                trace!("provisional exec: [{}/{}]", provisional_count, lock_count);
                *contended_count = contended_count.checked_sub(1).unwrap();
                next_task.mark_as_uncontended();
                let tracker = std::sync::Arc::new(ProvisioningTracker::new(provisional_count, TaskInQueue::clone(&a2)));
                *provisioning_tracker_count = provisioning_tracker_count.checked_add(1).unwrap();
                Self::finalize_lock_for_provisional_execution(
                    ast,
                    address_book,
                    next_task,
                    tracker
                );

                return None;
                continue;
            }

            trace!("successful lock: (from_runnable: {}) after {} contentions", from_runnable, next_task.contention_count);

            if !from_runnable {
                *contended_count = contended_count.checked_sub(1).unwrap();
                next_task.mark_as_uncontended();
            }
            let lock_attempts = std::mem::take(&mut next_task.tx.1);

            return Some((unique_weight, arc_next_task, lock_attempts));
        } else {
            break;
        }
        }

        None
    }

    #[inline(never)]
    fn finalize_lock_for_provisional_execution<AST: AtScheduleThread>(
        ast: AST,
        address_book: &mut AddressBook,
        next_task: &mut Task,
        tracker: std::sync::Arc<ProvisioningTracker>,
    ) {
        for l in next_task.tx.1.iter_mut() {
            match l.status {
                LockStatus::Provisional => {
                    l.target.page_mut(ast).provisional_task_ids.push(std::sync::Arc::clone(&tracker));
                }
                LockStatus::Succeded => {
                    // do nothing
                }
                LockStatus::Failed => {
                    unreachable!();
                }
            }
        }
        //trace!("provisioning_trackers: {}", address_book.provisioning_trackers.len());
    }

    #[inline(never)]
    fn reset_lock_for_failed_execution<AST: AtScheduleThread>(
        ast: AST,
        address_book: &mut AddressBook,
        unique_weight: &UniqueWeight,
        lock_attempts: &mut Vec<LockAttempt>,
        from_runnable: bool,
    ) {
        for l in lock_attempts {
            address_book.reset_lock(ast, l, false);
        }
    }

    #[inline(never)]
    fn unlock_after_execution<AST: AtScheduleThread>(ast: AST, address_book: &mut AddressBook, lock_attempts: &mut Vec<LockAttempt>, provisioning_tracker_count: &mut usize) {
        for mut l in lock_attempts.into_iter() {
            let newly_uncontended = address_book.reset_lock(ast, &mut l, true);

            let mut page = l.target.page_mut(ast);
            if newly_uncontended && page.next_usage == Usage::Unused {
                let mut inserted = false;

                if let Some(task) = l.heaviest_uncontended.take() {
                    //assert!(!task.already_finished());
                    if /*true ||*/ task.currently_contended() {
                        //assert!(task.currently_contended());
                        inserted = true;
                        address_book.uncontended_task_ids.insert(task.unique_weight, task);
                    } /*else {
                        let contended_unique_weights = &page.contended_unique_weights;
                        contended_unique_weights.heaviest_task_cursor().map(|mut task_cursor| {
                            let mut found = true;
                            //assert_ne!(task_cursor.key(), &task.uq);
                            let mut task = task_cursor.value();
                            while !task.currently_contended() {
                                if let Some(new_cursor) = task_cursor.prev() {
                                    assert!(new_cursor.key() < task_cursor.key());
                                    //assert_ne!(new_cursor.key(), &uq);
                                    task_cursor = new_cursor;
                                    task = task_cursor.value();
                                } else {
                                    found = false;
                                    break;
                                }
                            }
                            found.then(|| TaskInQueue::clone(task))
                        }).flatten().map(|task| {
                            address_book.uncontended_task_ids.insert(task.unique_weight, task);
                            ()
                        });
                    }*/
                }
            }
            if page.current_usage == Usage::Unused && page.next_usage != Usage::Unused {
                page.switch_to_next_usage();
                for mut tracker in std::mem::take(&mut page.provisional_task_ids).into_iter() {
                    panic!();
                    /*
                    let tracker = unsafe { std::sync::Arc::<ProvisioningTracker>::get_mut_unchecked(&mut tracker) };
                    tracker.progress();
                    if tracker.is_fulfilled() {
                        trace!("provisioning tracker progress: {} => {} (!)", tracker.prev_count(), tracker.count());
                        address_book.fulfilled_provisional_task_ids.insert(tracker.task.unique_weight, TaskInQueue::clone(&tracker.task));
                        *provisioning_tracker_count = provisioning_tracker_count.checked_sub(1).unwrap();
                    } else {
                        trace!("provisioning tracker progress: {} => {}", tracker.prev_count(), tracker.count());
                    }
                    */
                }
            }

            // todo: mem::forget and panic in LockAttempt::drop()
        }
    }

    #[inline(never)]
    fn prepare_scheduled_execution(
        address_book: &mut AddressBook,
        unique_weight: UniqueWeight,
        task: TaskInQueue,
        lock_attempts: Vec<LockAttempt>,
        queue_clock: &usize,
        execute_clock: &mut usize,
    ) -> Box<ExecutionEnvironment> {
        let mut rng = rand::thread_rng();
        // load account now from AccountsDb
        task.record_execute_time(*queue_clock, *execute_clock);
        *execute_clock = execute_clock.checked_add(1).unwrap();

        Box::new(ExecutionEnvironment {
            task,
            unique_weight,
            cu: rng.gen_range(3, 1000),
            lock_attempts,
        })
    }

    fn commit_completed_execution2<AST: AtScheduleThread>(_a: AST) {
        panic!();
    }

    #[inline(never)]
    fn commit_completed_execution<AST: AtScheduleThread>(ast: AST, ee: &mut ExecutionEnvironment, address_book: &mut AddressBook, commit_time: &mut usize, provisioning_tracker_count: &mut usize) {
        // do par()-ly?

        //ee.reindex();
        ee.task.record_commit_time(*commit_time);
        //ee.task.trace_timestamps("commit");
        //*commit_time = commit_time.checked_add(1).unwrap();

        // which order for data race free?: unlocking / marking
        Self::unlock_after_execution(ast, address_book, &mut ee.lock_attempts, provisioning_tracker_count);
        ee.task.mark_as_finished();

        // block-wide qos validation will be done here
        // if error risen..:
        //   don't commit the tx for banking and potentially finish scheduling at block max cu
        //   limit
        //   mark the block as dead for replaying

        // par()-ly clone updated Accounts into address book
    }

    #[inline(never)]
    fn schedule_next_execution<AST: AtScheduleThread>(
        ast: AST,
        task_sender: &crossbeam_channel::Sender<(TaskInQueue, Vec<LockAttempt>)>,
        runnable_queue: &mut TaskQueue,
        address_book: &mut AddressBook,
        contended_count: &mut usize,
        prefer_immediate: bool,
        sequence_time: &usize,
        queue_clock: &mut usize,
        execute_clock: &mut usize,
        provisioning_tracker_count: &mut usize,
    ) -> Option<Box<ExecutionEnvironment>> {
        let maybe_ee =
            Self::pop_from_queue_then_lock(ast, task_sender, runnable_queue, address_book, contended_count, prefer_immediate, sequence_time, queue_clock, provisioning_tracker_count)
                .map(|(uw, t,ll)| Self::prepare_scheduled_execution(address_book, uw, t, ll, queue_clock, execute_clock));
        maybe_ee
    }

    #[inline(never)]
    fn register_runnable_task(
        weighted_tx: TaskInQueue,
        runnable_queue: &mut TaskQueue,
        sequence_time: &mut usize,
    ) {
        weighted_tx.record_sequence_time(*sequence_time);
        *sequence_time = sequence_time.checked_add(1).unwrap();
        Self::push_to_runnable_queue(weighted_tx, runnable_queue)
    }

    fn _run<AST: AtScheduleThread>(
        ast: AST,
        max_executing_queue_count: usize,
        runnable_queue: &mut TaskQueue,
        address_book: &mut AddressBook,
        from: &crossbeam_channel::Receiver<TaskInQueue>,
        from_exec: &crossbeam_channel::Receiver<Box<ExecutionEnvironment>>,
        to_execute_substage: &crossbeam_channel::Sender<Box<ExecutionEnvironment>>,
        maybe_to_next_stage: Option<&crossbeam_channel::Sender<Box<ExecutionEnvironment>>>, // assume nonblocking
    ) {

        let mut executing_queue_count = 0_usize;
        let mut contended_count = 0;
        let mut provisioning_tracker_count = 0;
        let mut sequence_time = 0;
        let mut queue_clock = 0;
        let mut execute_clock = 0;
        let (ee_sender, ee_receiver) = crossbeam_channel::unbounded::<Box<ExecutionEnvironment>>();

        let (to_next_stage, maybe_jon_handle) = if let Some(to_next_stage) = maybe_to_next_stage {
            (to_next_stage, None)
        } else {
            let h = std::thread::Builder::new().name("sol-reaper".to_string()).spawn(move || {
                while let Ok(mut a) = ee_receiver.recv() {
                    assert!(a.task.tx.1.is_empty());
                    //assert!(a.task.sequence_time() != usize::max_value());
                    //let lock_attempts = std::mem::take(&mut a.lock_attempts);
                    //drop(lock_attempts);
                    //TaskInQueue::get_mut(&mut a.task).unwrap();
                }
                assert_eq!(ee_receiver.len(), 0);
            }).unwrap();

            (&ee_sender, Some(h))
        };
        let (task_sender, task_receiver) = crossbeam_channel::unbounded::<(TaskInQueue, Vec<LockAttempt>)>();
        let indexer_count = std::env::var("INDEXER_COUNT")
            .unwrap_or(format!("{}", 4))
            .parse::<usize>()
            .unwrap();
        for thx in 0..indexer_count {
            let task_receiver = task_receiver.clone();
            let h = std::thread::Builder::new().name(format!("sol-indexer{:02}", thx)).spawn(move || {
                while let Ok((task, ll)) = task_receiver.recv() {
                    for lock_attempt in ll {
                        if task.already_finished() {
                            break;
                        }
                        lock_attempt.contended_unique_weights().insert_task(task.unique_weight, TaskInQueue::clone(&task));
                    }
                }
                assert_eq!(task_receiver.len(), 0);
            }).unwrap();
        }

        let (mut from_disconnected, mut from_exec_disconnected) = (false, false);
        loop {
            crossbeam_channel::select! {
               recv(from_exec) -> maybe_from_exec => {
                   if maybe_from_exec.is_err() {
                       assert_eq!(from_exec.len(), 0);
                       from_exec_disconnected = true;
                       if from_disconnected {
                           break;
                       } else {
                           continue;
                       }
                   }
                   let mut processed_execution_environment = maybe_from_exec.unwrap();
                    executing_queue_count = executing_queue_count.checked_sub(1).unwrap();
                    Self::commit_completed_execution(ast, &mut processed_execution_environment, address_book, &mut execute_clock, &mut provisioning_tracker_count);
                    to_next_stage.send(processed_execution_environment).unwrap();
               }
               recv(from) -> maybe_from => {
                   if maybe_from.is_err() {
                       assert_eq!(from.len(), 0);
                       from_disconnected = true;
                       if from_exec_disconnected {
                           break;
                       } else {
                           continue;
                       }
                   }
                   let task = maybe_from.unwrap();
                   Self::register_runnable_task(task, runnable_queue, &mut sequence_time);
               }
            }

            let mut first_iteration = true;
            let (mut empty_from, mut empty_from_exec) = (false, false);
            let (mut from_len, mut from_exec_len) = (0, 0);

            loop {
                while (executing_queue_count + provisioning_tracker_count) < max_executing_queue_count {
                    trace!("schedule_once (from: {}, to: {}, runnnable: {}, contended: {}, (immediate+provisional)/max: ({}+{})/{}) active from contended: {}!", from.len(), to_execute_substage.len(), runnable_queue.task_count(), contended_count, executing_queue_count, provisioning_tracker_count, max_executing_queue_count, address_book.uncontended_task_ids.len());
                    let prefer_immediate = provisioning_tracker_count/4 > executing_queue_count;
                    if let Some(ee) = Self::schedule_next_execution(ast, &task_sender, runnable_queue, address_book, &mut contended_count, prefer_immediate, &sequence_time, &mut queue_clock, &mut execute_clock, &mut provisioning_tracker_count) {
                        executing_queue_count = executing_queue_count.checked_add(1).unwrap();
                        to_execute_substage.send(ee).unwrap();
                    } else {
                        break;
                    }
                }
                //break;
                if first_iteration {
                    first_iteration = false;
                    (from_len,  from_exec_len) = (from.len(), from_exec.len());
                } else {
                    if empty_from {
                        from_len = from.len();
                    }
                    if empty_from_exec {
                        from_exec_len = from_exec.len();
                    }
                }
                (empty_from, empty_from_exec) = (from_len == 0, from_exec_len == 0);

                if empty_from && empty_from_exec {
                   break;
                } else {
                    if !empty_from_exec {
                        let mut processed_execution_environment = from_exec.recv().unwrap();
                        from_exec_len = from_exec_len.checked_sub(1).unwrap();
                        empty_from_exec = from_exec_len == 0;
                        executing_queue_count = executing_queue_count.checked_sub(1).unwrap();
                        Self::commit_completed_execution(ast, &mut processed_execution_environment, address_book, &mut execute_clock, &mut provisioning_tracker_count);
                        to_next_stage.send(processed_execution_environment).unwrap();
                    }
                    if !empty_from {
                       let task = from.recv().unwrap();
                       from_len = from_len.checked_sub(1).unwrap();
                       empty_from = from_len == 0;
                       Self::register_runnable_task(task, runnable_queue, &mut sequence_time);
                    }
                }
            }
        }
        info!("run finished...");
    }

    pub fn run(
        max_executing_queue_count: usize,
        runnable_queue: &mut TaskQueue,
        address_book: &mut AddressBook,
        from: &crossbeam_channel::Receiver<TaskInQueue>,
        from_exec: &crossbeam_channel::Receiver<Box<ExecutionEnvironment>>,
        to_execute_substage: &crossbeam_channel::Sender<Box<ExecutionEnvironment>>,
        maybe_to_next_stage: Option<&crossbeam_channel::Sender<Box<ExecutionEnvironment>>>, // assume nonblocking
    ) {
        #[derive(Clone, Copy, Debug)]
        struct AtTopOfScheduleThread;
        unsafe impl AtScheduleThread for AtTopOfScheduleThread {}
        //impl !Send for AtTopOfScheduleThread {}
        //impl !Sync for AtTopOfScheduleThread {}

        Self::_run::<AtTopOfScheduleThread>(AtTopOfScheduleThread, max_executing_queue_count, runnable_queue, address_book, from, from_exec, to_execute_substage, maybe_to_next_stage)
    }
}

struct ExecuteStage {
    //bank: Bank,
}

impl ExecuteStage {}
