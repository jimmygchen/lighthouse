use derivative::Derivative;
use slot_clock::SlotClock;
use std::sync::Arc;

use crate::beacon_chain::{
    BeaconChain, BeaconChainTypes, MAXIMUM_GOSSIP_CLOCK_DISPARITY,
    VALIDATOR_PUBKEY_CACHE_LOCK_TIMEOUT,
};
use crate::{kzg_utils, BeaconChainError};
use state_processing::per_block_processing::eip4844::eip4844::verify_kzg_commitments_against_transactions;
use types::signed_beacon_block::BlobReconstructionError;
use types::{
    BeaconBlockRef, BeaconStateError, BlobsSidecar, EthSpec, Hash256, KzgCommitment,
    SignedBeaconBlock, SignedBeaconBlockAndBlobsSidecar, SignedBeaconBlockHeader,
    SignedBlobSidecar, Slot, Transactions,
};
use types::{Epoch, ExecPayload};

#[derive(Debug)]
pub enum BlobError {
    /// The blob sidecar is from a slot that is later than the current slot (with respect to the
    /// gossip clock disparity).
    ///
    /// ## Peer scoring
    ///
    /// Assuming the local clock is correct, the peer has sent an invalid message.
    FutureSlot {
        message_slot: Slot,
        latest_permissible_slot: Slot,
    },

    /// The blob sidecar has a different slot than the block.
    ///
    /// ## Peer scoring
    ///
    /// Assuming the local clock is correct, the peer has sent an invalid message.
    SlotMismatch {
        blob_slot: Slot,
        block_slot: Slot,
    },

    /// No kzg ccommitment associated with blob sidecar.
    KzgCommitmentMissing,

    /// No transactions in block
    TransactionsMissing,

    /// Blob transactions in the block do not correspond to the kzg commitments.
    TransactionCommitmentMismatch,

    TrustedSetupNotInitialized,

    InvalidKzgProof,

    KzgError(kzg::Error),

    /// There was an error whilst processing the sync contribution. It is not known if it is valid or invalid.
    ///
    /// ## Peer scoring
    ///
    /// We were unable to process this sync committee message due to an internal error. It's unclear if the
    /// sync committee message is valid.
    BeaconChainError(BeaconChainError),
    /// No blobs for the specified block where we would expect blobs.
    UnavailableBlobs,
    /// Blobs provided for a pre-Eip4844 fork.
    InconsistentFork,

    /// The `blobs_sidecar.message.beacon_block_root` block is unknown.
    ///
    /// ## Peer scoring
    ///
    /// The blob points to a block we have not yet imported. The blob cannot be imported
    /// into fork choice yet
    UnknownHeadBlock {
        beacon_block_root: Hash256,
    },

    /// The `BlobSidecar` was gossiped over an incorrect subnet.
    InvalidSubnet {
        expected: u64,
        received: u64,
    },

    /// The sidecar corresponds to a slot older than the finalized head slot.
    PastFinalizedSlot {
        blob_slot: Slot,
        finalized_slot: Slot,
    },

    /// The proposer index specified in the sidecar does not match the locally computed
    /// proposer index.
    ProposerIndexMismatch {
        sidecar: usize,
        local: usize,
    },

    ProposerSignatureInvalid,

    /// A sidecar with same slot, beacon_block_root and proposer_index but different blob is received for
    /// the same blob index.
    RepeatSidecar {
        proposer: usize,
        slot: Slot,
        blob_index: usize,
    },

    /// The proposal_index corresponding to blob.beacon_block_root is not known.
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer is faulty.
    UnknownValidator(u64),
}

impl From<BlobReconstructionError> for BlobError {
    fn from(e: BlobReconstructionError) -> Self {
        match e {
            BlobReconstructionError::UnavailableBlobs => BlobError::UnavailableBlobs,
            BlobReconstructionError::InconsistentFork => BlobError::InconsistentFork,
        }
    }
}

impl From<BeaconChainError> for BlobError {
    fn from(e: BeaconChainError) -> Self {
        BlobError::BeaconChainError(e)
    }
}

impl From<BeaconStateError> for BlobError {
    fn from(e: BeaconStateError) -> Self {
        BlobError::BeaconChainError(BeaconChainError::BeaconStateError(e))
    }
}

