use ssz::{Decode, Encode};
use ssz_derive::{Decode, Encode};
use store::{DBColumn, Error as StoreError, KeyValueStoreOp, StoreItem};
use types::Hash256;

// This key is all zero because it gets stored in its own column, see `DBColumn` type.
pub const BEACON_CHAIN_DB_KEY: Hash256 = Hash256::ZERO;

/// Return a database operation for writing the `PersistedBeaconChain` to disk.
///
/// These days the `PersistedBeaconChain` is only used to store the genesis block root, so it
/// should only ever be written once at startup.
pub fn persist_head_in_batch_standalone(genesis_block_root: Hash256) -> KeyValueStoreOp {
    PersistedBeaconChain { genesis_block_root }.as_kv_store_op(BEACON_CHAIN_DB_KEY)
}

#[derive(Clone, Encode, Decode)]
pub struct PersistedBeaconChain {
    pub genesis_block_root: Hash256,
}

impl StoreItem for PersistedBeaconChain {
    fn db_column() -> DBColumn {
        DBColumn::BeaconChain
    }

    fn as_store_bytes(&self) -> Vec<u8> {
        self.as_ssz_bytes()
    }

    fn from_store_bytes(bytes: &[u8]) -> Result<Self, StoreError> {
        Self::from_ssz_bytes(bytes).map_err(Into::into)
    }
}
