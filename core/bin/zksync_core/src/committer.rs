// Built-in uses
use std::time::{Duration, Instant};
// External uses
use anyhow::format_err;
use futures::channel::mpsc::{Receiver, Sender};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{task::JoinHandle, time};
// Workspace uses
use crate::mempool::MempoolBlocksRequest;
use zksync_storage::ConnectionPool;
use zksync_types::{
    block::{Block, ExecutedOperations, PendingBlock},
    AccountUpdates, Action, BlockNumber, Operation,
};

#[derive(Debug)]
pub enum CommitRequest {
    PendingBlock((PendingBlock, AppliedUpdatesRequest)),
    Block((BlockCommitRequest, AppliedUpdatesRequest)),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockCommitRequest {
    pub block: Block,
    pub accounts_updated: AccountUpdates,
}

#[derive(Clone, Debug)]
pub struct AppliedUpdatesRequest {
    pub account_updates: AccountUpdates,
    pub first_update_order_id: usize,
}

pub struct ExecutedOpsNotify {
    pub operations: Vec<ExecutedOperations>,
    pub block_number: BlockNumber,
}

const PROOF_POLL_INTERVAL: Duration = Duration::from_secs(1);

async fn handle_new_commit_task(
    mut rx_for_ops: Receiver<CommitRequest>,
    mut mempool_req_sender: Sender<MempoolBlocksRequest>,
    pool: ConnectionPool,
) {
    while let Some(request) = rx_for_ops.next().await {
        match request {
            CommitRequest::Block((block_commit_request, applied_updates_req)) => {
                commit_block(
                    block_commit_request,
                    applied_updates_req,
                    &pool,
                    &mut mempool_req_sender,
                )
                .await;
            }
            CommitRequest::PendingBlock((pending_block, applied_updates_req)) => {
                let mut operations = pending_block.success_operations.clone();
                operations.extend(
                    pending_block
                        .failed_txs
                        .clone()
                        .into_iter()
                        .map(|tx| ExecutedOperations::Tx(Box::new(tx))),
                );
                save_pending_block(pending_block, applied_updates_req, &pool).await;
            }
        }
    }
}

async fn save_pending_block(
    pending_block: PendingBlock,
    applied_updates_request: AppliedUpdatesRequest,
    pool: &ConnectionPool,
) {
    let start = Instant::now();
    let mut storage = pool
        .access_storage()
        .await
        .expect("db connection fail for committer");

    let mut transaction = storage
        .start_transaction()
        .await
        .expect("Failed initializing a DB transaction");

    let block_number = pending_block.number;

    vlog::trace!("persist pending block #{}", block_number);

    transaction
        .chain()
        .block_schema()
        .save_pending_block(pending_block)
        .await
        .expect("committer must commit the pending block into db");

    transaction
        .chain()
        .state_schema()
        .commit_state_update(
            block_number,
            &applied_updates_request.account_updates,
            applied_updates_request.first_update_order_id,
        )
        .await
        .expect("committer must commit the pending block into db");

    transaction
        .commit()
        .await
        .expect("Unable to commit DB transaction");

    metrics::histogram!("committer.save_pending_block", start.elapsed());
}

async fn commit_block(
    block_commit_request: BlockCommitRequest,
    applied_updates_request: AppliedUpdatesRequest,
    pool: &ConnectionPool,
    mempool_req_sender: &mut Sender<MempoolBlocksRequest>,
) {
    let start = Instant::now();
    let BlockCommitRequest {
        block,
        accounts_updated,
    } = block_commit_request;

    let mut storage = pool
        .access_storage()
        .await
        .expect("db connection fail for committer");

    let mut transaction = storage
        .start_transaction()
        .await
        .expect("Failed initializing a DB transaction");

    for exec_op in block.block_transactions.clone() {
        if let Some(exec_tx) = exec_op.get_executed_tx() {
            if exec_tx.success && exec_tx.signed_tx.tx.is_withdraw() {
                transaction
                    .chain()
                    .operations_schema()
                    .add_pending_withdrawal(&exec_tx.signed_tx.tx.hash(), None)
                    .await
                    .map_err(|e| {
                        format_err!(
                            "Failed to save pending withdrawal {:?}, error : {}",
                            exec_tx,
                            e
                        )
                    })
                    .expect("failed to save pending withdrawals into db");
            }
        }
    }

    // This is needed to keep track of how many priority ops are in each block
    // and trigger grafana alerts if there are suspiciously few
    let total_priority_ops = block
        .block_transactions
        .iter()
        .filter(|tx| matches!(tx, ExecutedOperations::PriorityOp(_)))
        .count();
    metrics::histogram!(
        "committer.priority_ops_per_block",
        total_priority_ops as u64
    );

    transaction
        .chain()
        .state_schema()
        .commit_state_update(
            block.block_number,
            &applied_updates_request.account_updates,
            applied_updates_request.first_update_order_id,
        )
        .await
        .expect("committer must commit the pending block into db");

    let op = Operation {
        action: Action::Commit,
        block,
        id: None,
    };
    vlog::info!("commit block #{}", op.block.block_number);
    transaction
        .chain()
        .block_schema()
        .execute_operation(op.clone())
        .await
        .expect("committer must commit the op into db");

    mempool_req_sender
        .send(MempoolBlocksRequest::UpdateNonces(accounts_updated))
        .await
        .map_err(|e| vlog::warn!("Failed notify mempool about account updates: {}", e))
        .unwrap_or_default();

    transaction
        .commit()
        .await
        .expect("Unable to commit DB transaction");

    metrics::histogram!("committer.commit_block", start.elapsed());
}

async fn poll_for_new_proofs_task(pool: ConnectionPool) {
    let mut last_verified_block = {
        let mut storage = pool
            .access_storage()
            .await
            .expect("db connection failed for committer");
        storage
            .chain()
            .block_schema()
            .get_last_verified_block()
            .await
            .expect("db failed")
    };

    let mut timer = time::interval(PROOF_POLL_INTERVAL);
    loop {
        timer.tick().await;

        let mut storage = pool
            .access_storage()
            .await
            .expect("db connection failed for committer");

        loop {
            let block_number = last_verified_block + 1;
            let proof = storage.prover_schema().load_proof(block_number).await;
            if let Ok(Some(proof)) = proof {
                let mut transaction = storage
                    .start_transaction()
                    .await
                    .expect("Unable to start DB transaction");

                vlog::info!("New proof for block: {}", block_number);
                let block = transaction
                    .chain()
                    .block_schema()
                    .load_committed_block(block_number)
                    .await
                    .unwrap_or_else(|| panic!("failed to load block #{}", *block_number));

                let op = Operation {
                    action: Action::Verify {
                        proof: Box::new(proof),
                    },
                    block,
                    id: None,
                };
                transaction
                    .chain()
                    .block_schema()
                    .execute_operation(op.clone())
                    .await
                    .expect("committer must commit the op into db");
                *last_verified_block += 1;

                transaction
                    .commit()
                    .await
                    .expect("Failed to commit transaction");
            } else {
                break;
            }
        }
    }
}

#[must_use]
pub fn run_committer(
    rx_for_ops: Receiver<CommitRequest>,
    mempool_req_sender: Sender<MempoolBlocksRequest>,
    pool: ConnectionPool,
) -> JoinHandle<()> {
    tokio::spawn(handle_new_commit_task(
        rx_for_ops,
        mempool_req_sender,
        pool.clone(),
    ));
    tokio::spawn(poll_for_new_proofs_task(pool))
}
