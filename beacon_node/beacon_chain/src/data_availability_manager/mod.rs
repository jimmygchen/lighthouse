#[cfg(test)]
mod tests;

use crate::BeaconChainTypes;
use crate::beacon_chain::BeaconStore;
use crate::data_availability_checker::DataAvailabilityChecker;
use crate::errors::BeaconChainError as Error;
use crate::kzg_utils::reconstruct_blobs;
use crate::observed_data_sidecars::ObservedDataSidecars;
use kzg::Kzg;
use parking_lot::{Mutex, RwLock};
use rand::RngCore;
use std::collections::HashSet;
use std::sync::Arc;
use store::BlobSidecarListFromRoot;
use task_executor::TaskExecutor;
use tracing::error;
use types::data::{ColumnIndex, DataColumnSidecar, DataColumnSidecarList};
use types::*;

/// Manages data availability concerns: blob/column processing, custody boundary
/// calculations, and DA queries.
///
/// Generic over `T: BeaconChainTypes` because it needs store access for
/// persisting custody info and `TaskExecutor` for async operations.
///
/// State is passed as method parameters where possible. The component never
/// fetches head state or slot clock values on its own.
pub struct DataAvailabilityManager<T: BeaconChainTypes> {
    spec: Arc<ChainSpec>,
    store: BeaconStore<T>,
    #[allow(dead_code)]
    task_executor: TaskExecutor,
    data_availability_checker: Arc<DataAvailabilityChecker<T>>,
    #[allow(dead_code)]
    pub(crate) observed_blob_sidecars:
        RwLock<ObservedDataSidecars<BlobSidecar<T::EthSpec>, T::EthSpec>>,
    #[allow(dead_code)]
    pub(crate) observed_column_sidecars:
        RwLock<ObservedDataSidecars<DataColumnSidecar<T::EthSpec>, T::EthSpec>>,
    kzg: Arc<Kzg>,
    #[allow(dead_code)]
    rng: Arc<Mutex<Box<dyn RngCore + Send>>>,
}

impl<T: BeaconChainTypes> DataAvailabilityManager<T> {
    /// Create a new `DataAvailabilityManager`.
    pub fn new(
        spec: Arc<ChainSpec>,
        store: BeaconStore<T>,
        task_executor: TaskExecutor,
        data_availability_checker: Arc<DataAvailabilityChecker<T>>,
        kzg: Arc<Kzg>,
        rng: Arc<Mutex<Box<dyn RngCore + Send>>>,
    ) -> Self {
        Self {
            observed_blob_sidecars: RwLock::new(ObservedDataSidecars::new(spec.clone())),
            observed_column_sidecars: RwLock::new(ObservedDataSidecars::new(spec.clone())),
            spec,
            store,
            task_executor,
            data_availability_checker,
            kzg,
            rng,
        }
    }

    /// Return a reference to the inner `DataAvailabilityChecker`.
    pub fn data_availability_checker(&self) -> &Arc<DataAvailabilityChecker<T>> {
        &self.data_availability_checker
    }

    /// Return a reference to the KZG trusted setup.
    pub fn kzg(&self) -> &Arc<Kzg> {
        &self.kzg
    }

    /// Return a reference to the RNG.
    pub fn rng(&self) -> &Arc<Mutex<Box<dyn RngCore + Send>>> {
        &self.rng
    }

    // -----------------------------------------------------------------------
    // DA boundary and queries
    // -----------------------------------------------------------------------

    /// The epoch at which we require a data availability check in block processing.
    /// `None` if the `Deneb` fork is disabled.
    pub fn data_availability_boundary(&self) -> Option<Epoch> {
        self.data_availability_checker.data_availability_boundary()
    }

    /// Returns true if epoch is within the data availability boundary.
    pub fn da_check_required_for_epoch(&self, epoch: Epoch) -> bool {
        self.data_availability_checker
            .da_check_required_for_epoch(epoch)
    }

    /// Returns true if we should fetch blobs for this block.
    pub fn should_fetch_blobs(&self, block_epoch: Epoch) -> bool {
        self.da_check_required_for_epoch(block_epoch)
            && !self.spec.is_peer_das_enabled_for_epoch(block_epoch)
    }

    /// Returns true if we should fetch custody columns for this block.
    pub fn should_fetch_custody_columns(&self, block_epoch: Epoch) -> bool {
        self.da_check_required_for_epoch(block_epoch)
            && self.spec.is_peer_das_enabled_for_epoch(block_epoch)
    }

