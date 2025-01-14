use once_cell::sync::Lazy;

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use vm::{
    vm::{VmPartialExecutionResult, VmTxExecutionResult},
    vm_with_bootloader::{BlockContext, BlockContextMode, DerivedBlockContext},
    VmBlockResult, VmExecutionResult,
};
use zksync_config::{configs::chain::StateKeeperConfig, constants::ZKPORTER_IS_AVAILABLE};
use zksync_contracts::{BaseSystemContracts, BaseSystemContractsHashes};
use zksync_types::{
    block::BlockGasCount,
    commitment::{BlockMetaParameters, BlockMetadata},
    fee::Fee,
    l2::L2Tx,
    transaction_request::PaymasterParams,
    tx::tx_execution_info::{TxExecutionStatus, VmExecutionLogs},
    vm_trace::{VmExecutionTrace, VmTrace},
    zk_evm::aux_structures::{LogQuery, Timestamp},
    zk_evm::block_properties::BlockProperties,
    Address, L2ChainId, MiniblockNumber, Nonce, StorageLogQuery, StorageLogQueryType, Transaction,
    H256, U256,
};
use zksync_utils::h256_to_u256;

use self::tester::{
    bootloader_tip_out_of_gas, pending_batch_data, random_tx, rejected_exec, successful_exec,
    successful_exec_with_metrics, TestScenario,
};
use crate::gas_tracker::constants::{
    BLOCK_COMMIT_BASE_COST, BLOCK_EXECUTE_BASE_COST, BLOCK_PROVE_BASE_COST,
};
use crate::state_keeper::{
    keeper::POLL_WAIT_DURATION,
    seal_criteria::{
        criteria::{GasCriterion, SlotsCriterion},
        ConditionalSealer, SealManager,
    },
    types::ExecutionMetricsForCriteria,
    updates::UpdatesManager,
};

mod tester;

pub(super) static BASE_SYSTEM_CONTRACTS: Lazy<BaseSystemContracts> =
    Lazy::new(BaseSystemContracts::load_from_disk);

pub(super) fn default_block_properties() -> BlockProperties {
    BlockProperties {
        default_aa_code_hash: h256_to_u256(BASE_SYSTEM_CONTRACTS.default_aa.hash),
        zkporter_is_available: ZKPORTER_IS_AVAILABLE,
    }
}

pub(super) fn create_block_metadata(number: u32) -> BlockMetadata {
    BlockMetadata {
        root_hash: H256::from_low_u64_be(number.into()),
        rollup_last_leaf_index: u64::from(number) + 20,
        merkle_root_hash: H256::from_low_u64_be(number.into()),
        initial_writes_compressed: vec![],
        repeated_writes_compressed: vec![],
        commitment: H256::from_low_u64_be(number.into()),
        l2_l1_messages_compressed: vec![],
        l2_l1_merkle_root: H256::from_low_u64_be(number.into()),
        block_meta_params: BlockMetaParameters {
            zkporter_is_available: ZKPORTER_IS_AVAILABLE,
            bootloader_code_hash: BASE_SYSTEM_CONTRACTS.bootloader.hash,
            default_aa_code_hash: BASE_SYSTEM_CONTRACTS.default_aa.hash,
        },
        aux_data_hash: H256::zero(),
        meta_parameters_hash: H256::zero(),
        pass_through_data_hash: H256::zero(),
    }
}

pub(super) fn default_vm_block_result() -> VmBlockResult {
    VmBlockResult {
        full_result: VmExecutionResult {
            events: vec![],
            storage_log_queries: vec![],
            used_contract_hashes: vec![],
            l2_to_l1_logs: vec![],
            return_data: vec![],
            gas_used: 0,
            contracts_used: 0,
            revert_reason: None,
            trace: VmTrace::ExecutionTrace(VmExecutionTrace::default()),
            total_log_queries: 0,
            cycles_used: 0,
            computational_gas_used: 0,
        },
        block_tip_result: VmPartialExecutionResult {
            logs: VmExecutionLogs::default(),
            revert_reason: None,
            contracts_used: 0,
            cycles_used: 0,
            computational_gas_used: 0,
        },
    }
}

