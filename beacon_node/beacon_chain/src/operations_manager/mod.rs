#[cfg(test)]
mod tests;

use crate::errors::BeaconChainError as Error;
use crate::observed_operations::{ObservationOutcome, ObservedOperations};
use operation_pool::{OperationPool, ReceivedPreCapella};
use parking_lot::Mutex;
use state_processing::SigVerifiedOp;
use std::sync::Arc;
use types::{
    AttesterSlashing, BeaconState, ChainSpec, Epoch, EthSpec, ProposerSlashing,
    SignedBlsToExecutionChange, SignedVoluntaryExit,
};

/// Manages verification and import of voluntary exits, proposer slashings,
/// attester slashings, and BLS-to-execution changes.
///
/// Generic over `E: EthSpec` rather than `T: BeaconChainTypes` so it can be
/// constructed and tested without a full `BeaconChain`.
///
/// State is passed as method parameters -- this component never fetches head
/// state, slot clock values, or similar chain-level context on its own.
pub struct OperationsManager<E: EthSpec> {
    spec: Arc<ChainSpec>,
    op_pool: Arc<OperationPool<E>>,
    pub(crate) observed_voluntary_exits: Mutex<ObservedOperations<SignedVoluntaryExit, E>>,
    pub(crate) observed_proposer_slashings: Mutex<ObservedOperations<ProposerSlashing, E>>,
    pub(crate) observed_attester_slashings: Mutex<ObservedOperations<AttesterSlashing<E>, E>>,
    pub(crate) observed_bls_to_execution_changes:
        Mutex<ObservedOperations<SignedBlsToExecutionChange, E>>,
}

impl<E: EthSpec> OperationsManager<E> {
    /// Create a new `OperationsManager`.
    pub fn new(spec: Arc<ChainSpec>, op_pool: Arc<OperationPool<E>>) -> Self {
        Self {
            spec,
            op_pool,
            observed_voluntary_exits: <_>::default(),
            observed_proposer_slashings: <_>::default(),
            observed_attester_slashings: <_>::default(),
            observed_bls_to_execution_changes: <_>::default(),
        }
    }

    // -----------------------------------------------------------------------
    // Voluntary exits
    // -----------------------------------------------------------------------

    /// Verify a voluntary exit against the head state and wall-clock epoch.
    ///
    /// Returns `ObservationOutcome::New` with a signature-verified exit on
    /// success, or `ObservationOutcome::AlreadyKnown` if this exit was already
    /// observed.
    pub fn verify_voluntary_exit(
        &self,
        exit: SignedVoluntaryExit,
        head_state: &BeaconState<E>,
        wall_clock_epoch: Epoch,
    ) -> Result<ObservationOutcome<SignedVoluntaryExit, E>, Error> {
        Ok(self.observed_voluntary_exits.lock().verify_and_observe_at(
            exit,
            wall_clock_epoch,
            head_state,
            &self.spec,
        )?)
    }

    /// Accept a pre-verified exit and queue it for inclusion in an appropriate block.
    pub fn import_voluntary_exit(&self, exit: SigVerifiedOp<SignedVoluntaryExit, E>) {
        self.op_pool.insert_voluntary_exit(exit)
    }

    // -----------------------------------------------------------------------
    // Proposer slashings
    // -----------------------------------------------------------------------

    /// Verify a proposer slashing against the provided state.
    pub fn verify_proposer_slashing(
        &self,
        proposer_slashing: ProposerSlashing,
        state: &BeaconState<E>,
    ) -> Result<ObservationOutcome<ProposerSlashing, E>, Error> {
        Ok(self.observed_proposer_slashings.lock().verify_and_observe(
            proposer_slashing,
            state,
            &self.spec,
        )?)
    }

