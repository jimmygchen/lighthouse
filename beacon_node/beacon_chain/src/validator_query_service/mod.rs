#[cfg(test)]
mod tests;

use crate::BeaconChainTypes;
use crate::errors::BeaconChainError as Error;
use crate::validator_pubkey_cache::ValidatorPubkeyCache;
use bls::{PublicKey, PublicKeyBytes};
use parking_lot::RwLock;
use std::collections::HashMap;

/// Handles validator public key and index lookups from the validator pubkey
/// cache.
///
/// Generic over `T: BeaconChainTypes` because the underlying
/// `ValidatorPubkeyCache` requires store access for persistence.
///
/// State is passed as method parameters -- this component never fetches head
/// state, slot clock values, or similar chain-level context on its own.
pub struct ValidatorQueryService<T: BeaconChainTypes> {
    pub(crate) validator_pubkey_cache: RwLock<ValidatorPubkeyCache<T>>,
}

impl<T: BeaconChainTypes> ValidatorQueryService<T> {
    /// Create a new `ValidatorQueryService`.
    pub fn new(validator_pubkey_cache: ValidatorPubkeyCache<T>) -> Self {
        Self {
            validator_pubkey_cache: RwLock::new(validator_pubkey_cache),
        }
    }

    /// Return the validator index (if any) for the given public key.
    ///
    /// This query uses the `validator_pubkey_cache` which contains _all_ validators ever seen,
    /// even if those validators aren't included in the head state. It is important to remember
    /// that just because a validator exists here, it doesn't necessarily exist in all
    /// `BeaconStates`.
    pub fn validator_index(&self, pubkey: &PublicKeyBytes) -> Result<Option<usize>, Error> {
        let pubkey_cache = self.validator_pubkey_cache.read();
        Ok(pubkey_cache.get_index(pubkey))
    }

    /// Return the validator indices of all public keys fetched from an iterator.
    ///
    /// If any public key doesn't belong to a known validator then an error will be returned.
    /// We could consider relaxing this by returning `Vec<Option<usize>>` in future.
    pub fn validator_indices<'a>(
        &self,
        validator_pubkeys: impl Iterator<Item = &'a PublicKeyBytes>,
    ) -> Result<Vec<u64>, Error> {
        let pubkey_cache = self.validator_pubkey_cache.read();

        validator_pubkeys
            .map(|pubkey| {
                pubkey_cache
                    .get_index(pubkey)
                    .map(|id| id as u64)
                    .ok_or(Error::ValidatorPubkeyUnknown(*pubkey))
            })
            .collect()
    }

    /// Returns the validator pubkey (if any) for the given validator index.
    ///
    /// This query uses the `validator_pubkey_cache` which contains _all_ validators ever seen,
    /// even if those validators aren't included in the head state. It is important to remember
    /// that just because a validator exists here, it doesn't necessarily exist in all
    /// `BeaconStates`.
    pub fn validator_pubkey(&self, validator_index: usize) -> Result<Option<PublicKey>, Error> {
        let pubkey_cache = self.validator_pubkey_cache.read();
        Ok(pubkey_cache.get(validator_index).cloned())
    }

    /// As per `Self::validator_pubkey`, but returns `PublicKeyBytes`.
    pub fn validator_pubkey_bytes(
        &self,
        validator_index: usize,
    ) -> Result<Option<PublicKeyBytes>, Error> {
        let pubkey_cache = self.validator_pubkey_cache.read();
        Ok(pubkey_cache.get_pubkey_bytes(validator_index).copied())
    }

    /// As per `Self::validator_pubkey_bytes` but will resolve multiple indices at once to avoid
    /// bouncing the read-lock on the pubkey cache.
    ///
    /// Returns a map that may have a length less than `validator_indices.len()` if some indices
    /// were unable to be resolved.
    pub fn validator_pubkey_bytes_many(
        &self,
        validator_indices: &[usize],
    ) -> Result<HashMap<usize, PublicKeyBytes>, Error> {
        let pubkey_cache = self.validator_pubkey_cache.read();

        let mut map = HashMap::with_capacity(validator_indices.len());
        for &validator_index in validator_indices {
            if let Some(pubkey) = pubkey_cache.get_pubkey_bytes(validator_index) {
                map.insert(validator_index, *pubkey);
            }
        }
        Ok(map)
    }
}
