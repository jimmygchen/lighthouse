//! Fork digest utilities for ENR management and network fork transitions.

use crate::beacon_chain::BeaconChainTypes;
use slot_clock::SlotClock;
use std::time::Duration;
use types::*;

/// Returns the current ENR fork ID for the chain.
///
/// If the slot clock cannot be read, the genesis slot is used.
pub fn enr_fork_id<T: BeaconChainTypes>(
    slot_clock: &T::SlotClock,
    spec: &ChainSpec,
    genesis_validators_root: Hash256,
) -> EnrForkId {
    let slot = slot_clock.now().unwrap_or(spec.genesis_slot);
    spec.enr_fork_id::<T::EthSpec>(slot, genesis_validators_root)
}

/// Returns the fork digest corresponding to an epoch.
///
/// See [`ChainSpec::compute_fork_digest`].
pub fn compute_fork_digest(
    spec: &ChainSpec,
    genesis_validators_root: Hash256,
    epoch: Epoch,
) -> [u8; 4] {
    spec.compute_fork_digest(genesis_validators_root, epoch)
}

/// Calculates the `Duration` to the next fork digest (this could be either a regular or BPO
/// hard fork) if it exists and returns it with its corresponding `Epoch`.
pub fn duration_to_next_digest<T: BeaconChainTypes>(
    slot_clock: &T::SlotClock,
    spec: &ChainSpec,
) -> Option<(Epoch, Duration)> {
    let slot = slot_clock.now().unwrap_or(spec.genesis_slot);
    let epoch = slot.epoch(T::EthSpec::slots_per_epoch());

    let next_digest_epoch = spec.next_digest_epoch(epoch)?;
    let next_digest_slot = next_digest_epoch.start_slot(T::EthSpec::slots_per_epoch());

    slot_clock
        .duration_to_slot(next_digest_slot)
        .map(|duration| (next_digest_epoch, duration))
}
