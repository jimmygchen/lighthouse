use crate::data_provider::{
    DataProviderError, LightClientDataProvider, MAX_REQUEST_LIGHT_CLIENT_UPDATES,
};
use crate::light_client::LightClientTypes;
use crate::store::LightClientStore;
use parking_lot::RwLock;
use safe_arith::ArithError;
use slog::{error, info, Logger};
use slot_clock::SlotClock;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use types::light_client_update::LightClientUpdate;
use types::{
    ChainSpec, FixedVector, ForkName, ForkVersionedResponse, Hash256, LightClientFinalityUpdate,
    LightClientHeader, LightClientOptimisticUpdate, Slot, SyncCommittee,
};

pub struct LightClientSyncService<T: LightClientTypes> {
    /// In memory storage for the light client state.
    store: Arc<RwLock<LightClientStore<T::EthSpec>>>,
    /// Provider to fetch light client data from.
    data_provider: Arc<T::DataProvider>,
    slot_clock: Arc<T::SlotClock>,
    genesis_validators_root: Hash256,
    log: Logger,
    spec: ChainSpec,
}

#[derive(Debug)]
pub enum Error {
    UnableToReadSlot,
    Arith(ArithError),
    UnsupportedFork(Option<ForkName>),
    DataProviderError(DataProviderError),
    NextSyncCommitteeNotKnown,
}

impl From<ArithError> for Error {
    fn from(e: ArithError) -> Self {
        Error::Arith(e)
    }
}

impl<T: LightClientTypes> LightClientSyncService<T> {
    pub fn new(
        store: Arc<RwLock<LightClientStore<T::EthSpec>>>,
        data_provider: Arc<T::DataProvider>,
        slot_clock: Arc<T::SlotClock>,
        genesis_validators_root: Hash256,
        log: Logger,
        spec: ChainSpec,
    ) -> Self {
        Self {
            store,
            data_provider,
            slot_clock,
            genesis_validators_root,
            log,
            spec,
        }
    }

    pub async fn start(self) {
        let spec = &self.spec;
        info!(self.log, "Starting light client sync service");
        loop {
            if let Err(e) = self.sync().await {
                error!(self.log, "Error occurred during sync"; "error" => ?e);
            }

            if let Err(e) = self.update().await {
                error!(self.log, "Error occurred during update"; "error" => ?e);
            }

            let slot_duration = Duration::from_secs(spec.seconds_per_slot);
            sleep(slot_duration).await;
        }
    }

    async fn update(&self) -> Result<(), Error> {
        let (optimistic_update_res, finality_update_res) = tokio::join!(
            self.get_light_client_optimistic_update(),
            self.get_light_client_finality_update()
        );
        optimistic_update_res?;
        finality_update_res
    }

    // FIXME: hack, don't read..
    async fn sync(&self) -> Result<(), Error> {
        let spec = &self.spec;
        let current_period = self.get_current_period()?;
        let optimistic_period = self.store.read().optimistic_period(spec)?;
        let mut finalized_period = self.store.read().finalized_period(spec)?;
        let is_next_sync_committee_known = self.store.read().is_next_sync_committee_known();

        if finalized_period == optimistic_period && is_next_sync_committee_known {
            self.get_light_client_update(finalized_period, 1).await?;
        } else if finalized_period == current_period {
            self.get_light_client_update(finalized_period, 1).await?;
        }

        while finalized_period + 1 < current_period {
            let start_period = finalized_period + 1;
            let count = std::cmp::min(
                MAX_REQUEST_LIGHT_CLIENT_UPDATES,
                current_period - start_period,
            );
            self.get_light_client_update(start_period, count).await?;
            finalized_period = self.store.read().finalized_period(spec)?;
        }

        Ok(())
    }

    async fn get_light_client_update(&self, start_period: u64, count: u64) -> Result<(), Error> {
        let light_client_updates = self
            .data_provider
            .get_light_client_updates(start_period, count)
            .await
            .map_err(Error::DataProviderError)?;

        info!(self.log, "Received light client updates";
            "start_period" => start_period,
            "count" => count,
            "received" => light_client_updates.len(),
        );

        for ForkVersionedResponse { version, data } in light_client_updates {
            match version.as_ref() {
                Some(ForkName::Altair) => Self::process_light_client_update(
                    self.store.clone(),
                    data,
                    self.slot_clock.now().ok_or(Error::UnableToReadSlot)?,
                    self.genesis_validators_root,
                    &self.spec,
                ),
                _ => Err(Error::UnsupportedFork(version)),
            }?;
        }

        Ok(())
    }