    /// Returns a list of column indices that should be sampled for a given epoch.
    /// Used for data availability sampling in PeerDAS.
    pub fn sampling_columns_for_epoch(&self, epoch: Epoch) -> &[ColumnIndex] {
        self.data_availability_checker
            .custody_context()
            .sampling_columns_for_epoch(epoch, &self.spec)
    }

    /// Returns a list of column indices that the node is expected to custody for a given epoch.
    /// i.e. the node must have validated and persisted the column samples and should be able to
    /// serve them to peers.
    ///
    /// If epoch is `None`, this function computes the custody columns at head.
    pub fn custody_columns_for_epoch(&self, epoch_opt: Option<Epoch>) -> &[ColumnIndex] {
        self.data_availability_checker
            .custody_context()
            .custody_columns_for_epoch(epoch_opt, &self.spec)
    }

    /// The data availability boundary for custodying columns. It will just be the
    /// regular data availability boundary unless we are near the Fulu fork epoch.
    pub fn column_data_availability_boundary(&self) -> Option<Epoch> {
        match self.data_availability_boundary() {
            Some(da_boundary_epoch) => {
                if let Some(fulu_fork_epoch) = self.spec.fulu_fork_epoch {
                    if da_boundary_epoch < fulu_fork_epoch {
                        Some(fulu_fork_epoch)
                    } else {
                        Some(da_boundary_epoch)
                    }
                } else {
                    None // Fulu hasn't been enabled
                }
            }
            None => None, // Deneb hasn't been enabled
        }
    }

    /// The DA boundary for custodying columns. It will just be the DA boundary
    /// unless we are near the Fulu fork epoch.
    pub fn get_column_da_boundary(&self) -> Option<Epoch> {
        match self.data_availability_boundary() {
            Some(da_boundary_epoch) => {
                if let Some(fulu_fork_epoch) = self.spec.fulu_fork_epoch {
                    if da_boundary_epoch < fulu_fork_epoch {
                        Some(fulu_fork_epoch)
                    } else {
                        Some(da_boundary_epoch)
                    }
                } else {
                    None
                }
            }
            None => None, // If no DA boundary set, dont try to custody backfill
        }
    }

    // -----------------------------------------------------------------------
    // Custody info (store-backed)
    // -----------------------------------------------------------------------

    /// Update data column custody info with the slot at which cgc was changed.
    pub fn update_data_column_custody_info(&self, slot: Option<Slot>) {
        self.store
            .put_data_column_custody_info(slot)
            .unwrap_or_else(|e| error!(error = ?e, "Failed to update data column custody info"));
    }

    /// Get the earliest epoch in which the node has met its custody requirements.
    /// A `None` response indicates that we've met our custody requirements up to the
    /// column data availability window.
    pub fn earliest_custodied_data_column_epoch(&self) -> Option<Epoch> {
        self.store
            .get_data_column_custody_info()
            .inspect_err(
                |e| error!(error=?e, "Failed to get data column custody info from the store"),
            )
            .ok()
            .flatten()
            .and_then(|info| info.earliest_data_column_slot)
            .map(|slot| {
                let mut epoch = slot.epoch(T::EthSpec::slots_per_epoch());
                // If the earliest custodied slot isn't the first slot in the epoch
                // The node has only met its custody requirements for the next epoch.
                if slot > epoch.start_slot(T::EthSpec::slots_per_epoch()) {
                    epoch += 1;
                }
                epoch
            })
    }

    /// Safely update data column custody info by ensuring that:
    /// - cgc values at the updated epoch and the earliest custodied column epoch are equal
    /// - we are only decrementing the earliest custodied data column epoch by one epoch
    /// - the new earliest data column slot is set to the first slot in `effective_epoch`.
    pub fn safely_backfill_data_column_custody_info(
        &self,
        effective_epoch: Epoch,
    ) -> Result<(), Error> {
        let Some(earliest_data_column_epoch) = self.earliest_custodied_data_column_epoch() else {
            return Ok(());
        };

        if effective_epoch >= earliest_data_column_epoch {
            return Ok(());
        }

        let cgc_at_effective_epoch = self
            .data_availability_checker
            .custody_context()
            .custody_group_count_at_epoch(effective_epoch, &self.spec);

        let cgc_at_earliest_data_colum_epoch = self
            .data_availability_checker
            .custody_context()
            .custody_group_count_at_epoch(earliest_data_column_epoch, &self.spec);

        let can_update_data_column_custody_info = cgc_at_effective_epoch
            == cgc_at_earliest_data_colum_epoch
            && effective_epoch == earliest_data_column_epoch - 1;

        if can_update_data_column_custody_info {
            self.store.put_data_column_custody_info(Some(
                effective_epoch.start_slot(T::EthSpec::slots_per_epoch()),
            ))?;
        } else {
            error!(
                ?cgc_at_effective_epoch,
                ?cgc_at_earliest_data_colum_epoch,
                ?effective_epoch,
                ?earliest_data_column_epoch,
                "Couldn't update data column custody info"
            );
            return Err(Error::FailedColumnCustodyInfoUpdate);
        }

        Ok(())
    }

