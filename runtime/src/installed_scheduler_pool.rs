//! Transaction processing glue code, mainly consisting of Object-safe traits
//!
//! `trait InstalledSchedulerPool` is the most crucial piece of code for this whole integration.
//!
//! It lends one of pooled `trait InstalledScheduler`s out to a `Bank`, so that the ubiquitous
//! `Arc<Bank>` can conveniently work as a facade for transaction scheduling, to higher-layer
//! subsystems (i.e. both `ReplayStage` and `BankingStage`). `BankForks`/`BankWithScheduler` and
//! some `Bank` methods are responsible for this book-keeping, including returning the scheduler
//! from the bank to the pool after use.
//!
//! `trait InstalledScheduler` can be fed with `SanitizedTransaction`s. Then, it schedules and
//! commits those transaction execution results into the associated _bank_. That means,
//! `InstalledScheduler` and `Bank` are mutually linked to each other, resulting in somewhat
//! special handling as part of their life-cycle.
//!
//! Albeit being at this abstract interface level, it's generally assumed that each
//! `InstalledScheduler` is backed by multiple threads for performant transaction execution and
//! there're multiple independent schedulers inside a single instance of `InstalledSchedulerPool`.
//!
//! Dynamic dispatch was inevitable, due to the need of delegating those implementations to the
//! dependent crate (`solana-scheduler-pool`, which in turn depends on `solana-ledger`; another
//! dependent crate of `solana-runtime`...), while cutting cyclic dependency.
//!
//! See [InstalledScheduler] for visualized interaction.

use {
    crate::bank::Bank,
    log::*,
    solana_program_runtime::timings::ExecuteTimings,
    solana_scheduler::{SchedulingMode, WithSchedulingMode},
    solana_sdk::{
        hash::Hash,
        slot_history::Slot,
        transaction::{Result, SanitizedTransaction},
    },
    std::{
        borrow::Borrow,
        fmt::Debug,
        ops::Deref,
        sync::{Arc, RwLock},
    },
};
#[cfg(feature = "dev-context-only-utils")]
use {mockall::automock, qualifier_attr::qualifiers};

#[cfg_attr(feature = "dev-context-only-utils", automock)]
pub trait InstalledSchedulerPool<SEA: ScheduleExecutionArg>: Send + Sync + Debug {
    fn take_from_pool(&self, context: SchedulingContext) -> Box<dyn InstalledScheduler<SEA>>;
    fn return_to_pool(&self, scheduler: Box<dyn InstalledScheduler<SEA>>);
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// Schedules, executes, and commits transactions under encapsulated implementation
///
/// The following chart illustrates the ownership/reference interaction between inter-dependent objects across crates:
///
///  ```mermaid
///  graph TD
///      Bank["Arc#lt;Bank#gt;"]
///
///      subgraph solana-runtime
///          BankForks;
///          subgraph cyclic-ref
///              Bank;
///              SchedulingContext;
///              InstalledScheduler{{InstalledScheduler}};
///          end
///          InstalledSchedulerPool{{InstalledSchedulerPool}};
///      end
///
///      subgraph solana-ledger
///          ExecuteBatch(["execute_batch()"]);
///      end
///
///      subgraph solana-scheduler-pool
///          SchedulerPool;
///          PooledScheduler;
///      end
///
///      subgraph solana-scheduler
///          WithSchedulingMode{{WithSchedulingMode}};
///      end
///
///      SchedulingContext -- refs --> Bank;
///      BankForks -- owns --> Bank;
///      BankForks -- owns --> InstalledSchedulerPool;
///      Bank -- refs --> InstalledScheduler;
///
///      SchedulerPool -. impls .-> InstalledSchedulerPool;
///      PooledScheduler -. impls .-> InstalledScheduler;
///      InstalledScheduler -- refs --> SchedulingContext;
///      SchedulingContext -. impls .-> WithSchedulingMode;
///
///      PooledScheduler -- refs --> SchedulerPool;
///      SchedulerPool -- owns --> PooledScheduler;
///      PooledScheduler -. calls .-> ExecuteBatch;
///
///      solana-scheduler-pool -- deps --> solana-scheduler;
///      solana-scheduler-pool -- deps --> solana-ledger;
///      solana-scheduler-pool -- deps --> solana-runtime;
///      solana-ledger -- deps --> solana-runtime;
///      solana-runtime -- deps --> solana-scheduler;
///  ```
#[cfg_attr(feature = "dev-context-only-utils", automock)]
// suppress false clippy complaints arising from mockall-derive:
//   warning: `#[must_use]` has no effect when applied to a struct field
//   warning: the following explicit lifetimes could be elided: 'a
#[cfg_attr(
    feature = "dev-context-only-utils",
    allow(unused_attributes, clippy::needless_lifetimes)
)]
pub trait InstalledScheduler<SEA: ScheduleExecutionArg>: Send + Sync + Debug + 'static {
    fn id(&self) -> SchedulerId;
    fn pool(&self) -> InstalledSchedulerPoolArc<SEA>;

    // Calling this is illegal as soon as wait_for_termination is called.
    fn schedule_execution<'a>(&'a self, transaction_with_index: SEA::TransactionWithIndex<'a>);

    #[must_use]
    fn wait_for_termination(&mut self, reason: &WaitReason) -> Option<ResultWithTimings>;

    fn context<'a>(&'a self) -> Option<&'a SchedulingContext>;
    fn replace_context(&mut self, context: SchedulingContext);
}