    async fn get_light_client_optimistic_update(&self) -> Result<(), Error> {
        let optimistic_update = self
            .data_provider
            .get_light_client_optimistic_update()
            .await
            .map_err(Error::DataProviderError)?;

        let ForkVersionedResponse { version, data } = optimistic_update;
        info!(self.log, "Received light client optimistic update"; "slot" => data.signature_slot,);

        match version.as_ref() {
            Some(ForkName::Altair) => Self::process_light_client_optimistic_update(
                self.store.clone(),
                data,
                self.slot_clock.now().ok_or(Error::UnableToReadSlot)?,
                self.genesis_validators_root,
                &self.spec,
            ),
            _ => Err(Error::UnsupportedFork(version)),
        }?;

        Ok(())
    }

    async fn get_light_client_finality_update(&self) -> Result<(), Error> {
        let optimistic_update = self
            .data_provider
            .get_light_client_finality_update()
            .await
            .map_err(Error::DataProviderError)?;

        let ForkVersionedResponse { version, data } = optimistic_update;
        info!(self.log, "Received light client finality update"; "slot" => data.signature_slot,);

        match version.as_ref() {
            Some(ForkName::Altair) => Self::process_light_client_finality_update(
                self.store.clone(),
                data,
                self.slot_clock.now().ok_or(Error::UnableToReadSlot)?,
                self.genesis_validators_root,
                &self.spec,
            ),
            _ => Err(Error::UnsupportedFork(version)),
        }?;

        Ok(())
    }

    fn get_current_period(&self) -> Result<u64, Error> {
        let spec = &self.spec;
        let current_slot = self.slot_clock.now().ok_or(Error::UnableToReadSlot)?;
        let current_period = current_slot
            .epoch(spec.seconds_per_slot)
            .sync_committee_period(spec)?;

        Ok(current_period)
    }

    fn process_light_client_update(
        store: Arc<RwLock<LightClientStore<T::EthSpec>>>,
        update: LightClientUpdate<T::EthSpec>,
        _current_slot: Slot,
        _genesis_validators_root: Hash256,
        spec: &ChainSpec,
    ) -> Result<(), Error> {
        Self::apply_light_client_update(store, update, spec)
    }

    fn process_light_client_optimistic_update(
        store: Arc<RwLock<LightClientStore<T::EthSpec>>>,
        optimistic_update: LightClientOptimisticUpdate<T::EthSpec>,
        current_slot: Slot,
        genesis_validators_root: Hash256,
        spec: &ChainSpec,
    ) -> Result<(), Error> {
        let update = LightClientUpdate {
            attested_header: optimistic_update.attested_header,
            next_sync_committee: Arc::new(SyncCommittee::temporary()),
            next_sync_committee_branch: FixedVector::default(),
            finalized_header: LightClientHeader::empty(),
            finality_branch: FixedVector::default(),
            sync_aggregate: optimistic_update.sync_aggregate,
            signature_slot: optimistic_update.signature_slot,
        };
        Self::process_light_client_update(
            store,
            update,
            current_slot,
            genesis_validators_root,
            spec,
        )
    }

    fn process_light_client_finality_update(
        store: Arc<RwLock<LightClientStore<T::EthSpec>>>,
        finality_update: LightClientFinalityUpdate<T::EthSpec>,
        current_slot: Slot,
        genesis_validators_root: Hash256,
        spec: &ChainSpec,
    ) -> Result<(), Error> {
        let update = LightClientUpdate {
            attested_header: finality_update.attested_header,
            next_sync_committee: Arc::new(SyncCommittee::temporary()),
            next_sync_committee_branch: FixedVector::default(),
            finalized_header: finality_update.finalized_header,
            finality_branch: finality_update.finality_branch,
            sync_aggregate: finality_update.sync_aggregate,
            signature_slot: finality_update.signature_slot,
        };
        Self::process_light_client_update(
            store,
            update,
            current_slot,
            genesis_validators_root,
            spec,
        )
    }

    fn apply_light_client_update(
        store: Arc<RwLock<LightClientStore<T::EthSpec>>>,
        update: LightClientUpdate<T::EthSpec>,
        spec: &ChainSpec,
    ) -> Result<(), Error> {
        let mut store = store.write();
        let store_period = store
            .finalized_header
            .beacon
            .slot
            .epoch(spec.seconds_per_slot)
            .sync_committee_period(spec)?;
        let update_finalized_period = update
            .finalized_header
            .beacon
            .slot
            .epoch(spec.seconds_per_slot)
            .sync_committee_period(spec)?;

        if !store.is_next_sync_committee_known() {
            // assert update_finalized_period == store_period
            store.next_sync_committee = update.next_sync_committee;
        } else if update_finalized_period == store_period + 1 {
            store.current_sync_committee = store.next_sync_committee.clone();
            store.next_sync_committee = update.next_sync_committee;
            store.previous_max_active_participants = store.current_max_active_participants;
            store.current_max_active_participants = 0;
        }

        if update.finalized_header.beacon.slot > store.finalized_header.beacon.slot {
            store.finalized_header = update.finalized_header;
            if store.finalized_header.beacon.slot > store.optimistic_header.beacon.slot {
                store.optimistic_header = store.finalized_header.clone();
            }
        }
        Ok(())
    }
}
