use crate::Hash256;
use ethereum_hashing::hash_fixed;
use std::mem;

const SEED_SIZE: usize = 32;
const ROUND_SIZE: usize = 1;
const POSITION_WINDOW_SIZE: usize = 4;
const PIVOT_VIEW_SIZE: usize = SEED_SIZE + ROUND_SIZE;
const TOTAL_SIZE: usize = SEED_SIZE + ROUND_SIZE + POSITION_WINDOW_SIZE;

/// A helper struct to manage the buffer used during shuffling.
struct Buf([u8; TOTAL_SIZE]);

impl Buf {
    /// Create a new buffer from the given `seed`.
    ///
    /// ## Panics
    ///
    /// Panics if `seed.len() != 32`.
    fn new(seed: &[u8]) -> Self {
        let mut buf = [0; TOTAL_SIZE];
        buf[0..SEED_SIZE].copy_from_slice(seed);
        Self(buf)
    }

    /// Set the shuffling round.
    fn set_round(&mut self, round: u8) {
        self.0[SEED_SIZE] = round;
    }

    /// Returns the new pivot. It is "raw" because it has not modulo the list size (this must be
    /// done by the caller).
    fn raw_pivot(&self) -> u64 {
        let digest = hash_fixed(&self.0[0..PIVOT_VIEW_SIZE]);

        let mut bytes = [0; mem::size_of::<u64>()];
        bytes[..].copy_from_slice(&digest[0..mem::size_of::<u64>()]);
        u64::from_le_bytes(bytes)
    }

    /// Add the current position into the buffer.
    fn mix_in_position(&mut self, position: usize) {
        self.0[PIVOT_VIEW_SIZE..].copy_from_slice(&position.to_le_bytes()[0..POSITION_WINDOW_SIZE]);
    }

    /// Hash the entire buffer.
    fn hash(&self) -> Hash256 {
        Hash256::from(hash_fixed(&self.0))
    }
}

/// Shuffles an entire list in-place.
///
/// Note: this is equivalent to the `compute_shuffled_index` function, except it shuffles an entire
/// list not just a single index. With large lists this function has been observed to be 250x
/// faster than running `compute_shuffled_index` across an entire list.
///
/// Credits to [@protolambda](https://github.com/protolambda) for defining this algorithm.
///
/// Shuffles if `forwards == true`, otherwise un-shuffles.
/// It holds that: shuffle_list(shuffle_list(l, r, s, true), r, s, false) == l
///           and: shuffle_list(shuffle_list(l, r, s, false), r, s, true) == l
///
/// The Eth2.0 spec mostly uses shuffling with `forwards == false`, because backwards
/// shuffled lists are slightly easier to specify, and slightly easier to compute.
///
/// The forwards shuffling of a list is equivalent to:
///
/// `[indices[x] for i in 0..n, where compute_shuffled_index(x) = i]`
///
/// Whereas the backwards shuffling of a list is:
///
/// `[indices[compute_shuffled_index(i)] for i in 0..n]`
///
/// Returns `None` under any of the following conditions:
///  - `list_size == 0`
///  - `list_size > 2**24`
///  - `list_size > usize::MAX / 2`
pub fn shuffle_list(
    mut input: Vec<usize>,
    rounds: u8,
    seed: &[u8],
    forwards: bool,
) -> Option<Vec<usize>> {
    let list_size = input.len();

    if input.is_empty() || list_size > usize::MAX / 2 || list_size > 2_usize.pow(24) || rounds == 0
    {
        return None;
    }

    let mut buf = Buf::new(seed);

    let mut r = if forwards { 0 } else { rounds - 1 };

    loop {
        buf.set_round(r);

        let pivot = (buf.raw_pivot() % list_size as u64) as usize;
        let mirror = (pivot + 1) >> 1;

        buf.mix_in_position(pivot >> 8);
        let mut source = buf.hash();
        let mut byte_v = source[(pivot & 0xff) >> 3];

        for i in 0..mirror {
            let j = pivot - i;

            if j & 0xff == 0xff {
                buf.mix_in_position(j >> 8);
                source = buf.hash();
            }

            if j & 0x07 == 0x07 {
                byte_v = source[(j & 0xff) >> 3];
            }
            let bit_v = (byte_v >> (j & 0x07)) & 0x01;

            if bit_v == 1 {
                input.swap(i, j);
            }
        }

        let mirror = (pivot + list_size + 1) >> 1;
        let end = list_size - 1;

        buf.mix_in_position(end >> 8);
        let mut source = buf.hash();
        let mut byte_v = source[(end & 0xff) >> 3];

        for (loop_iter, i) in ((pivot + 1)..mirror).enumerate() {
            let j = end - loop_iter;

            if j & 0xff == 0xff {
                buf.mix_in_position(j >> 8);
                source = buf.hash();
            }

            if j & 0x07 == 0x07 {
                byte_v = source[(j & 0xff) >> 3];
            }
            let bit_v = (byte_v >> (j & 0x07)) & 0x01;

            if bit_v == 1 {
                input.swap(i, j);
            }
        }

        if forwards {
            r += 1;
            if r == rounds {
                break;
            }
        } else {
            if r == 0 {
                break;
            }
            r -= 1;
        }
    }

    Some(input)
}

