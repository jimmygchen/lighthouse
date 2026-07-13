use std::hint::black_box;
use swap_or_not_shuffle::{shuffle_list, shuffle_list_branchless};

const SHUFFLE_ROUND_COUNT: u8 = 90;

type ShuffleFn = fn(Vec<usize>, u8, &[u8], bool) -> Option<Vec<usize>>;

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let variant = args.next().ok_or_else(|| {
        "usage: perf_shuffle <reference|branchless> <size> <iterations>".to_owned()
    })?;
    let size = args
        .next()
        .ok_or_else(|| "missing size".to_owned())?
        .parse::<usize>()
        .map_err(|error| format!("invalid size: {error}"))?;
    let iterations = args
        .next()
        .ok_or_else(|| "missing iterations".to_owned())?
        .parse::<usize>()
        .map_err(|error| format!("invalid iteration count: {error}"))?;

    if args.next().is_some() {
        return Err("unexpected extra argument".to_owned());
    }

    let shuffle = match variant.as_str() {
        "reference" => shuffle_list as ShuffleFn,
        "branchless" => shuffle_list_branchless as ShuffleFn,
        _ => return Err(format!("unknown variant: {variant}")),
    };

    let seed = [42; 32];
    let mut input: Vec<usize> = (0..size).collect();

    for _ in 0..iterations {
        input = shuffle(
            black_box(input),
            SHUFFLE_ROUND_COUNT,
            black_box(&seed),
            false,
        )
        .ok_or_else(|| "shuffle rejected valid profiling input".to_owned())?;
        input = black_box(input);
    }

    black_box(input);
    Ok(())
}
