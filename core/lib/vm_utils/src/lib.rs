pub mod vm_env;

use anyhow::{anyhow, Context};
use multivm::{
    interface::{VmInterface, VmInterfaceHistoryEnabled},
    vm_latest::HistoryEnabled,
    HistoryMode, MultiVmTracerPointer, VmInstance,
};
use tokio::runtime::Handle;
use vm_env::{VmEnv, VmEnvBuilder};
use zksync_dal::StorageProcessor;
use zksync_state::{PostgresStorage, StoragePtr, StorageView, WriteStorage};
use zksync_types::{L1BatchNumber, L2ChainId, MiniblockNumber, Transaction};

type VmAndStorage<'a, H> = (
    VmInstance<StorageView<PostgresStorage<'a>>, H>,
    StoragePtr<StorageView<PostgresStorage<'a>>>,
);

pub fn execute_tx<S: WriteStorage>(
    tx: &Transaction,
    vm: &mut VmInstance<S, HistoryEnabled>,
    tracers: Vec<MultiVmTracerPointer<S, HistoryEnabled>>,
) -> anyhow::Result<()> {
    // Attempt to run VM with bytecode compression on.
    vm.make_snapshot();
    if vm
        .inspect_transaction_with_bytecode_compression(tracers.into(), tx.clone(), true)
        .0
        .is_ok()
    {
        vm.pop_snapshot_no_rollback();
        return Ok(());
    }

    // // If failed with bytecode compression, attempt to run without bytecode compression.
    // vm.rollback_to_the_latest_snapshot();
    // if vm
    //     .inspect_transaction_with_bytecode_compression(tracers.into(), tx.clone(), false)
    //     .0
    //     .is_err()
    // {
    //     return Err(anyhow!("compression can't fail if we don't apply it"));
    // }
    Ok(())
}

pub async fn prepare_vm_env_for_l1_batch(
    l1_batch_number: L1BatchNumber,
    l2_chain_id: L2ChainId,
    connection: &mut StorageProcessor<'_>,
) -> anyhow::Result<(VmEnv, MiniblockNumber)> {
    let prev_l1_batch_number = l1_batch_number - 1;
    let (_, miniblock_number) = connection
        .blocks_dal()
        .get_miniblock_range_of_l1_batch(prev_l1_batch_number)
        .await?
        .with_context(|| {
            format!(
                "l1_batch_number {l1_batch_number:?} must have a previous miniblock to start from"
            )
        })?;

    Ok((
        VmEnvBuilder::new(l1_batch_number, u32::MAX, l2_chain_id)
            .with_miniblock_number(miniblock_number)
            .build(connection)
            .await
            .with_context(|| {
                format!("failed to create vm env for l1_batch_number {l1_batch_number:?}")
            })?,
        miniblock_number,
    ))
}
