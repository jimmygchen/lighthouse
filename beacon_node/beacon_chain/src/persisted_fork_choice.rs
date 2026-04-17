use crate::beacon_fork_choice_store::PersistedForkChoiceStoreV28;
use crate::errors::BeaconChainError;
use crate::{BeaconChainTypes, BeaconForkChoiceStore, metrics};
use fork_choice::{ForkChoice, ResetPayloadStatuses};
use ssz::{Decode, Encode};
use ssz_derive::{Decode, Encode};
use store::{DBColumn, Error, KeyValueStore, KeyValueStoreOp, StoreConfig};
use superstruct::superstruct;
use types::{ChainSpec, Hash256};

pub type BeaconForkChoice<T> = ForkChoice<
    BeaconForkChoiceStore<
        <T as BeaconChainTypes>::EthSpec,
        <T as BeaconChainTypes>::HotStore,
        <T as BeaconChainTypes>::ColdStore,
    >,
    <T as BeaconChainTypes>::EthSpec,
>;

pub use crate::beacon_chain::FORK_CHOICE_DB_KEY;

// If adding a new version you should update this type alias and fix the breakages.
pub type PersistedForkChoice = PersistedForkChoiceV29;

#[superstruct(
    variants(V28, V29),
    variant_attributes(derive(Encode, Decode)),
    no_enum
)]
pub struct PersistedForkChoice {
    #[superstruct(only(V28))]
    pub fork_choice_v28: fork_choice::PersistedForkChoiceV28,
    #[superstruct(only(V29))]
    pub fork_choice: fork_choice::PersistedForkChoiceV29,
    #[superstruct(only(V28, V29))]
    pub fork_choice_store: PersistedForkChoiceStoreV28,
}

impl PersistedForkChoiceV28 {
    pub fn from_bytes(bytes: &[u8], store_config: &StoreConfig) -> Result<Self, Error> {
        let decompressed_bytes = store_config
            .decompress_bytes(bytes)
            .map_err(Error::Compression)?;
        Self::from_ssz_bytes(&decompressed_bytes).map_err(Into::into)
    }

    pub fn as_bytes(&self, store_config: &StoreConfig) -> Result<Vec<u8>, Error> {
        let encode_timer = metrics::start_timer(&metrics::FORK_CHOICE_ENCODE_TIMES);
        let ssz_bytes = self.as_ssz_bytes();
        drop(encode_timer);

        let _compress_timer = metrics::start_timer(&metrics::FORK_CHOICE_COMPRESS_TIMES);
        store_config
            .compress_bytes(&ssz_bytes)
            .map_err(Error::Compression)
    }

    pub fn as_kv_store_op(
        &self,
        key: Hash256,
        store_config: &StoreConfig,
    ) -> Result<KeyValueStoreOp, Error> {
        Ok(KeyValueStoreOp::PutKeyValue(
            DBColumn::ForkChoice,
            key.as_slice().to_vec(),
            self.as_bytes(store_config)?,
        ))
    }
}

impl PersistedForkChoiceV29 {
    pub fn from_bytes(bytes: &[u8], store_config: &StoreConfig) -> Result<Self, Error> {
        let decompressed_bytes = store_config
            .decompress_bytes(bytes)
            .map_err(Error::Compression)?;
        Self::from_ssz_bytes(&decompressed_bytes).map_err(Into::into)
    }

    pub fn as_bytes(&self, store_config: &StoreConfig) -> Result<Vec<u8>, Error> {
        let encode_timer = metrics::start_timer(&metrics::FORK_CHOICE_ENCODE_TIMES);
        let ssz_bytes = self.as_ssz_bytes();
        drop(encode_timer);

        let _compress_timer = metrics::start_timer(&metrics::FORK_CHOICE_COMPRESS_TIMES);
        store_config
            .compress_bytes(&ssz_bytes)
            .map_err(Error::Compression)
    }

    pub fn as_kv_store_op(
        &self,
        key: Hash256,
        store_config: &StoreConfig,
    ) -> Result<KeyValueStoreOp, Error> {
        Ok(KeyValueStoreOp::PutKeyValue(
            DBColumn::ForkChoice,
            key.as_slice().to_vec(),
            self.as_bytes(store_config)?,
        ))
    }
}

impl From<PersistedForkChoiceV28> for PersistedForkChoiceV29 {
    fn from(v28: PersistedForkChoiceV28) -> Self {
        Self {
            fork_choice: v28.fork_choice_v28.into(),
            fork_choice_store: v28.fork_choice_store,
        }
    }
}

impl From<PersistedForkChoiceV29> for PersistedForkChoiceV28 {
    fn from(v29: PersistedForkChoiceV29) -> Self {
        Self {
            fork_choice_v28: v29.fork_choice.into(),
            fork_choice_store: v29.fork_choice_store,
        }
    }
}

/// Load fork choice from disk, returning `None` if it isn't found.
pub fn load_fork_choice<T: BeaconChainTypes>(
    store: crate::beacon_chain::BeaconStore<T>,
    reset_payload_statuses: ResetPayloadStatuses,
    spec: &ChainSpec,
) -> Result<Option<BeaconForkChoice<T>>, BeaconChainError> {
    let Some(persisted_fork_choice_bytes) = store
        .hot_db
        .get_bytes(DBColumn::ForkChoice, FORK_CHOICE_DB_KEY.as_slice())?
    else {
        return Ok(None);
    };

    let persisted_fork_choice =
        PersistedForkChoice::from_bytes(&persisted_fork_choice_bytes, store.get_config())?;
    let fc_store = crate::BeaconForkChoiceStore::from_persisted(
        persisted_fork_choice.fork_choice_store,
        store,
    )?;

    Ok(Some(ForkChoice::from_persisted(
        persisted_fork_choice.fork_choice,
        reset_payload_statuses,
        fc_store,
        spec,
    )?))
}