pub(super) fn default_block_context() -> DerivedBlockContext {
    DerivedBlockContext {
        context: BlockContext {
            block_number: 0,
            block_timestamp: 0,
            l1_gas_price: 0,
            fair_l2_gas_price: 0,
            operator_address: Address::default(),
        },
        base_fee: 0,
    }
}

pub(super) fn create_updates_manager() -> UpdatesManager {
    let block_context = BlockContextMode::NewBlock(default_block_context(), 0.into());
    UpdatesManager::new(&block_context, BaseSystemContractsHashes::default())
}

pub(super) fn create_l2_transaction(fee_per_gas: u64, gas_per_pubdata: u32) -> L2Tx {
    let fee = Fee {
        gas_limit: 1000_u64.into(),
        max_fee_per_gas: fee_per_gas.into(),
        max_priority_fee_per_gas: 0_u64.into(),
        gas_per_pubdata_limit: gas_per_pubdata.into(),
    };
    let mut tx = L2Tx::new_signed(
        Address::random(),
        vec![],
        Nonce(0),
        fee,
        U256::zero(),
        L2ChainId(271),
        &H256::repeat_byte(0x11),
        None,
        PaymasterParams::default(),
    )
    .unwrap();
    // Input means all transaction data (NOT calldata, but all tx fields) that came from the API.
    // This input will be used for the derivation of the tx hash, so put some random to it to be sure
    // that the transaction hash is unique.
    tx.set_input(H256::random().0.to_vec(), H256::random());
    tx
}

pub(super) fn create_transaction(fee_per_gas: u64, gas_per_pubdata: u32) -> Transaction {
    create_l2_transaction(fee_per_gas, gas_per_pubdata).into()
}