pub fn validate_blob_for_gossip<T: BeaconChainTypes>(
    block_wrapper: BlockWrapper<T::EthSpec>,
    block_root: Hash256,
    chain: &BeaconChain<T>,
) -> Result<AvailableBlock<T::EthSpec>, BlobError> {
    if let BlockWrapper::BlockAndBlob(ref block, ref blobs_sidecar) = block_wrapper {
        let blob_slot = blobs_sidecar.beacon_block_slot;
        // Do not gossip or process blobs from future or past slots.
        let latest_permissible_slot = chain
            .slot_clock
            .now_with_future_tolerance(MAXIMUM_GOSSIP_CLOCK_DISPARITY)
            .ok_or(BeaconChainError::UnableToReadSlot)?;
        if blob_slot > latest_permissible_slot {
            return Err(BlobError::FutureSlot {
                message_slot: latest_permissible_slot,
                latest_permissible_slot: blob_slot,
            });
        }

        if blob_slot != block.slot() {
            return Err(BlobError::SlotMismatch {
                blob_slot,
                block_slot: block.slot(),
            });
        }
    }

    block_wrapper.into_available_block(block_root, chain)
}

pub fn validate_blob_sidecar_for_gossip<T: BeaconChainTypes>(
    blob_sidecar: SignedBlobSidecar<T::EthSpec>,
    subnet: u64,
    chain: &BeaconChain<T>,
) -> Result<(), BlobError> {
    let blob_slot = blob_sidecar.message.slot;
    let blob_index = blob_sidecar.message.index;
    let block_root = blob_sidecar.message.block_root;

    // Verify that the blob_sidecar was received on the correct subnet.
    if blob_index != subnet {
        return Err(BlobError::InvalidSubnet {
            expected: blob_index,
            received: subnet,
        });
    }

    // Verify that the sidecar is not from a future slot.
    let latest_permissible_slot = chain
        .slot_clock
        .now_with_future_tolerance(MAXIMUM_GOSSIP_CLOCK_DISPARITY)
        .ok_or(BeaconChainError::UnableToReadSlot)?;
    if blob_slot > latest_permissible_slot {
        return Err(BlobError::FutureSlot {
            message_slot: blob_slot,
            latest_permissible_slot,
        });
    }

    // TODO(pawan): Verify not from a past slot?

    // Verify that the sidecar slot is greater than the latest finalized slot
    let latest_finalized_slot = chain
        .head()
        .finalized_checkpoint()
        .epoch
        .start_slot(T::EthSpec::slots_per_epoch());
    if blob_slot <= latest_finalized_slot {
        return Err(BlobError::PastFinalizedSlot {
            blob_slot,
            finalized_slot: latest_finalized_slot,
        });
    }

    // TODO(pawan): should we verify locally that the parent root is correct
    // or just use whatever the proposer gives us?
    let proposer_shuffling_root = blob_sidecar.message.block_parent_root;

    let (proposer_index, fork) = match chain
        .beacon_proposer_cache
        .lock()
        .get_slot::<T::EthSpec>(proposer_shuffling_root, blob_slot)
    {
        Some(proposer) => (proposer.index, proposer.fork),
        None => {
            let state = &chain.canonical_head.cached_head().snapshot.beacon_state;
            (
                state.get_beacon_proposer_index(blob_slot, &chain.spec)?,
                state.fork(),
            )
        }
    };

    let blob_proposer_index = blob_sidecar.message.proposer_index;
    if proposer_index != blob_proposer_index {
        return Err(BlobError::ProposerIndexMismatch {
            sidecar: blob_proposer_index,
            local: proposer_index,
        });
    }

    let signature_is_valid = {
        let pubkey_cache = chain
            .validator_pubkey_cache
            .try_read_for(VALIDATOR_PUBKEY_CACHE_LOCK_TIMEOUT)
            .ok_or(BeaconChainError::ValidatorPubkeyCacheLockTimeout)
            .map_err(BlobError::BeaconChainError)?;

        let pubkey = pubkey_cache
            .get(proposer_index as usize)
            .ok_or_else(|| BlobError::UnknownValidator(proposer_index as u64))?;

        blob_sidecar.verify_signature(
            None,
            pubkey,
            &fork,
            chain.genesis_validators_root,
            &chain.spec,
        )
    };

    if !signature_is_valid {
        return Err(BlobError::ProposerSignatureInvalid);
    }

    // TODO(pawan): kzg validations.

    // TODO(pawan): Check if other blobs for the same proposer index and blob index have been
    // received and drop if required.

    // TODO(pawan): potentially add to a seen cache at this point.

    // Verify if the corresponding block for this blob has been received.
    // Note: this should be the last gossip check so that we can forward the blob
    // over the gossip network even if we haven't received the corresponding block yet
    // as all other validations have passed.
    let block_opt = chain
        .canonical_head
        .fork_choice_read_lock()
        .get_block(&block_root)
        .or_else(|| chain.early_attester_cache.get_proto_block(block_root)); // TODO(pawan): should we be checking this cache?

    if block_opt.is_none() {
        return Err(BlobError::UnknownHeadBlock {
            beacon_block_root: block_root,
        });
    }

    Ok(())
}