pub type DefaultInstalledSchedulerBox = Box<dyn InstalledScheduler<DefaultScheduleExecutionArg>>;

pub type InstalledSchedulerPoolArc<SEA> = Arc<dyn InstalledSchedulerPool<SEA>>;

pub type SchedulerId = u64;

pub type ResultWithTimings = (Result<()>, ExecuteTimings);

pub trait WithTransactionAndIndex: Send + Sync + Debug {
    fn with_transaction_and_index(&self, callback: impl FnOnce(&SanitizedTransaction, usize));
}

impl<
        T: Send + Sync + Debug + Borrow<SanitizedTransaction>,
        U: Send + Sync + Debug + Borrow<usize>,
        Z: Send + Sync + Debug + Deref<Target = (T, U)>,
    > WithTransactionAndIndex for Z
{
    fn with_transaction_and_index(&self, callback: impl FnOnce(&SanitizedTransaction, usize)) {
        callback(self.0.borrow(), *self.1.borrow());
    }
}

pub trait ScheduleExecutionArg: Send + Sync + Debug + 'static {
    // GAT is used to make schedule_execution parametric even supporting references
    // under the object-safety req. of InstalledScheduler trait...
    type TransactionWithIndex<'tx>: WithTransactionAndIndex;
}

#[derive(Debug, Default, Clone)]
pub struct DefaultScheduleExecutionArg;