pub(super) fn create_execution_result(
    tx_number_in_block: u16,
    storage_logs: impl IntoIterator<Item = (U256, Query)>,
) -> VmTxExecutionResult {
    let storage_logs: Vec<_> = storage_logs
        .into_iter()
        .map(|(key, query)| query.into_log(key, tx_number_in_block))
        .collect();

    let logs = VmExecutionLogs {
        total_log_queries_count: storage_logs.len() + 2,
        storage_logs,
        events: vec![],
        l2_to_l1_logs: vec![],
    };
    VmTxExecutionResult {
        status: TxExecutionStatus::Success,
        result: VmPartialExecutionResult {
            logs,
            revert_reason: None,
            contracts_used: 0,
            cycles_used: 0,
            computational_gas_used: 0,
        },
        call_traces: vec![],
        gas_refunded: 0,
        operator_suggested_refund: 0,
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Query {
    Read(U256),
    InitialWrite(U256),
    RepeatedWrite(U256, U256),
}

impl Query {
    fn into_log(self, key: U256, tx_number_in_block: u16) -> StorageLogQuery {
        let log_type = match self {
            Self::Read(_) => StorageLogQueryType::Read,
            Self::InitialWrite(_) => StorageLogQueryType::InitialWrite,
            Self::RepeatedWrite(_, _) => StorageLogQueryType::RepeatedWrite,
        };

        StorageLogQuery {
            log_query: LogQuery {
                timestamp: Timestamp(0),
                tx_number_in_block,
                aux_byte: 0,
                shard_id: 0,
                address: Address::default(),
                key,
                read_value: match self {
                    Self::Read(prev) | Self::RepeatedWrite(prev, _) => prev,
                    Self::InitialWrite(_) => U256::zero(),
                },
                written_value: match self {
                    Self::Read(_) => U256::zero(),
                    Self::InitialWrite(value) | Self::RepeatedWrite(_, value) => value,
                },
                rw_flag: !matches!(self, Self::Read(_)),
                rollback: false,
                is_service: false,
            },
            log_type,
        }
    }
}

#[tokio::test]
async fn sealed_by_number_of_txs() {
    let config = StateKeeperConfig {
        transaction_slots: 2,
        ..Default::default()
    };
    let conditional_sealer = Some(ConditionalSealer::with_sealers(
        config,
        vec![Box::new(SlotsCriterion)],
    ));
    let sealer = SealManager::custom(
        conditional_sealer,
        vec![Box::new(|_| false)],
        vec![Box::new(|updates| {
            updates.miniblock.executed_transactions.len() == 1
        })],
    );

    let scenario = TestScenario::new();

    scenario
        .next_tx("First tx", random_tx(1), successful_exec())
        .miniblock_sealed("Miniblock 1")
        .next_tx("Second tx", random_tx(2), successful_exec())
        .miniblock_sealed("Miniblock 2")
        .batch_sealed("Batch 1")
        .run(sealer)
        .await;
}

#[tokio::test]
async fn sealed_by_gas() {
    let config = StateKeeperConfig {
        max_single_tx_gas: 62_002,
        reject_tx_at_gas_percentage: 1.0,
        close_block_at_gas_percentage: 0.5,
        ..Default::default()
    };
    let conditional_sealer = Some(ConditionalSealer::with_sealers(
        config,
        vec![Box::new(GasCriterion)],
    ));
    let sealer = SealManager::custom(
        conditional_sealer,
        vec![Box::new(|_| false)],
        vec![Box::new(|updates| {
            updates.miniblock.executed_transactions.len() == 1
        })],
    );

    let l1_gas_per_tx = BlockGasCount {
        commit: 1, // Both txs together with block_base_cost would bring it over the block 31_001 commit bound.
        prove: 0,
        execute: 0,
    };
    let execution_result = successful_exec_with_metrics(ExecutionMetricsForCriteria {
        l1_gas: l1_gas_per_tx,
        execution_metrics: Default::default(),
    });

    TestScenario::new()
        .next_tx("First tx", random_tx(1), execution_result.clone())
        .miniblock_sealed_with("Miniblock with a single tx", move |updates| {
            assert_eq!(
                updates.miniblock.l1_gas_count,
                l1_gas_per_tx,
                "L1 gas used by a miniblock should consist of the gas used by its txs"
            );
        })
        .next_tx("Second tx", random_tx(1), execution_result)
        .miniblock_sealed("Miniblock 2")
        .batch_sealed_with("Batch sealed with both txs", |_, updates, _| {
            assert_eq!(
                updates.l1_batch.l1_gas_count,
                BlockGasCount {
                    commit: BLOCK_COMMIT_BASE_COST + 2,
                    prove: BLOCK_PROVE_BASE_COST,
                    execute: BLOCK_EXECUTE_BASE_COST,
                },
                "L1 gas used by a batch should consists of gas used by its txs + basic block gas cost"
            );
        })
        .run(sealer).await;
}

#[tokio::test]
async fn sealed_by_gas_then_by_num_tx() {
    let config = StateKeeperConfig {
        max_single_tx_gas: 62_000,
        reject_tx_at_gas_percentage: 1.0,
        close_block_at_gas_percentage: 0.5,
        transaction_slots: 3,
        ..Default::default()
    };
    let conditional_sealer = Some(ConditionalSealer::with_sealers(
        config,
        vec![Box::new(GasCriterion), Box::new(SlotsCriterion)],
    ));
    let sealer = SealManager::custom(
        conditional_sealer,
        vec![Box::new(|_| false)],
        vec![Box::new(|updates| {
            updates.miniblock.executed_transactions.len() == 1
        })],
    );

    let execution_result = successful_exec_with_metrics(ExecutionMetricsForCriteria {
        l1_gas: BlockGasCount {
            commit: 1,
            prove: 0,
            execute: 0,
        },
        execution_metrics: Default::default(),
    });

    // 1st tx is sealed by gas sealer; 2nd, 3rd, & 4th are sealed by slots sealer.
    TestScenario::new()
        .next_tx("First tx", random_tx(1), execution_result)
        .miniblock_sealed("Miniblock 1")
        .batch_sealed("Batch 1")
        .next_tx("Second tx", random_tx(2), successful_exec())
        .miniblock_sealed("Miniblock 2")
        .next_tx("Third tx", random_tx(3), successful_exec())
        .miniblock_sealed("Miniblock 3")
        .next_tx("Fourth tx", random_tx(4), successful_exec())
        .miniblock_sealed("Miniblock 4")
        .batch_sealed("Batch 2")
        .run(sealer)
        .await;
}

#[tokio::test]
async fn batch_sealed_before_miniblock_does() {
    let config = StateKeeperConfig {
        transaction_slots: 2,
        ..Default::default()
    };
    let conditional_sealer = Some(ConditionalSealer::with_sealers(
        config,
        vec![Box::new(SlotsCriterion)],
    ));
    let sealer = SealManager::custom(
        conditional_sealer,
        vec![Box::new(|_| false)],
        vec![Box::new(|updates| {
            updates.miniblock.executed_transactions.len() == 3
        })],
    );

    let scenario = TestScenario::new();

    // Miniblock sealer will not return true before the batch is sealed because the batch only has 2 txs.
    scenario
        .next_tx("First tx", random_tx(1), successful_exec())
        .next_tx("Second tx", random_tx(2), successful_exec())
        .miniblock_sealed_with("Miniblock with two txs", |updates| {
            assert_eq!(
                updates.miniblock.executed_transactions.len(),
                2,
                "The miniblock should have 2 txs"
            );
        })
        .batch_sealed("Batch 1")
        .run(sealer)
        .await;
}

#[tokio::test]
async fn basic_flow() {
    let config = StateKeeperConfig {
        transaction_slots: 2,
        ..Default::default()
    };
    let conditional_sealer = Some(ConditionalSealer::with_sealers(
        config,
        vec![Box::new(SlotsCriterion)],
    ));
    let sealer = SealManager::custom(
        conditional_sealer,
        vec![Box::new(|_| false)],
        vec![Box::new(|updates| {
            updates.miniblock.executed_transactions.len() == 1
        })],
    );

    TestScenario::new()
        .next_tx("First tx", random_tx(1), successful_exec())
        .miniblock_sealed("Miniblock 1")
        .next_tx("Second tx", random_tx(2), successful_exec())
        .miniblock_sealed("Miniblock 2")
        .batch_sealed("Batch 1")
        .run(sealer)
        .await;
}

#[tokio::test]
async fn rejected_tx() {
    let config = StateKeeperConfig {
        transaction_slots: 2,
        ..Default::default()
    };
    let conditional_sealer = Some(ConditionalSealer::with_sealers(
        config,
        vec![Box::new(SlotsCriterion)],
    ));
    let sealer = SealManager::custom(
        conditional_sealer,
        vec![Box::new(|_| false)],
        vec![Box::new(|updates| {
            updates.miniblock.executed_transactions.len() == 1
        })],
    );

    let rejected_tx = random_tx(1);
    TestScenario::new()
        .next_tx("Rejected tx", rejected_tx.clone(), rejected_exec())
        .tx_rejected("Tx got rejected", rejected_tx, None)
        .next_tx("Successful tx", random_tx(2), successful_exec())
        .miniblock_sealed("Miniblock with successful tx")
        .next_tx("Second successful tx", random_tx(3), successful_exec())
        .miniblock_sealed("Second miniblock")
        .batch_sealed("Batch with 2 successful txs")
        .run(sealer)
        .await;
}

#[tokio::test]
async fn bootloader_tip_out_of_gas_flow() {
    let config = StateKeeperConfig {
        transaction_slots: 2,
        ..Default::default()
    };
    let conditional_sealer = Some(ConditionalSealer::with_sealers(
        config,
        vec![Box::new(SlotsCriterion)],
    ));
    let sealer = SealManager::custom(
        conditional_sealer,
        vec![Box::new(|_| false)],
        vec![Box::new(|updates| {
            updates.miniblock.executed_transactions.len() == 1
        })],
    );

    let first_tx = random_tx(1);
    let bootloader_out_of_gas_tx = random_tx(2);
    let third_tx = random_tx(3);
    TestScenario::new()
        .next_tx("First tx", first_tx, successful_exec())
        .miniblock_sealed("Miniblock with 1st tx")
        .next_tx(
            "Tx -> Bootloader tip out of gas",
            bootloader_out_of_gas_tx.clone(),
            bootloader_tip_out_of_gas(),
        )
        .tx_rollback(
            "Last tx rolled back to seal the block",
            bootloader_out_of_gas_tx.clone(),
        )
        .batch_sealed("Batch sealed with 1 tx")
        .next_tx(
            "Same tx now succeeds",
            bootloader_out_of_gas_tx,
            successful_exec(),
        )
        .miniblock_sealed("Miniblock with this tx sealed")
        .next_tx("Second tx of the 2nd batch", third_tx, successful_exec())
        .miniblock_sealed("Miniblock with 2nd tx")
        .batch_sealed("2nd batch sealed")
        .run(sealer)
        .await;
}

#[tokio::test]
async fn bootloader_config_has_been_updated() {
    let sealer = SealManager::custom(
        None,
        vec![SealManager::code_hash_batch_sealer(
            BaseSystemContractsHashes {
                bootloader: Default::default(),
                default_aa: Default::default(),
            },
        )],
        vec![Box::new(|_| false)],
    );

    let pending_batch =
        pending_batch_data(vec![(MiniblockNumber(1), vec![random_tx(1), random_tx(2)])]);

    TestScenario::new()
        .load_pending_batch(pending_batch)
        .batch_sealed_with("Batch sealed with all 2 tx", |_, updates, _| {
            assert_eq!(
                updates.l1_batch.executed_transactions.len(),
                2,
                "There should be 2 transactions in the batch"
            );
        })
        .next_tx("Final tx of batch", random_tx(3), successful_exec())
        .miniblock_sealed("Miniblock with this tx sealed")
        .batch_sealed_with("Batch sealed with all 1 tx", |_, updates, _| {
            assert_eq!(
                updates.l1_batch.executed_transactions.len(),
                1,
                "There should be 1 transactions in the batch"
            );
        })
        .run(sealer)
        .await;
}

#[tokio::test]
async fn pending_batch_is_applied() {
    let config = StateKeeperConfig {
        transaction_slots: 3,
        ..Default::default()
    };
    let conditional_sealer = Some(ConditionalSealer::with_sealers(
        config,
        vec![Box::new(SlotsCriterion)],
    ));
    let sealer = SealManager::custom(
        conditional_sealer,
        vec![Box::new(|_| false)],
        vec![Box::new(|updates| {
            updates.miniblock.executed_transactions.len() == 1
        })],
    );

    let pending_batch = pending_batch_data(vec![
        (MiniblockNumber(1), vec![random_tx(1)]),
        (MiniblockNumber(2), vec![random_tx(2)]),
    ]);

    // We configured state keeper to use different system contract hashes, so it must seal the pending batch immediately.
    TestScenario::new()
        .load_pending_batch(pending_batch)
        .next_tx("Final tx of batch", random_tx(3), successful_exec())
        .miniblock_sealed_with("Miniblock with a single tx", |updates| {
            assert_eq!(
                updates.miniblock.executed_transactions.len(),
                1,
                "Only one transaction should be in miniblock"
            );
        })
        .batch_sealed_with("Batch sealed with all 3 txs", |_, updates, _| {
            assert_eq!(
                updates.l1_batch.executed_transactions.len(),
                3,
                "There should be 3 transactions in the batch"
            );
        })
        .run(sealer)
        .await;
}

/// Unconditionally seal the batch without triggering specific criteria.
#[tokio::test]
async fn unconditional_sealing() {
    // Trigger to know when to seal the batch.
    // Once miniblock with one tx would be sealed, trigger would allow batch to be sealed as well.
    let batch_seal_trigger = Arc::new(AtomicBool::new(false));
    let batch_seal_trigger_checker = batch_seal_trigger.clone();
    let start = Instant::now();
    let seal_miniblock_after = POLL_WAIT_DURATION; // Seal after 2 state keeper polling duration intervals.

    let config = StateKeeperConfig {
        transaction_slots: 2,
        ..Default::default()
    };
    let conditional_sealer = Some(ConditionalSealer::with_sealers(
        config,
        vec![Box::new(SlotsCriterion)],
    ));
    let sealer = SealManager::custom(
        conditional_sealer,
        vec![Box::new(move |_| {
            batch_seal_trigger_checker.load(Ordering::Relaxed)
        })],
        vec![Box::new(move |upd_manager| {
            if upd_manager.pending_executed_transactions_len() != 0
                && start.elapsed() >= seal_miniblock_after
            {
                batch_seal_trigger.store(true, Ordering::Relaxed);
                true
            } else {
                false
            }
        })],
    );

    TestScenario::new()
        .next_tx("The only tx", random_tx(1), successful_exec())
        .no_txs_until_next_action("We don't give transaction to wait for miniblock to be sealed")
        .miniblock_sealed("Miniblock is sealed with just one tx")
        .no_txs_until_next_action("Still no tx")
        .batch_sealed("Batch is sealed with just one tx")
        .run(sealer)
        .await;
}

/// Checks the next miniblock sealed after pending batch has a correct timestamp
#[tokio::test]
async fn miniblock_timestamp_after_pending_batch() {
    let config = StateKeeperConfig {
        transaction_slots: 2,
        ..Default::default()
    };
    let conditional_sealer = Some(ConditionalSealer::with_sealers(
        config,
        vec![Box::new(SlotsCriterion)],
    ));
    let sealer = SealManager::custom(
        conditional_sealer,
        vec![Box::new(|_| false)],
        vec![Box::new(|updates| {
            updates.miniblock.executed_transactions.len() == 1
        })],
    );

    let pending_batch = pending_batch_data(vec![(MiniblockNumber(1), vec![random_tx(1)])]);

    TestScenario::new()
        .load_pending_batch(pending_batch)
        .next_tx(
            "First tx after pending batch",
            random_tx(2),
            successful_exec(),
        )
        .miniblock_sealed_with("Miniblock with a single tx", move |updates| {
            assert!(
                updates.miniblock.timestamp == 1,
                "Timestamp for the new block must be taken from the test IO"
            );
        })
        .batch_sealed("Batch is sealed with two transactions")
        .run(sealer)
        .await;
}

/// Makes sure that the timestamp doesn't decrease in consequent miniblocks.
///
/// Timestamps are faked in the IO layer, so this test mostly makes sure that the state keeper doesn't substitute
/// any unexpected value on its own.
#[tokio::test]
async fn time_is_monotonic() {
    let timestamp_first_miniblock = Arc::new(AtomicU64::new(0u64)); // Time is faked in tests.
    let timestamp_second_miniblock = timestamp_first_miniblock.clone();
    let timestamp_third_miniblock = timestamp_first_miniblock.clone();

    let config = StateKeeperConfig {
        transaction_slots: 2,
        ..Default::default()
    };
    let conditional_sealer = Some(ConditionalSealer::with_sealers(
        config,
        vec![Box::new(SlotsCriterion)],
    ));
    let sealer = SealManager::custom(
        conditional_sealer,
        vec![Box::new(|_| false)],
        vec![Box::new(|updates| {
            updates.miniblock.executed_transactions.len() == 1
        })],
    );

    let scenario = TestScenario::new();

    scenario
        .next_tx("First tx", random_tx(1), successful_exec())
        .miniblock_sealed_with("Miniblock 1", move |updates| {
            let min_expected = timestamp_first_miniblock.load(Ordering::Relaxed);
            let actual = updates.miniblock.timestamp;
            assert!(
                actual > min_expected,
                "First miniblock: Timestamp cannot decrease. Expected at least {}, got {}",
                min_expected,
                actual
            );
            timestamp_first_miniblock.store(updates.miniblock.timestamp, Ordering::Relaxed);
        })
        .next_tx("Second tx", random_tx(2), successful_exec())
        .miniblock_sealed_with("Miniblock 2", move |updates| {
            let min_expected = timestamp_second_miniblock.load(Ordering::Relaxed);
            let actual = updates.miniblock.timestamp;
            assert!(
                actual > min_expected,
                "Second miniblock: Timestamp cannot decrease. Expected at least {}, got {}",
                min_expected,
                actual
            );
            timestamp_second_miniblock.store(updates.miniblock.timestamp, Ordering::Relaxed);
        })
        .batch_sealed_with("Batch 1", move |_, updates, _| {
            // Timestamp from the currently stored miniblock would be used in the fictive miniblock.
            // It should be correct as well.
            let min_expected = timestamp_third_miniblock.load(Ordering::Relaxed);
            let actual = updates.miniblock.timestamp;
            assert!(
                actual > min_expected,
                "Fictive miniblock: Timestamp cannot decrease. Expected at least {}, got {}",
                min_expected,
                actual
            );
            timestamp_third_miniblock.store(updates.miniblock.timestamp, Ordering::Relaxed);
        })
        .run(sealer)
        .await;
}
