use std::sync::Arc;

use anyhow::Context;
use multivm::{
    interface::{ExecutionResult, VmInterface},
    tracers::CallTracer,
    vm_latest::constants::BLOCK_GAS_LIMIT,
    zk_evm_1_3_1::zkevm_opcode_defs::decoding::AllowedPcOrImm,
    zk_evm_1_4_1::sha3::digest::typenum::Min,
    MultiVMTracer, VmInstance,
};
use once_cell::sync::OnceCell;
use vm_utils::{execute_tx, vm_env::VmEnvBuilder};
use zksync_contracts::BaseSystemContracts;
use zksync_dal::connection;
use zksync_state::{PostgresStorage, StorageView};
use zksync_system_constants::MAX_ENCODED_TX_SIZE;
use zksync_types::{
    api::{BlockId, BlockNumber, DebugCall, ResultDebugCall, TracerConfig},
    fee_model::BatchFeeInput,
    l2::L2Tx,
    transaction_request::CallRequest,
    vm_trace::Call,
    AccountTreeId, L1BatchNumber, MiniblockNumber, H256,
};
use zksync_web3_decl::error::Web3Error;

use crate::api_server::{
    execution_sandbox::{ApiTracer, TxSharedArgs, VmConcurrencyLimiter},
    tx_sender::{ApiContracts, TxSenderConfig},
    web3::{backend_jsonrpsee::internal_error, metrics::API_METRICS, state::RpcState},
};

#[derive(Debug, Clone)]
pub struct DebugNamespace {
    batch_fee_input: BatchFeeInput,
    state: RpcState,
    api_contracts: ApiContracts,
}

impl DebugNamespace {
    pub async fn new(state: RpcState) -> Self {
        let api_contracts = ApiContracts::load_from_disk();
        Self {
            // For now, the same scaling is used for both the L1 gas price and the pubdata price
            batch_fee_input: state
                .tx_sender
                .0
                .batch_fee_input_provider
                .get_batch_fee_input_scaled(
                    state.api_config.estimate_gas_scale_factor,
                    state.api_config.estimate_gas_scale_factor,
                )
                .await,
            state,
            api_contracts,
        }
    }

    fn sender_config(&self) -> &TxSenderConfig {
        &self.state.tx_sender.0.sender_config
    }

    #[tracing::instrument(skip(self))]
    pub async fn debug_trace_block_impl(
        &self,
        block_id: BlockId,
        options: Option<TracerConfig>,
    ) -> Result<Vec<ResultDebugCall>, Web3Error> {
        const METHOD_NAME: &str = "debug_trace_block";

        let method_latency = API_METRICS.start_block_call(METHOD_NAME, block_id);
        let only_top_call = options
            .map(|options| options.tracer_config.only_top_call)
            .unwrap_or(false);
        let mut connection = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await
            .map_err(|err| internal_error(METHOD_NAME, err))?;
        let block_number = self
            .state
            .resolve_block(&mut connection, block_id, METHOD_NAME)
            .await?;
        let call_traces = connection
            .blocks_web3_dal()
            .get_trace_for_miniblock(block_number) // FIXME: is some ordering among transactions expected?
            .await
            .map_err(|err| internal_error(METHOD_NAME, err))?;
        let call_trace = call_traces
            .into_iter()
            .map(|call_trace| {
                let mut result: DebugCall = call_trace.into();
                if only_top_call {
                    result.calls = vec![];
                }
                ResultDebugCall { result }
            })
            .collect();

        let block_diff = self.state.last_sealed_miniblock.diff(block_number);
        method_latency.observe(block_diff);
        Ok(call_trace)
    }