impl ScheduleExecutionArg for DefaultScheduleExecutionArg {
    type TransactionWithIndex<'tx> = &'tx (&'tx SanitizedTransaction, usize);
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum WaitReason {
    // most normal termination waiting mode; couldn't be done implicitly inside Bank::freeze() -> () to return
    // the result and timing in some way to higher-layer subsystems;
    TerminatedToFreeze,
    // just behaves like TerminatedToFreeze but hint that this is called from Drop::drop().
    DroppedFromBankForks,
    // scheduler is paused without being returned to the pool to collect ResultWithTimings later.
    PausedForRecentBlockhash,
}

impl WaitReason {
    pub fn is_paused(&self) -> bool {
        match self {
            WaitReason::PausedForRecentBlockhash => true,
            WaitReason::TerminatedToFreeze | WaitReason::DroppedFromBankForks => false,
        }
    }
}

/// A small context to propagate a bank and its scheduling mode to the scheduler subsystem.
///
/// Note that this isn't called SchedulerContext because the context isn't necessarily tied to a
/// particular scheduler. A scheduler will use many SchedulingContexts during its lifetime.
/// "Scheduling" part of the context name refers to a slice of time to schedule and execute all
/// transactions for a given bank for block verification or block production. A context is expected
/// to be used by a particular scheduler only for that duration of time and to be disposed by the
/// scheduler. Then, it will be able to work on subsequent SchedulingContexts.
#[derive(Clone, Debug)]
pub struct SchedulingContext {
    mode: SchedulingMode,
    bank: Arc<Bank>,
}

impl WithSchedulingMode for SchedulingContext {
    fn mode(&self) -> SchedulingMode {
        self.mode
    }
}

impl SchedulingContext {
    pub fn new(mode: SchedulingMode, bank: Arc<Bank>) -> Self {
        Self { mode, bank }
    }

    pub fn bank(&self) -> &Arc<Bank> {
        &self.bank
    }

    pub fn slot(&self) -> Slot {
        self.bank().slot()
    }

    pub fn log_prefix(context: Option<&Self>, scheduler_id: SchedulerId) -> String {
        const BITS_PER_HEX_DIGIT: usize = 4;

        format!(
            "sch_{:0width$x}{}: ",
            scheduler_id,
            context
                .map(|c| format!("(slot:{}/mode:{:?})", c.slot(), c.mode()))
                .unwrap_or_else(|| "(?)".into()),
            width = SchedulerId::BITS as usize / BITS_PER_HEX_DIGIT,
        )
    }
}

/// Very thin wrapper around Arc<Bank>
///
/// It brings type-safety against accidental mixing of bank and scheduler with different slots,
/// which is a pretty dangerous condition. Also, it guarantees to call wait_for_termination() via
/// ::drop() inside BankForks::set_root()'s pruning, perfectly matching to Arc<Bank>'s lifetime by
/// piggybacking on the pruning.
///
/// Semantically, a scheduler is tightly coupled with a particular bank. But scheduler wasn't put
/// into Bank fields to avoid circular-references (a scheduler needs to refer to its accompanied
/// Arc<Bank>). BankWithScheduler behaves almost like Arc<Bank>. It only adds a few of transaction
/// scheduling and scheduler management functions. For this reason, `bank` variable names should be
/// used for `BankWithScheduler` across codebase.
///
/// BankWithScheduler even implements Deref for convenience. And Clone is omitted to implement to
/// avoid ambiguity as to which to clone: BankWithScheduler or Arc<Bank>. Use
/// clone_without_scheduler() for Arc<Bank>. Otherwise, use clone_with_scheduler() (this should be
/// unusual outside scheduler code-path)
#[derive(Debug)]
pub struct BankWithScheduler {
    inner: Arc<BankWithSchedulerInner>,
}

#[derive(Debug)]
pub struct BankWithSchedulerInner {
    bank: Arc<Bank>,
    scheduler: InstalledSchedulerRwLock,
}
pub type InstalledSchedulerRwLock = RwLock<Option<DefaultInstalledSchedulerBox>>;

impl BankWithScheduler {
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn new(bank: Arc<Bank>, scheduler: Option<DefaultInstalledSchedulerBox>) -> Self {
        if let Some(bank_in_context) = scheduler.as_ref().and_then(|scheduler| scheduler.context())
        {
            assert_eq!(bank.slot(), bank_in_context.slot());
        }

        Self {
            inner: Arc::new(BankWithSchedulerInner {
                bank,
                scheduler: RwLock::new(scheduler),
            }),
        }
    }

    pub fn new_without_scheduler(bank: Arc<Bank>) -> Self {
        Self::new(bank, None)
    }

    pub fn clone_with_scheduler(&self) -> BankWithScheduler {
        BankWithScheduler {
            inner: self.inner.clone(),
        }
    }

    pub fn clone_without_scheduler(&self) -> Arc<Bank> {
        self.inner.bank.clone()
    }

    pub fn register_tick(&self, hash: &Hash) {
        self.inner.bank.register_tick(hash, &self.inner.scheduler);
    }

    pub fn fill_bank_with_ticks_for_tests(&self) {
        self.do_fill_bank_with_ticks_for_tests(&self.inner.scheduler);
    }

    pub fn has_installed_scheduler(&self) -> bool {
        self.inner.scheduler.read().unwrap().is_some()
    }

    // 'a is needed; anonymous_lifetime_in_impl_trait isn't stabilized yet...
    pub fn schedule_transaction_executions<'a>(
        &self,
        transactions_with_indexes: impl ExactSizeIterator<Item = (&'a SanitizedTransaction, &'a usize)>,
    ) {
        trace!(
            "schedule_transaction_executions(): {} txs",
            transactions_with_indexes.len()
        );

        let scheduler_guard = self.inner.scheduler.read().unwrap();
        let scheduler = scheduler_guard.as_ref().unwrap();

        for (sanitized_transaction, &index) in transactions_with_indexes {
            scheduler.schedule_execution(&(sanitized_transaction, index));
        }
    }

    // take needless &mut only to communicate its semantic mutability to humans...
    #[cfg(feature = "dev-context-only-utils")]
    pub fn drop_scheduler(&mut self) {
        self.inner.drop_scheduler();
    }

    pub(crate) fn wait_for_paused_scheduler(bank: &Bank, scheduler: &InstalledSchedulerRwLock) {
        let maybe_result_with_timings = BankWithSchedulerInner::wait_for_scheduler(
            bank,
            scheduler,
            WaitReason::PausedForRecentBlockhash,
        );
        assert!(
            maybe_result_with_timings.is_none(),
            "Premature result was returned from scheduler after paused"
        );
    }

    #[must_use]
    pub fn wait_for_completed_scheduler(&self) -> Option<ResultWithTimings> {
        BankWithSchedulerInner::wait_for_scheduler(
            &self.inner.bank,
            &self.inner.scheduler,
            WaitReason::TerminatedToFreeze,
        )
    }

    pub const fn no_scheduler_available() -> InstalledSchedulerRwLock {
        RwLock::new(None)
    }
}

impl BankWithSchedulerInner {
    #[must_use]
    fn wait_for_completed_scheduler_from_drop(&self) -> Option<ResultWithTimings> {
        Self::wait_for_scheduler(
            &self.bank,
            &self.scheduler,
            WaitReason::DroppedFromBankForks,
        )
    }

