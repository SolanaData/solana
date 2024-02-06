#![feature(test)]

extern crate test;

#[cfg(not(target_env = "msvc"))]
use jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

use {
    solana_program_runtime::timings::ExecuteTimings,
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
        installed_scheduler_pool::{InstalledScheduler, SchedulingContext},
        prioritization_fee_cache::PrioritizationFeeCache,
    },
    solana_sdk::{
        system_transaction,
        transaction::{Result, SanitizedTransaction},
    },
    solana_unified_scheduler_pool::{HandlerContext, PooledScheduler, SchedulerPool, TaskHandler},
    std::sync::Arc,
    test::Bencher,
};

use solana_runtime::installed_scheduler_pool::DefaultScheduleExecutionArg;
use solana_sdk::scheduling::SchedulingMode;
use solana_unified_scheduler_logic::SchedulingStateMachine;
use solana_unified_scheduler_pool::SpawnableScheduler;

#[derive(Debug, Clone)]
struct DummyTaskHandler;

impl TaskHandler<DefaultScheduleExecutionArg> for DummyTaskHandler {
    fn handle(
        &self,
        _result: &mut Result<()>,
        _timings: &mut ExecuteTimings,
        _bank: &Arc<Bank>,
        _transaction: &SanitizedTransaction,
        _index: usize,
        _handler_context: &HandlerContext,
    ) {
    }

    fn create<T: SpawnableScheduler<Self, DefaultScheduleExecutionArg>>(pool: &SchedulerPool<T, Self, DefaultScheduleExecutionArg>) -> Self {
        Self
    }
}

fn setup_dummy_fork_graph(bank: Bank) -> Arc<Bank> {
    let slot = bank.slot();
    let bank_fork = BankForks::new_rw_arc(bank);
    let bank = bank_fork.read().unwrap().get(slot).unwrap();
    bank.loaded_programs_cache
        .write()
        .unwrap()
        .set_fork_graph(bank_fork);
    bank
}

use {
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        message::Message,
        pubkey::Pubkey,
        signature::Signer,
        signer::keypair::Keypair,
        transaction::Transaction,
    },
    solana_unified_scheduler_logic::{Task},
};

fn do_bench_tx_throughput(label: &str, bencher: &mut Criterion) {
    solana_logger::setup();

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(10_000);
    let payer = Keypair::new();
    let memo_ix = Instruction {
        program_id: Pubkey::default(),
        accounts: vec![AccountMeta::new(payer.pubkey(), true)],
        data: vec![0x00],
    };
    let mut ixs = vec![];
    for _ in 0..0 {
        ixs.push(memo_ix.clone());
    }
    let msg = Message::new(&ixs, Some(&payer.pubkey()));
    let mut txn = Transaction::new_unsigned(msg);
    //assert_eq!(wire_txn.len(), 3);
    let tx0 = SanitizedTransaction::from_transaction_for_tests(txn);
    let bank = Bank::new_for_tests(&genesis_config);
    let bank = setup_dummy_fork_graph(bank);
    let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
    let pool = SchedulerPool::<PooledScheduler<DummyTaskHandler, DefaultScheduleExecutionArg>, _, _>::new(
        None,
        None,
        None,
        ignored_prioritization_fee_cache,
    );
    let context = SchedulingContext::new(SchedulingMode::BlockVerification, bank.clone());

    let (s, r) = crossbeam_channel::bounded(1000);

    use std::sync::atomic::AtomicUsize;
    let i = AtomicUsize::new();
    for _ in 0..3 {
        std::thread::Builder::new()
            .name("solScGen".to_owned())
            .spawn({
                let tx1 = tx0.clone();
                let s = s.clone();
                move || loop {
                    let tasks = std::iter::repeat_with(|| SchedulingStateMachine::create_task(tx1.clone(), i.fetch_add(1, std::sync::atomic::Ordering::Relaxed), &mut |_| Default::default())).take(100).collect::<Vec<_>>();
                    if s.send(tasks).is_err() {
                        break;
                    }
                }
            })
            .unwrap();
    }
    std::thread::sleep(std::time::Duration::from_secs(5));

    assert_eq!(bank.transaction_count(), 0);
    let mut scheduler = pool.do_take_scheduler(context);
    bencher.bench_function(label, |b| b.iter(|| {
        for _ in 0..600 {
            for t in r.recv().unwrap() {
                scheduler.schedule_task(t);
            }
        }
        scheduler.pause_for_recent_blockhash();
        scheduler.clear_session_result_with_timings();
        scheduler.restart_session();
    }));
}

fn bench_entrypoint(bencher: &mut Criterion) {
    do_bench_tx_throughput("bench_tx_throughput_drop_in_accumulator_conflicting", bencher)
}

use criterion::{criterion_group, criterion_main, Criterion};
criterion_group!(benches, bench_entrypoint);
criterion_main!(benches);