fn verify_data_availability<T: BeaconChainTypes>(
    blob_sidecar: &BlobsSidecar<T::EthSpec>,
    kzg_commitments: &[KzgCommitment],
    transactions: &Transactions<T::EthSpec>,
    block_slot: Slot,
    block_root: Hash256,
    chain: &BeaconChain<T>,
) -> Result<(), BlobError> {
    if verify_kzg_commitments_against_transactions::<T::EthSpec>(transactions, kzg_commitments)
        .is_err()
    {
        return Err(BlobError::TransactionCommitmentMismatch);
    }

    // Validatate that the kzg proof is valid against the commitments and blobs
    let kzg = chain
        .kzg
        .as_ref()
        .ok_or(BlobError::TrustedSetupNotInitialized)?;

    if !kzg_utils::validate_blobs_sidecar(
        kzg,
        block_slot,
        block_root,
        kzg_commitments,
        blob_sidecar,
    )
    .map_err(BlobError::KzgError)?
    {
        return Err(BlobError::InvalidKzgProof);
    }
    Ok(())
}

/// A wrapper over a [`SignedBeaconBlock`] or a [`SignedBeaconBlockAndBlobsSidecar`]. This makes no
/// claims about data availability and should not be used in consensus. This struct is useful in
/// networking when we want to send blocks around without consensus checks.
#[derive(Clone, Debug, Derivative)]
#[derivative(PartialEq, Hash(bound = "E: EthSpec"))]
pub enum BlockWrapper<E: EthSpec> {
    Block(Arc<SignedBeaconBlock<E>>),
    BlockAndBlob(Arc<SignedBeaconBlock<E>>, Arc<BlobsSidecar<E>>),
}

impl<E: EthSpec> BlockWrapper<E> {
    pub fn new(
        block: Arc<SignedBeaconBlock<E>>,
        blobs_sidecar: Option<Arc<BlobsSidecar<E>>>,
    ) -> Self {
        if let Some(blobs_sidecar) = blobs_sidecar {
            BlockWrapper::BlockAndBlob(block, blobs_sidecar)
        } else {
            BlockWrapper::Block(block)
        }
    }
}

impl<E: EthSpec> From<SignedBeaconBlock<E>> for BlockWrapper<E> {
    fn from(block: SignedBeaconBlock<E>) -> Self {
        BlockWrapper::Block(Arc::new(block))
    }
}

impl<E: EthSpec> From<SignedBeaconBlockAndBlobsSidecar<E>> for BlockWrapper<E> {
    fn from(block: SignedBeaconBlockAndBlobsSidecar<E>) -> Self {
        let SignedBeaconBlockAndBlobsSidecar {
            beacon_block,
            blobs_sidecar,
        } = block;
        BlockWrapper::BlockAndBlob(beacon_block, blobs_sidecar)
    }
}

impl<E: EthSpec> From<Arc<SignedBeaconBlock<E>>> for BlockWrapper<E> {
    fn from(block: Arc<SignedBeaconBlock<E>>) -> Self {
        BlockWrapper::Block(block)
    }
}

#[derive(Copy, Clone)]
pub enum DataAvailabilityCheckRequired {
    Yes,
    No,
}

pub trait IntoAvailableBlock<T: BeaconChainTypes> {
    fn into_available_block(
        self,
        block_root: Hash256,
        chain: &BeaconChain<T>,
    ) -> Result<AvailableBlock<T::EthSpec>, BlobError>;
}