    #[must_use]
    fn wait_for_scheduler(
        bank: &Bank,
        scheduler: &InstalledSchedulerRwLock,
        reason: WaitReason,
    ) -> Option<ResultWithTimings> {
        debug!(
            "wait_for_scheduler(slot: {}, reason: {:?}): started...",
            bank.slot(),
            reason,
        );

        let mut scheduler = scheduler.write().unwrap();
        let result_with_timings = if scheduler.is_some() {
            let result_with_timings = scheduler
                .as_mut()
                .and_then(|scheduler| scheduler.wait_for_termination(&reason));
            if !reason.is_paused() {
                let scheduler = scheduler.take().expect("scheduler after waiting");
                scheduler.pool().return_to_pool(scheduler);
            }
            result_with_timings
        } else {
            None
        };
        debug!(
            "wait_for_scheduler(slot: {}, reason: {:?}): finished with: {:?}...",
            bank.slot(),
            reason,
            result_with_timings.as_ref().map(|(result, _)| result),
        );

        result_with_timings
    }

    fn drop_scheduler(&self) {
        if std::thread::panicking() {
            error!(
                "BankWithSchedulerInner::drop_scheduler(): slot: {} skipping due to already panicking...",
                self.bank.slot(),
            );
            return;
        }

        // There's no guarantee ResultWithTimings is available or not at all when being dropped.
        if let Some(Err(err)) = self
            .wait_for_completed_scheduler_from_drop()
            .map(|(result, _timings)| result)
        {
            warn!(
                "BankWithSchedulerInner::drop_scheduler(): slot: {} discarding error from scheduler: {:?}",
                self.bank.slot(),
                err,
            );
        }
    }
}

impl Drop for BankWithSchedulerInner {
    fn drop(&mut self) {
        self.drop_scheduler();
    }
}

impl Deref for BankWithScheduler {
    type Target = Arc<Bank>;

    fn deref(&self) -> &Self::Target {
        &self.inner.bank
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::test_utils::goto_end_of_slot_with_scheduler,
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
        },
        assert_matches::assert_matches,
        mockall::Sequence,
        solana_sdk::system_transaction,
    };

    fn setup_mocked_scheduler_pool(
        seq: &mut Sequence,
    ) -> InstalledSchedulerPoolArc<DefaultScheduleExecutionArg> {
        let mut mock = MockInstalledSchedulerPool::new();
        mock.expect_return_to_pool()
            .times(1)
            .in_sequence(seq)
            .returning(|_| ());
        Arc::new(mock)
    }

    fn setup_mocked_scheduler_with_extra(
        wait_reasons: impl Iterator<Item = WaitReason>,
        f: Option<impl Fn(&mut MockInstalledScheduler<DefaultScheduleExecutionArg>)>,
    ) -> DefaultInstalledSchedulerBox {
        let mut mock = MockInstalledScheduler::new();
        let mut seq = Sequence::new();

        mock.expect_context()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|| None);