    #[tracing::instrument(skip(self))]
    pub async fn debug_trace_transaction_impl(
        &self,
        tx_hash: H256,
        options: Option<TracerConfig>,
    ) -> Result<Option<DebugCall>, Web3Error> {
        const METHOD_NAME: &str = "debug_trace_transaction";

        let only_top_call = options
            .map(|options| options.tracer_config.only_top_call)
            .unwrap_or(false);
        let mut connection = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await
            .map_err(|err| internal_error(METHOD_NAME, err))?;

        let receipt = connection
            .transactions_web3_dal()
            .get_transaction_receipt(tx_hash)
            .await
            .unwrap()
            .unwrap();

        let miniblock_data = connection
            .transactions_dal()
            .get_miniblock_to_execute(MiniblockNumber(receipt.block_number.as_u32()), Some(1))
            .await
            .unwrap()
            .unwrap();

        let tx_hash = miniblock_data.txs.last().unwrap().hash();
        let receipt = connection
            .transactions_web3_dal()
            .get_transaction_receipt(tx_hash)
            .await
            .unwrap()
            .unwrap();
        let call_trace = connection
            .transactions_dal()
            .get_call_trace(tx_hash)
            .await
            .unwrap()
            .calls;
        let mut vm_env = VmEnvBuilder::new(
            L1BatchNumber(receipt.l1_batch_number.unwrap().as_u32()),
            BLOCK_GAS_LIMIT,
            self.state.api_config.l2_chain_id,
        )
        .with_miniblock_number(miniblock_data.number)
        .with_base_system_contracts(BaseSystemContracts::playground())
        .build(&mut connection)
        .await
        .unwrap();
        let vm_permit = self
            .state
            .tx_sender
            .vm_concurrency_limiter()
            .acquire()
            .await
            .unwrap();
        drop(connection);
        let connection_pool = self.state.connection_pool.clone();
        let call_trace = tokio::task::spawn_blocking(move || {
            let rt_handle = vm_permit.rt_handle().clone();
            let connection = rt_handle
                .block_on(connection_pool.access_storage())
                .unwrap();
            let pg_storage = PostgresStorage::new(
                rt_handle.clone(),
                connection,
                miniblock_data.number - 1,
                true,
            );

            let storage_view = StorageView::new(pg_storage).to_rc_ptr();
            vm_env.l1_batch_env.previous_batch_hash = None;
            let mut vm =
                VmInstance::new(vm_env.l1_batch_env, vm_env.system_env, storage_view.clone());
            for tx in &miniblock_data.txs[..receipt.transaction_index.as_u64() as usize] {
                tracing::trace!("Started execution of tx: {tx:?}");
                execute_tx(tx, &mut vm, vec![]).unwrap();
                tracing::trace!("Finished execution of tx: {tx:?}");
            }
            let call_tracer_result = Arc::new(OnceCell::default());
            let call_tracer = CallTracer::new(call_tracer_result.clone());
            let last_tx = miniblock_data.txs.last().unwrap();
            execute_tx(last_tx, &mut vm, vec![call_tracer.into_tracer_pointer()]).unwrap();
            let trace = Arc::try_unwrap(call_tracer_result)
                .unwrap()
                .take()
                .unwrap_or_default();

            assert_eq!(trace, call_trace);
            let call_trace = Call::new_high_level(
                last_tx.gas_limit().as_u32(),
                receipt.gas_used.unwrap().as_u32(),
                last_tx.execute.value,
                last_tx.execute.calldata.clone(),
                vec![],
                None,
                trace,
            );
            print!("fuck");
            call_trace
        })
        .await
        .unwrap();
        let mut result: DebugCall = call_trace.into();
        if only_top_call {
            result.calls = vec![];
        }
        Ok(Some(result))
    }

    #[tracing::instrument(skip(self, request, block_id))]
    pub async fn debug_trace_call_impl(
        &self,
        request: CallRequest,
        block_id: Option<BlockId>,
        options: Option<TracerConfig>,
    ) -> Result<DebugCall, Web3Error> {
        const METHOD_NAME: &str = "debug_trace_call";

        let block_id = block_id.unwrap_or(BlockId::Number(BlockNumber::Pending));
        let method_latency = API_METRICS.start_block_call(METHOD_NAME, block_id);
        let only_top_call = options
            .map(|options| options.tracer_config.only_top_call)
            .unwrap_or(false);

        let mut connection = self
            .state
            .connection_pool
            .access_storage_tagged("api")
            .await
            .map_err(|err| internal_error(METHOD_NAME, err))?;
        let block_args = self
            .state
            .resolve_block_args(&mut connection, block_id, METHOD_NAME)
            .await?;
        drop(connection);

        let tx = L2Tx::from_request(request.into(), MAX_ENCODED_TX_SIZE)?;

        let shared_args = self.shared_args();
        let vm_permit = self
            .state
            .tx_sender
            .vm_concurrency_limiter()
            .acquire()
            .await;
        let vm_permit = vm_permit.ok_or(Web3Error::InternalError)?;

        // We don't need properly trace if we only need top call
        let call_tracer_result = Arc::new(OnceCell::default());
        let custom_tracers = if only_top_call {
            vec![]
        } else {
            vec![ApiTracer::CallTracer(call_tracer_result.clone())]
        };

        let executor = &self.state.tx_sender.0.executor;
        let result = executor
            .execute_tx_eth_call(
                vm_permit,
                shared_args,
                self.state.connection_pool.clone(),
                tx.clone(),
                block_args,
                self.sender_config().vm_execution_cache_misses_limit,
                custom_tracers,
            )
            .await
            .map_err(|err| internal_error(METHOD_NAME, err))?;

        let (output, revert_reason) = match result.result {
            ExecutionResult::Success { output, .. } => (output, None),
            ExecutionResult::Revert { output } => (vec![], Some(output.to_string())),
            ExecutionResult::Halt { reason } => {
                return Err(Web3Error::SubmitTransactionError(
                    reason.to_string(),
                    vec![],
                ))
            }
        };

        // We had only one copy of Arc this arc is already dropped it's safe to unwrap
        let trace = Arc::try_unwrap(call_tracer_result)
            .unwrap()
            .take()
            .unwrap_or_default();
        let call = Call::new_high_level(
            tx.common_data.fee.gas_limit.as_u32(),
            result.statistics.gas_used,
            tx.execute.value,
            tx.execute.calldata,
            output,
            revert_reason,
            trace,
        );

        let block_diff = self
            .state
            .last_sealed_miniblock
            .diff_with_block_args(&block_args);
        method_latency.observe(block_diff);
        Ok(call.into())
    }

    fn shared_args(&self) -> TxSharedArgs {
        let sender_config = self.sender_config();
        TxSharedArgs {
            operator_account: AccountTreeId::default(),
            fee_input: self.batch_fee_input,
            base_system_contracts: self.api_contracts.eth_call.clone(),
            caches: self.state.tx_sender.storage_caches().clone(),
            validation_computational_gas_limit: BLOCK_GAS_LIMIT,
            chain_id: sender_config.chain_id,
        }
    }
}