impl<T: BeaconChainTypes> IntoAvailableBlock<T> for BlockWrapper<T::EthSpec> {
    fn into_available_block(
        self,
        block_root: Hash256,
        chain: &BeaconChain<T>,
    ) -> Result<AvailableBlock<T::EthSpec>, BlobError> {
        let data_availability_boundary = chain.data_availability_boundary();
        let da_check_required =
            data_availability_boundary.map_or(DataAvailabilityCheckRequired::No, |boundary| {
                if self.slot().epoch(T::EthSpec::slots_per_epoch()) >= boundary {
                    DataAvailabilityCheckRequired::Yes
                } else {
                    DataAvailabilityCheckRequired::No
                }
            });
        match self {
            BlockWrapper::Block(block) => AvailableBlock::new(block, block_root, da_check_required),
            BlockWrapper::BlockAndBlob(block, blobs_sidecar) => {
                if matches!(da_check_required, DataAvailabilityCheckRequired::Yes) {
                    let kzg_commitments = block
                        .message()
                        .body()
                        .blob_kzg_commitments()
                        .map_err(|_| BlobError::KzgCommitmentMissing)?;
                    let transactions = block
                        .message()
                        .body()
                        .execution_payload_eip4844()
                        .map(|payload| payload.transactions())
                        .map_err(|_| BlobError::TransactionsMissing)?
                        .ok_or(BlobError::TransactionsMissing)?;
                    verify_data_availability(
                        &blobs_sidecar,
                        kzg_commitments,
                        transactions,
                        block.slot(),
                        block_root,
                        chain,
                    )?;
                }

                AvailableBlock::new_with_blobs(block, blobs_sidecar, da_check_required)
            }
        }
    }
}

/// A wrapper over a [`SignedBeaconBlock`] or a [`SignedBeaconBlockAndBlobsSidecar`].  An
/// `AvailableBlock` has passed any required data availability checks and should be used in
/// consensus. This newtype wraps `AvailableBlockInner` to ensure data availability checks
/// cannot be circumvented on construction.
#[derive(Clone, Debug, Derivative)]
#[derivative(PartialEq, Hash(bound = "E: EthSpec"))]
pub struct AvailableBlock<E: EthSpec>(AvailableBlockInner<E>);

/// A wrapper over a [`SignedBeaconBlock`] or a [`SignedBeaconBlockAndBlobsSidecar`].
#[derive(Clone, Debug, Derivative)]
#[derivative(PartialEq, Hash(bound = "E: EthSpec"))]
enum AvailableBlockInner<E: EthSpec> {
    Block(Arc<SignedBeaconBlock<E>>),
    BlockAndBlob(SignedBeaconBlockAndBlobsSidecar<E>),
}

impl<E: EthSpec> AvailableBlock<E> {
    pub fn new(
        beacon_block: Arc<SignedBeaconBlock<E>>,
        block_root: Hash256,
        da_check_required: DataAvailabilityCheckRequired,
    ) -> Result<Self, BlobError> {
        match beacon_block.as_ref() {
            // No data availability check required prior to Eip4844.
            SignedBeaconBlock::Base(_)
            | SignedBeaconBlock::Altair(_)
            | SignedBeaconBlock::Capella(_)
            | SignedBeaconBlock::Merge(_) => {
                Ok(AvailableBlock(AvailableBlockInner::Block(beacon_block)))
            }
            SignedBeaconBlock::Eip4844(_) => {
                match da_check_required {
                    DataAvailabilityCheckRequired::Yes => {
                        // Attempt to reconstruct empty blobs here.
                        let blobs_sidecar = beacon_block
                            .reconstruct_empty_blobs(Some(block_root))
                            .map(Arc::new)?;
                        return Ok(AvailableBlock(AvailableBlockInner::BlockAndBlob(
                            SignedBeaconBlockAndBlobsSidecar {
                                beacon_block,
                                blobs_sidecar,
                            },
                        )));
                    }
                    DataAvailabilityCheckRequired::No => {
                        Ok(AvailableBlock(AvailableBlockInner::Block(beacon_block)))
                    }
                }
            }
        }
    }

    /// This function is private because an `AvailableBlock` should be
    /// constructed via the `into_available_block` method.
    fn new_with_blobs(
        beacon_block: Arc<SignedBeaconBlock<E>>,
        blobs_sidecar: Arc<BlobsSidecar<E>>,
        da_check_required: DataAvailabilityCheckRequired,
    ) -> Result<Self, BlobError> {
        match beacon_block.as_ref() {
            // This method shouldn't be called with a pre-Eip4844 block.
            SignedBeaconBlock::Base(_)
            | SignedBeaconBlock::Altair(_)
            | SignedBeaconBlock::Capella(_)
            | SignedBeaconBlock::Merge(_) => Err(BlobError::InconsistentFork),
            SignedBeaconBlock::Eip4844(_) => {
                match da_check_required {
                    DataAvailabilityCheckRequired::Yes => Ok(AvailableBlock(
                        AvailableBlockInner::BlockAndBlob(SignedBeaconBlockAndBlobsSidecar {
                            beacon_block,
                            blobs_sidecar,
                        }),
                    )),
                    DataAvailabilityCheckRequired::No => {
                        // Blobs were not verified so we drop them, we'll instead just pass around
                        // an available `Eip4844` block without blobs.
                        Ok(AvailableBlock(AvailableBlockInner::Block(beacon_block)))
                    }
                }
            }
        }
    }