        for wait_reason in wait_reasons {
            mock.expect_wait_for_termination()
                .with(mockall::predicate::eq(wait_reason))
                .times(1)
                .in_sequence(&mut seq)
                .returning(move |_| {
                    if wait_reason.is_paused() {
                        None
                    } else {
                        Some((Ok(()), ExecuteTimings::default()))
                    }
                });
        }

        mock.expect_pool()
            .times(1)
            .in_sequence(&mut seq)
            .returning(move || setup_mocked_scheduler_pool(&mut seq));
        if let Some(f) = f {
            f(&mut mock);
        }

        Box::new(mock)
    }

    fn setup_mocked_scheduler(
        wait_reasons: impl Iterator<Item = WaitReason>,
    ) -> DefaultInstalledSchedulerBox {
        setup_mocked_scheduler_with_extra(
            wait_reasons,
            None::<fn(&mut MockInstalledScheduler<DefaultScheduleExecutionArg>) -> ()>,
        )
    }

    #[test]
    fn test_scheduling_context() {
        solana_logger::setup();

        let bank = Arc::new(Bank::default_for_tests());
        let context = &SchedulingContext::new(SchedulingMode::BlockVerification, bank);
        assert_eq!(context.slot(), 0);
        assert_eq!(
            SchedulingContext::log_prefix(Some(context), 3),
            "sch_0000000000000003(slot:0/mode:BlockVerification): "
        );
        assert_eq!(
            SchedulingContext::log_prefix(None, 3),
            "sch_0000000000000003(?): "
        );
    }

    #[test]
    fn test_scheduler_normal_termination() {
        solana_logger::setup();

        let bank = Arc::new(Bank::default_for_tests());
        let bank = BankWithScheduler::new(
            bank,
            Some(setup_mocked_scheduler(
                [WaitReason::TerminatedToFreeze].into_iter(),
            )),
        );
        assert!(bank.has_installed_scheduler());
        assert_matches!(bank.wait_for_completed_scheduler(), Some(_));

        // Repeating to call wait_for_completed_scheduler() is okay with no ResultWithTimings being
        // returned.
        assert!(!bank.has_installed_scheduler());
        assert_matches!(bank.wait_for_completed_scheduler(), None);
    }

    #[test]
    fn test_no_scheduler_termination() {
        solana_logger::setup();

        let bank = Arc::new(Bank::default_for_tests());
        let bank = BankWithScheduler::new_without_scheduler(bank);

        // Calling wait_for_completed_scheduler() is noop, when no scheduler is installed.
        assert!(!bank.has_installed_scheduler());
        assert_matches!(bank.wait_for_completed_scheduler(), None);
    }

    #[test]
    fn test_scheduler_termination_from_drop() {
        solana_logger::setup();

        let bank = Arc::new(Bank::default_for_tests());
        let bank = BankWithScheduler::new(
            bank,
            Some(setup_mocked_scheduler(
                [WaitReason::DroppedFromBankForks].into_iter(),
            )),
        );
        drop(bank);
    }

    #[test]
    fn test_scheduler_pause() {
        solana_logger::setup();

        let bank = Arc::new(crate::bank::tests::create_simple_test_bank(42));
        let bank = BankWithScheduler::new(
            bank,
            Some(setup_mocked_scheduler(
                [
                    WaitReason::PausedForRecentBlockhash,
                    WaitReason::TerminatedToFreeze,
                ]
                .into_iter(),
            )),
        );
        goto_end_of_slot_with_scheduler(&bank);
        assert_matches!(bank.wait_for_completed_scheduler(), Some(_));
    }

    #[test]
    fn test_schedule_executions() {
        solana_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let tx0 = SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_sdk::pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let mocked_scheduler = setup_mocked_scheduler_with_extra(
            [WaitReason::DroppedFromBankForks].into_iter(),
            Some(
                |mocked: &mut MockInstalledScheduler<DefaultScheduleExecutionArg>| {
                    mocked
                        .expect_schedule_execution()
                        .times(1)
                        .returning(|(_, _)| ());
                },
            ),
        );

        let bank = BankWithScheduler::new(bank, Some(mocked_scheduler));
        bank.schedule_transaction_executions([(&tx0, &0)].into_iter());
    }
}
