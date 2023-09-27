use ssz_types::VariableList;
use tree_hash::TreeHash;

use crate::{Blob, EthSpec, Hash256};

pub trait BlobItems<T: EthSpec>: Sync + Send + Sized {
    fn try_from_blob_roots(roots: BlobRootsList<T>) -> Result<Self, String>;
    fn try_from_blobs(blobs: BlobsList<T>) -> Result<Self, String>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool;
    fn blobs(&self) -> Option<&BlobsList<T>>;
    fn empty() -> Self;
}

pub type BlobsList<T> = VariableList<Blob<T>, <T as EthSpec>::MaxBlobCommitmentsPerBlock>;
pub type BlobRootsList<T> = VariableList<Hash256, <T as EthSpec>::MaxBlobCommitmentsPerBlock>;

impl<T: EthSpec> BlobItems<T> for BlobsList<T> {
    fn try_from_blob_roots(_roots: BlobRootsList<T>) -> Result<Self, String> {
        Err("Unexpected conversion from blob roots to blobs".to_string())
    }

    fn try_from_blobs(blobs: BlobsList<T>) -> Result<Self, String> {
        Ok(blobs)
    }

    fn len(&self) -> usize {
        VariableList::len(self)
    }

    fn is_empty(&self) -> bool {
        VariableList::is_empty(self)
    }

    fn blobs(&self) -> Option<&BlobsList<T>> {
        Some(self)
    }

    fn empty() -> Self {
        VariableList::empty()
    }
}

impl<T: EthSpec> BlobItems<T> for BlobRootsList<T> {
    fn try_from_blob_roots(roots: BlobRootsList<T>) -> Result<Self, String> {
        Ok(roots)
    }

    fn try_from_blobs(blobs: BlobsList<T>) -> Result<Self, String> {
        VariableList::new(
            blobs
                .into_iter()
                .map(|blob| blob.tree_hash_root())
                .collect(),
        )
        .map_err(|e| format!("{e:?}"))
    }

    fn len(&self) -> usize {
        VariableList::len(self)
    }

    fn is_empty(&self) -> bool {
        VariableList::is_empty(self)
    }

    fn blobs(&self) -> Option<&BlobsList<T>> {
        None
    }

    fn empty() -> Self {
        VariableList::empty()
    }
}