    /// Accept a pre-verified proposer slashing and queue it for inclusion in a block.
    ///
    /// Returns the inner `ProposerSlashing` so the caller can emit SSE events.
    pub fn import_proposer_slashing(
        &self,
        proposer_slashing: SigVerifiedOp<ProposerSlashing, E>,
    ) -> ProposerSlashing {
        let slashing = proposer_slashing.clone().into_inner();
        self.op_pool.insert_proposer_slashing(proposer_slashing);
        slashing
    }

    // -----------------------------------------------------------------------
    // Attester slashings
    // -----------------------------------------------------------------------

    /// Verify an attester slashing against the provided state.
    pub fn verify_attester_slashing(
        &self,
        attester_slashing: AttesterSlashing<E>,
        state: &BeaconState<E>,
    ) -> Result<ObservationOutcome<AttesterSlashing<E>, E>, Error> {
        Ok(self.observed_attester_slashings.lock().verify_and_observe(
            attester_slashing,
            state,
            &self.spec,
        )?)
    }

    /// Accept a pre-verified attester slashing and add it to the op pool.
    ///
    /// Note: the fork-choice write (`on_attester_slashing`) stays on
    /// `BeaconChain` because it requires the fork-choice write lock.
    pub fn import_attester_slashing(
        &self,
        attester_slashing: SigVerifiedOp<AttesterSlashing<E>, E>,
    ) {
        self.op_pool.insert_attester_slashing(attester_slashing)
    }

    // -----------------------------------------------------------------------
    // BLS-to-execution changes
    // -----------------------------------------------------------------------

    /// Verify a BLS-to-execution change for the HTTP API path.
    ///
    /// Checks the op pool for conflicts before running signature verification.
    pub fn verify_bls_to_execution_change(
        &self,
        bls_to_execution_change: SignedBlsToExecutionChange,
        head_state: &BeaconState<E>,
    ) -> Result<ObservationOutcome<SignedBlsToExecutionChange, E>, Error> {
        // Before checking the gossip duplicate filter, check that no prior change is already
        // in our op pool. Ignore these messages: do not gossip, do not try to override the pool.
        match self
            .op_pool
            .bls_to_execution_change_in_pool_equals(&bls_to_execution_change)
        {
            Some(true) => return Ok(ObservationOutcome::AlreadyKnown),
            Some(false) => return Err(Error::BlsToExecutionConflictsWithPool),
            None => (),
        }

        Ok(self
            .observed_bls_to_execution_changes
            .lock()
            .verify_and_observe(bls_to_execution_change, head_state, &self.spec)?)
    }

    /// Verify a BLS-to-execution change for the gossip network path.
    ///
    /// Rejects changes received prior to Capella and treats pool conflicts as
    /// duplicates ([IGNORE] per spec).
    pub fn verify_bls_to_execution_change_for_gossip(
        &self,
        bls_to_execution_change: SignedBlsToExecutionChange,
        head_state: &BeaconState<E>,
        is_post_capella: bool,
    ) -> Result<ObservationOutcome<SignedBlsToExecutionChange, E>, Error> {
        // Ignore BLS to execution changes on gossip prior to Capella.
        if !is_post_capella {
            return Err(Error::BlsToExecutionPriorToCapella);
        }
        self.verify_bls_to_execution_change(bls_to_execution_change, head_state)
            .or_else(|e| {
                // On gossip treat conflicts the same as duplicates [IGNORE].
                match e {
                    Error::BlsToExecutionConflictsWithPool => Ok(ObservationOutcome::AlreadyKnown),
                    e => Err(e),
                }
            })
    }

    /// Import a BLS-to-execution change to the op pool.
    ///
    /// Returns `true` if the change was added to the pool.
    pub fn import_bls_to_execution_change(
        &self,
        bls_to_execution_change: SigVerifiedOp<SignedBlsToExecutionChange, E>,
        received_pre_capella: ReceivedPreCapella,
    ) -> bool {
        self.op_pool
            .insert_bls_to_execution_change(bls_to_execution_change, received_pre_capella)
    }
}
