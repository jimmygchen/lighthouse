#[cfg(test)]
mod tests {
    use crate::persisted_fork_choice::PersistedForkChoice;
    use crate::BeaconForkChoiceStore;
    use fork_choice::{ForkChoice, ResetPayloadStatuses};
    use ssz::Decode;
    use std::fs;
    use store::MemoryStore;
    use task_executor::test_utils::null_logger;
    use types::{EthSpec, MainnetEthSpec};

    #[test]
    fn print_persisted_fc() {
        type E = MainnetEthSpec;
        let vec = fs::read("/Users/jimmychen/Workspace/eth/lighthouse/frk_0x0000…0000.ssz")
            .expect("should open file");
        println!("length {}", vec.len());
        let fc_persisted = PersistedForkChoice::from_ssz_bytes(&*vec).expect("should decode");
        let log = null_logger().unwrap();
        let fc = ForkChoice::<BeaconForkChoiceStore<E, MemoryStore<E>, MemoryStore<E>>, E>::proto_array_from_persisted(
            &fc_persisted.fork_choice,
            ResetPayloadStatuses::OnlyWithInvalidPayload,
            &E::default_spec(),
            &log,
        )
            .expect("should load proto array fc");

        dbg!(fc_persisted.fork_choice_store.finalized_checkpoint);
        dbg!(fc_persisted.fork_choice_store.justified_checkpoint);
        dbg!(
            fc_persisted
                .fork_choice_store
                .unrealized_justified_checkpoint
        );
        dbg!(
            fc_persisted
                .fork_choice_store
                .unrealized_finalized_checkpoint
        );
        dbg!(fc_persisted.fork_choice_store.proposer_boost_root);

        let proto_array = fc.core_proto_array();
        dbg!(proto_array);
    }
}
