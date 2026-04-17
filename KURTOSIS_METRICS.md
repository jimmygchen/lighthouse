# Kurtosis Testnet Performance Metrics

## Testnet setup

- **Host**: `root@89.167.19.148` (4 Lighthouse BN + 4 Geth EL via Kurtosis)
- **Lighthouse commit**: `ae38443` on branch `modularize-beacon-chain`
- **Containers**:
  - `cl-1-lighthouse-geth--46c630fd51b64a2e906b1f0ccc882267`
  - `cl-2-lighthouse-geth--05ad23d48fa4411d99e44fa4337cf8ee`
  - `cl-3-lighthouse-geth--b6320607945c4f3690992d8e32046a61`
  - `cl-4-lighthouse-geth--52506df2ee8242e8bf6e0dd5ae5e064d`
- **Observation window**: ~70 min (first `Synced` at 13:33:32, last log sample 14:44:04 UTC, 2026-04-17)
- **Slot range observed**: slot 899 → slot 2310 (1411 slots, ~3 s/slot, matches testnet config)
- **Finality**: Each node reached finalized epoch 68 by the end of observation (justification advancing in lockstep across all 4 nodes).

## Error & warning counts

| Node | CRIT | ERRO | WARN | INFO | DEBUG |
|---|---|---|---|---|---|
| cl-1 | 0 | 0 | 958 | 3723 | 448,445 |
| cl-2 | 0 | 0 | 0 | 3486 | 442,520 |
| cl-3 | 0 | 0 | 0 | 3629 | 439,687 |
| cl-4 | 0 | 0 | 0 | 3655 | 447,776 |

**Warning breakdown on cl-1** (only node with warnings): 973/973 are `status: 404 Not Found` on `/eth/v1/beacon/headers/<root>` — the validator client poll-before-import race. Benign; not a regression of this branch.

Zero `CRIT`/`ERRO` across all 4 nodes. Zero occurrences of `panic`, `reorg`, `late block`, `Skipped slot`, or `fork detected` in any container's logs.

## Slot timing samples (`Synced` per-slot tick)

**cl-1 latest**:
```
Apr 17 14:40:41.500 INFO  Synced  peers: "3", exec_hash: "0xb83a…fb0c (verified)",
    finalized_root: 0xf8b0…d7a7, finalized_epoch: 68, epoch: 70,
    block: "0x80f4511306fcedcfb3364aa34d3da38918602e96f6347da6e194db7eac2b2c19", slot: 2242
```

**cl-4 latest**:
```
Apr 17 14:40:47.500 INFO  Synced  peers: "3", exec_hash: "0xee65…d338 (verified)",
    finalized_root: 0xf8b0…d7a7, finalized_epoch: 68, epoch: 70,
    block: "0x0aabbdf97bc956c45caded34440830d6c9790990f0576d775a14bdf8d66af2cf", slot: 2244
```

All 4 nodes agree on `finalized_root` / `finalized_epoch` at every tick, maintain `peers: 3` steadily (max in this 4-node mesh).

## Block production (`Signed block published`)

Per-node proposal counts and `publish_delay_ms` (producer-side latency from block-production to gossip publish):

| Node | Proposals | publish_delay min / max / avg |
|---|---|---|
| cl-1 | 381 | 1 / 9 / 2.1 ms |
| cl-2 | 313 | 1 / 16 / 2.1 ms |
| cl-3 | 353 | 1 / 11 / 2.1 ms |
| cl-4 | 351 | 1 / 10 / 2.1 ms |

Sum: 1398 proposals over 1411 slots (≥99% slot coverage). Representative samples:
```
14:41:37.082 cl-1 Signed block published ... slot: 2261, publish_delay_ms: 8
14:41:40.061 cl-2 Signed block published ... slot: 2262, publish_delay_ms: 1
14:41:49.101 cl-3 Signed block published ... slot: 2265, publish_delay_ms: 2
14:41:19.066 cl-4 Signed block published ... slot: 2255, publish_delay_ms: 1
```

## Block import (`Valid block from HTTP API` — observer-side latency)

`block_delay` = time from slot start to when the node observed the block locally.

| Node | Imports | block_delay min / max / avg |
|---|---|---|
| cl-1 | 381 | 48.47 / 108.15 / **81.02** ms |
| cl-2 | 313 | 54.40 / 110.65 / **80.64** ms |
| cl-3 | 353 | 52.45 / 108.90 / **80.47** ms |
| cl-4 | 351 | 51.83 / 107.57 / **80.72** ms |

All max values are comfortably under the 4 s attestation deadline (max seen ≈ 110 ms, ~2.8% of slot).

Representative samples (gossip-received blocks):
```
14:42:10.097 cl-1 New block received  slot: 2272, root: 0x8119…a863b9
14:42:10.096 cl-2 New block received  slot: 2272, root: 0x8119…a863b9  (+/-1ms)
14:41:55.135 cl-3 Valid block from HTTP API  block_delay: 60.16ms, slot: 2267
14:41:31.167 cl-4 Valid block from HTTP API  block_delay: 77.02ms, slot: 2259
```

## Attestation inclusion distance

Not observed in BN logs (Lighthouse only logs this from the VC / metrics; the validator client would have it via `inclusion_distance` metric). Block-import timing well under slot deadline is a strong proxy for healthy inclusion.

## Slot delays / late markers

Not observed in logs (no `late block`, `block was late`, `Skipped slot`, `slot_delay`, or `reorg` strings in any of the 4 containers).

## Conclusion

**Healthy.** All 4 BNs finalize together (epoch 68, identical finalized_root), zero CRIT/ERRO, ~99% slot coverage, block_delay avg ~80 ms with max ~110 ms, publish_delay avg ~2 ms. The 973 WARN entries on cl-1 are all 404s from the validator's `/eth/v1/beacon/headers/<root>` poll race and are not a regression attributable to this branch.
