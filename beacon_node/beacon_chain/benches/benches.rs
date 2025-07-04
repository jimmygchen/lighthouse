use beacon_chain::kzg_utils::{blobs_to_data_column_sidecars, reconstruct_data_columns};
use beacon_chain::test_utils::get_kzg;
use bls::Signature;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use eth2::types::BlobsBundle;
use kzg::{KzgCommitment, KzgProof};
use ssz::{Decode, Encode};
use std::sync::Arc;
use std::time::Instant;
use types::{
    beacon_block_body::KzgCommitments, BeaconBlock, BeaconBlockDeneb, Blob, BlobsList, ChainSpec,
    EmptyBlock, EthSpec, ForkName, KzgProofs, MainnetEthSpec, SignedBeaconBlock,
};

fn create_test_block_and_blobs<E: EthSpec>(
    num_of_blobs: usize,
    spec: &ChainSpec,
) -> (SignedBeaconBlock<E>, BlobsList<E>, KzgProofs<E>) {
    let mut block = BeaconBlock::Deneb(BeaconBlockDeneb::empty(spec));
    let mut body = block.body_mut();
    let blob_kzg_commitments = body.blob_kzg_commitments_mut().unwrap();
    *blob_kzg_commitments =
        KzgCommitments::<E>::new(vec![KzgCommitment::empty_for_testing(); num_of_blobs]).unwrap();

    let signed_block = SignedBeaconBlock::from_block(block, Signature::empty());

    let blobs = (0..num_of_blobs)
        .map(|_| Blob::<E>::default())
        .collect::<Vec<_>>()
        .into();
    let proofs = vec![KzgProof::empty(); num_of_blobs * spec.number_of_columns as usize].into();

    (signed_block, blobs, proofs)
}

fn all_benches(c: &mut Criterion) {
    type E = MainnetEthSpec;
    const NUM_COLS: usize = 128;
    let spec = ForkName::Fulu.make_genesis_spec(E::default_spec());

    let mut test_with_blobs = |num_blobs: usize, spec: &ChainSpec| {
        let blobs_bundle = BlobsBundle::<E> {
            commitments: vec![KzgCommitment::empty_for_testing(); num_blobs].into(),
            proofs: vec![KzgProof::empty(); num_blobs * NUM_COLS].into(),
            blobs: vec![Blob::<E>::new(vec![0; 131072]).unwrap(); num_blobs].into(),
        };

        c.bench_function(&format!("json_encode_{}", num_blobs), |b| {
            b.iter(|| black_box(serde_json::to_string(&blobs_bundle).unwrap()))
        });

        let str = serde_json::to_string(&blobs_bundle).unwrap();

        c.bench_function(&format!("json_decode_{}", num_blobs), |b| {
            b.iter(|| black_box(serde_json::from_str::<BlobsBundle<E>>(&str).unwrap()))
        });

        c.bench_function(&format!("ssz_encode_{}", num_blobs), |b| {
            b.iter(|| black_box(blobs_bundle.as_ssz_bytes()))
        });

        let ssz = blobs_bundle.as_ssz_bytes();

        c.bench_function(&format!("ssz_decode_{}", num_blobs), |b| {
            b.iter(|| black_box(BlobsBundle::<E>::from_ssz_bytes(&ssz).unwrap()))
        });
    };

    for blob_count in vec![64, 128] {
        test_with_blobs(blob_count, &spec);
    }
}

criterion_group!(benches, all_benches);
criterion_main!(benches);