    /// Compare columns custodied for `epoch` versus columns custodied for the head of the chain
    /// and return any column indices that are missing.
    pub fn get_missing_columns_for_epoch(&self, epoch: Epoch) -> HashSet<ColumnIndex> {
        let custody_context = self.data_availability_checker.custody_context();

        let columns_required = custody_context
            .custody_columns_for_epoch(None, &self.spec)
            .iter()
            .cloned()
            .collect::<HashSet<_>>();

        let current_columns_at_epoch = custody_context
            .custody_columns_for_epoch(Some(epoch), &self.spec)
            .iter()
            .cloned()
            .collect::<HashSet<_>>();

        columns_required
            .difference(&current_columns_at_epoch)
            .cloned()
            .collect::<HashSet<_>>()
    }

    // -----------------------------------------------------------------------
    // Data retrieval
    // -----------------------------------------------------------------------

    /// Returns the blobs at the given root, if any.
    ///
    /// ## Errors
    /// May return a database error.
    pub fn get_blobs(
        &self,
        block_root: &Hash256,
    ) -> Result<BlobSidecarListFromRoot<T::EthSpec>, Error> {
        self.store.get_blobs(block_root).map_err(Error::from)
    }

    /// Returns the data columns at the given root, if any.
    ///
    /// ## Errors
    /// May return a database error.
    pub fn get_data_columns(
        &self,
        block_root: &Hash256,
        fork_name: ForkName,
    ) -> Result<Option<DataColumnSidecarList<T::EthSpec>>, Error> {
        self.store
            .get_data_columns(block_root, fork_name)
            .map_err(Error::from)
    }

    /// Returns the data column at the given root and index, if any.
    ///
    /// ## Errors
    /// May return a database error.
    pub fn get_data_column(
        &self,
        block_root: &Hash256,
        column_index: &ColumnIndex,
        fork_name: ForkName,
    ) -> Result<Option<Arc<DataColumnSidecar<T::EthSpec>>>, Error> {
        Ok(self
            .store
            .get_data_column(block_root, column_index, fork_name)?)
    }

    /// Returns the blobs at the given root, if any.
    ///
    /// Uses the `block.epoch()` to determine whether to retrieve blobs or columns from the store.
    ///
    /// If at least 50% of columns are retrieved, blobs will be reconstructed and returned,
    /// otherwise an error `InsufficientColumnsToReconstructBlobs` is returned.
    ///
    /// ## Errors
    /// May return a database error.
    pub fn get_or_reconstruct_blobs(
        &self,
        block_root: &Hash256,
    ) -> Result<Option<BlobSidecarList<T::EthSpec>>, Error> {
        let Some(block) = self.store.get_blinded_block(block_root)? else {
            return Ok(None);
        };

        if self.spec.is_peer_das_enabled_for_epoch(block.epoch()) {
            let fork_name = self.spec.fork_name_at_epoch(block.epoch());
            if let Some(columns) = self.store.get_data_columns(block_root, fork_name)? {
                let num_required_columns = T::EthSpec::number_of_columns() / 2;
                let reconstruction_possible = columns.len() >= num_required_columns;
                if reconstruction_possible {
                    reconstruct_blobs(&self.kzg, columns, None, &block, &self.spec)
                        .map(Some)
                        .map_err(Error::FailedToReconstructBlobs)
                } else {
                    Err(Error::InsufficientColumnsToReconstructBlobs {
                        columns_found: columns.len(),
                    })
                }
            } else {
                Ok(None)
            }
        } else {
            Ok(self.get_blobs(block_root)?.blobs())
        }
    }
}
