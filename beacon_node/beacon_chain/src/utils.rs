//! Shared utilities for the `beacon_chain` crate.

use crate::errors::BeaconChainError as Error;
use task_executor::TaskExecutor;

/// Spawn a blocking task via the task executor.
pub async fn spawn_blocking_handle<F, R>(
    task_executor: &TaskExecutor,
    task: F,
    name: &'static str,
) -> Result<R, Error>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let handle = task_executor
        .spawn_blocking_handle(task, name)
        .ok_or(Error::RuntimeShutdown)?;
    handle.await.map_err(Error::TokioJoin)
}