/// Experimental branchless variant of [`shuffle_list`].
///
/// This preserves the reference algorithm but replaces each conditional swap with a masked XOR
/// swap. It is exposed for differential tests and benchmarks, and is used by the production
/// `CommitteeCache` path for this experiment.
pub fn shuffle_list_branchless(
    mut input: Vec<usize>,
    rounds: u8,
    seed: &[u8],
    forwards: bool,
) -> Option<Vec<usize>> {
    let list_size = input.len();

    if input.is_empty() || list_size > usize::MAX / 2 || list_size > 2_usize.pow(24) || rounds == 0
    {
        return None;
    }

    let mut buf = Buf::new(seed);

    let mut r = if forwards { 0 } else { rounds - 1 };

    loop {
        buf.set_round(r);

        let pivot = (buf.raw_pivot() % list_size as u64) as usize;
        let mirror = (pivot + 1) >> 1;

        buf.mix_in_position(pivot >> 8);
        let mut source = buf.hash();
        let mut byte_v = source[(pivot & 0xff) >> 3];

        for i in 0..mirror {
            let j = pivot - i;

            if j & 0xff == 0xff {
                buf.mix_in_position(j >> 8);
                source = buf.hash();
            }

            if j & 0x07 == 0x07 {
                byte_v = source[(j & 0xff) >> 3];
            }
            let bit_v = (byte_v >> (j & 0x07)) & 0x01;

            let mask = 0usize.wrapping_sub(bit_v as usize);
            let left = input[i];
            let right = input[j];
            let delta = (left ^ right) & mask;
            input[i] = left ^ delta;
            input[j] = right ^ delta;
        }

        let mirror = (pivot + list_size + 1) >> 1;
        let end = list_size - 1;

        buf.mix_in_position(end >> 8);
        let mut source = buf.hash();
        let mut byte_v = source[(end & 0xff) >> 3];

        for (loop_iter, i) in ((pivot + 1)..mirror).enumerate() {
            let j = end - loop_iter;

            if j & 0xff == 0xff {
                buf.mix_in_position(j >> 8);
                source = buf.hash();
            }

            if j & 0x07 == 0x07 {
                byte_v = source[(j & 0xff) >> 3];
            }
            let bit_v = (byte_v >> (j & 0x07)) & 0x01;

            let mask = 0usize.wrapping_sub(bit_v as usize);
            let left = input[i];
            let right = input[j];
            let delta = (left ^ right) & mask;
            input[i] = left ^ delta;
            input[j] = right ^ delta;
        }

        if forwards {
            r += 1;
            if r == rounds {
                break;
            }
        } else {
            if r == 0 {
                break;
            }
            r -= 1;
        }
    }

    Some(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SIZES: [usize; 23] = [
        1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 255, 256, 257, 511, 512, 513,
    ];
    const TEST_ROUNDS: [u8; 4] = [1, 2, 17, 90];

    type ShuffleFn = fn(Vec<usize>, u8, &[u8], bool) -> Option<Vec<usize>>;

    fn test_seeds() -> [[u8; 32]; 3] {
        [[0; 32], [42; 32], std::array::from_fn(|index| index as u8)]
    }

    #[test]
    fn returns_none_for_zero_length_list() {
        assert_eq!(None, shuffle_list(vec![], 90, &[42, 42], true));
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn sanity_check_constants() {
        assert!(TOTAL_SIZE > SEED_SIZE);
        assert!(TOTAL_SIZE > PIVOT_VIEW_SIZE);
        assert!(mem::size_of::<usize>() >= POSITION_WINDOW_SIZE);
    }

    #[test]
    fn branchless_matches_reference() {
        for size in TEST_SIZES {
            let input: Vec<_> = (0..size).map(|value| value.wrapping_mul(17)).collect();

            for rounds in TEST_ROUNDS {
                for seed in test_seeds() {
                    for forwards in [true, false] {
                        assert_eq!(
                            shuffle_list(input.clone(), rounds, &seed, forwards),
                            shuffle_list_branchless(input.clone(), rounds, &seed, forwards),
                            "size={size}, rounds={rounds}, seed={seed:?}, forwards={forwards}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn branchless_matches_reference_for_deterministic_random_cases() {
        for case in 0_u64..32 {
            let size = ((case.wrapping_mul(7_919) % 8_192).wrapping_add(1)) as usize;
            let rounds = ((case.wrapping_mul(17) % 90).wrapping_add(1)) as u8;
            let seed: [u8; 32] = std::array::from_fn(|index| {
                (case as u8)
                    .wrapping_mul(31)
                    .wrapping_add((index as u8).wrapping_mul(17))
            });
            let input: Vec<_> = (0..size)
                .map(|value| value.wrapping_mul(7_919) ^ (value / 7))
                .collect();

            for forwards in [true, false] {
                assert_eq!(
                    shuffle_list(input.clone(), rounds, &seed, forwards),
                    shuffle_list_branchless(input.clone(), rounds, &seed, forwards),
                    "case={case}, size={size}, rounds={rounds}, forwards={forwards}"
                );
            }
        }

        let size = 65_537;
        let seed = [0xa5; 32];
        let input: Vec<_> = (0..size).collect();
        for forwards in [true, false] {
            assert_eq!(
                shuffle_list(input.clone(), 90, &seed, forwards),
                shuffle_list_branchless(input.clone(), 90, &seed, forwards),
                "large deterministic case, forwards={forwards}"
            );
        }
    }

    #[test]
    fn branchless_reverse_matches_compute_shuffled_index() {
        for size in [
            1, 2, 3, 7, 8, 15, 16, 31, 32, 255, 256, 257, 1_023, 1_024, 1_025,
        ] {
            for rounds in [1, 17, 90] {
                for seed in test_seeds() {
                    let expected: Option<Vec<_>> = (0..size)
                        .map(|index| crate::compute_shuffled_index(index, size, &seed, rounds))
                        .collect();
                    let input: Vec<_> = (0..size).collect();

                    assert_eq!(
                        shuffle_list_branchless(input, rounds, &seed, false),
                        expected,
                        "size={size}, rounds={rounds}, seed={seed:?}"
                    );
                }
            }
        }

        let size = 4_097;
        let rounds = 17;
        let seed = [0x5a; 32];
        let expected: Option<Vec<_>> = (0..size)
            .map(|index| crate::compute_shuffled_index(index, size, &seed, rounds))
            .collect();

        assert_eq!(
            shuffle_list_branchless((0..size).collect(), rounds, &seed, false),
            expected
        );
    }

    #[test]
    fn forward_then_reverse_restores_input() {
        let variants: [(&str, ShuffleFn); 2] = [
            ("reference", shuffle_list),
            ("branchless", shuffle_list_branchless),
        ];

        for (variant, shuffle) in variants {
            for size in TEST_SIZES {
                let input: Vec<_> = (0..size).map(|value| value % 11).collect();

                for rounds in TEST_ROUNDS {
                    for seed in test_seeds() {
                        let restored = shuffle(input.clone(), rounds, &seed, true)
                            .and_then(|shuffled| shuffle(shuffled, rounds, &seed, false));

                        assert_eq!(
                            restored,
                            Some(input.clone()),
                            "variant={variant}, size={size}, rounds={rounds}, seed={seed:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn branchless_matches_reference_for_invalid_inputs() {
        assert_eq!(
            shuffle_list(vec![], 90, &[42, 42], true),
            shuffle_list_branchless(vec![], 90, &[42, 42], true)
        );
        assert_eq!(
            shuffle_list(vec![0], 0, &[42, 42], false),
            shuffle_list_branchless(vec![0], 0, &[42, 42], false)
        );
    }
}