    pub fn blobs(&self) -> Option<Arc<BlobsSidecar<E>>> {
        match &self.0 {
            AvailableBlockInner::Block(_) => None,
            AvailableBlockInner::BlockAndBlob(block_sidecar_pair) => {
                Some(block_sidecar_pair.blobs_sidecar.clone())
            }
        }
    }

    pub fn deconstruct(self) -> (Arc<SignedBeaconBlock<E>>, Option<Arc<BlobsSidecar<E>>>) {
        match self.0 {
            AvailableBlockInner::Block(block) => (block, None),
            AvailableBlockInner::BlockAndBlob(block_sidecar_pair) => {
                let SignedBeaconBlockAndBlobsSidecar {
                    beacon_block,
                    blobs_sidecar,
                } = block_sidecar_pair;
                (beacon_block, Some(blobs_sidecar))
            }
        }
    }
}

pub trait IntoBlockWrapper<E: EthSpec>: AsBlock<E> {
    fn into_block_wrapper(self) -> BlockWrapper<E>;
}

impl<E: EthSpec> IntoBlockWrapper<E> for BlockWrapper<E> {
    fn into_block_wrapper(self) -> BlockWrapper<E> {
        self
    }
}

impl<E: EthSpec> IntoBlockWrapper<E> for AvailableBlock<E> {
    fn into_block_wrapper(self) -> BlockWrapper<E> {
        let (block, blobs) = self.deconstruct();
        if let Some(blobs) = blobs {
            BlockWrapper::BlockAndBlob(block, blobs)
        } else {
            BlockWrapper::Block(block)
        }
    }
}

pub trait AsBlock<E: EthSpec> {
    fn slot(&self) -> Slot;
    fn epoch(&self) -> Epoch;
    fn parent_root(&self) -> Hash256;
    fn state_root(&self) -> Hash256;
    fn signed_block_header(&self) -> SignedBeaconBlockHeader;
    fn message(&self) -> BeaconBlockRef<E>;
    fn as_block(&self) -> &SignedBeaconBlock<E>;
    fn block_cloned(&self) -> Arc<SignedBeaconBlock<E>>;
}

impl<E: EthSpec> AsBlock<E> for BlockWrapper<E> {
    fn slot(&self) -> Slot {
        match self {
            BlockWrapper::Block(block) => block.slot(),
            BlockWrapper::BlockAndBlob(block, _) => block.slot(),
        }
    }
    fn epoch(&self) -> Epoch {
        match self {
            BlockWrapper::Block(block) => block.epoch(),
            BlockWrapper::BlockAndBlob(block, _) => block.epoch(),
        }
    }
    fn parent_root(&self) -> Hash256 {
        match self {
            BlockWrapper::Block(block) => block.parent_root(),
            BlockWrapper::BlockAndBlob(block, _) => block.parent_root(),
        }
    }
    fn state_root(&self) -> Hash256 {
        match self {
            BlockWrapper::Block(block) => block.state_root(),
            BlockWrapper::BlockAndBlob(block, _) => block.state_root(),
        }
    }
    fn signed_block_header(&self) -> SignedBeaconBlockHeader {
        match &self {
            BlockWrapper::Block(block) => block.signed_block_header(),
            BlockWrapper::BlockAndBlob(block, _) => block.signed_block_header(),
        }
    }
    fn message(&self) -> BeaconBlockRef<E> {
        match &self {
            BlockWrapper::Block(block) => block.message(),
            BlockWrapper::BlockAndBlob(block, _) => block.message(),
        }
    }
    fn as_block(&self) -> &SignedBeaconBlock<E> {
        match &self {
            BlockWrapper::Block(block) => &block,
            BlockWrapper::BlockAndBlob(block, _) => &block,
        }
    }
    fn block_cloned(&self) -> Arc<SignedBeaconBlock<E>> {
        match &self {
            BlockWrapper::Block(block) => block.clone(),
            BlockWrapper::BlockAndBlob(block, _) => block.clone(),
        }
    }
}

impl<E: EthSpec> AsBlock<E> for &BlockWrapper<E> {
    fn slot(&self) -> Slot {
        match self {
            BlockWrapper::Block(block) => block.slot(),
            BlockWrapper::BlockAndBlob(block, _) => block.slot(),
        }
    }
    fn epoch(&self) -> Epoch {
        match self {
            BlockWrapper::Block(block) => block.epoch(),
            BlockWrapper::BlockAndBlob(block, _) => block.epoch(),
        }
    }
    fn parent_root(&self) -> Hash256 {
        match self {
            BlockWrapper::Block(block) => block.parent_root(),
            BlockWrapper::BlockAndBlob(block, _) => block.parent_root(),
        }
    }
    fn state_root(&self) -> Hash256 {
        match self {
            BlockWrapper::Block(block) => block.state_root(),
            BlockWrapper::BlockAndBlob(block, _) => block.state_root(),
        }
    }
    fn signed_block_header(&self) -> SignedBeaconBlockHeader {
        match &self {
            BlockWrapper::Block(block) => block.signed_block_header(),
            BlockWrapper::BlockAndBlob(block, _) => block.signed_block_header(),
        }
    }
    fn message(&self) -> BeaconBlockRef<E> {
        match &self {
            BlockWrapper::Block(block) => block.message(),
            BlockWrapper::BlockAndBlob(block, _) => block.message(),
        }
    }
    fn as_block(&self) -> &SignedBeaconBlock<E> {
        match &self {
            BlockWrapper::Block(block) => &block,
            BlockWrapper::BlockAndBlob(block, _) => &block,
        }
    }
    fn block_cloned(&self) -> Arc<SignedBeaconBlock<E>> {
        match &self {
            BlockWrapper::Block(block) => block.clone(),
            BlockWrapper::BlockAndBlob(block, _) => block.clone(),
        }
    }
}

impl<E: EthSpec> AsBlock<E> for AvailableBlock<E> {
    fn slot(&self) -> Slot {
        match &self.0 {
            AvailableBlockInner::Block(block) => block.slot(),
            AvailableBlockInner::BlockAndBlob(block_sidecar_pair) => {
                block_sidecar_pair.beacon_block.slot()
            }
        }
    }
    fn epoch(&self) -> Epoch {
        match &self.0 {
            AvailableBlockInner::Block(block) => block.epoch(),
            AvailableBlockInner::BlockAndBlob(block_sidecar_pair) => {
                block_sidecar_pair.beacon_block.epoch()
            }
        }
    }
    fn parent_root(&self) -> Hash256 {
        match &self.0 {
            AvailableBlockInner::Block(block) => block.parent_root(),
            AvailableBlockInner::BlockAndBlob(block_sidecar_pair) => {
                block_sidecar_pair.beacon_block.parent_root()
            }
        }
    }
    fn state_root(&self) -> Hash256 {
        match &self.0 {
            AvailableBlockInner::Block(block) => block.state_root(),
            AvailableBlockInner::BlockAndBlob(block_sidecar_pair) => {
                block_sidecar_pair.beacon_block.state_root()
            }
        }
    }
    fn signed_block_header(&self) -> SignedBeaconBlockHeader {
        match &self.0 {
            AvailableBlockInner::Block(block) => block.signed_block_header(),
            AvailableBlockInner::BlockAndBlob(block_sidecar_pair) => {
                block_sidecar_pair.beacon_block.signed_block_header()
            }
        }
    }
    fn message(&self) -> BeaconBlockRef<E> {
        match &self.0 {
            AvailableBlockInner::Block(block) => block.message(),
            AvailableBlockInner::BlockAndBlob(block_sidecar_pair) => {
                block_sidecar_pair.beacon_block.message()
            }
        }
    }
    fn as_block(&self) -> &SignedBeaconBlock<E> {
        match &self.0 {
            AvailableBlockInner::Block(block) => &block,
            AvailableBlockInner::BlockAndBlob(block_sidecar_pair) => {
                &block_sidecar_pair.beacon_block
            }
        }
    }
    fn block_cloned(&self) -> Arc<SignedBeaconBlock<E>> {
        match &self.0 {
            AvailableBlockInner::Block(block) => block.clone(),
            AvailableBlockInner::BlockAndBlob(block_sidecar_pair) => {
                block_sidecar_pair.beacon_block.clone()
            }
        }
    }
}
