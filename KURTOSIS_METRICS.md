# Kurtosis Testnet Performance Metrics

## Testnet setup

- **Host**: `root@89.167.19.148` (4 Lighthouse BN + 4 Geth EL via Kurtosis)
- **Lighthouse commit**: `ae38443` on branch `modularize-beacon-chain`
- **Enclave**: `local-testnet` (created Fri, 17 Apr 2026 12:47:20 UTC)
- **Containers**:
  - `cl-1-lighthouse-geth--46c630fd51b64a2e906b1f0ccc882267`
  - `cl-2-lighthouse-geth--05ad23d48fa4411d99e44fa4337cf8ee`
  - `cl-3-lighthouse-geth--b6320607945c4f3690992d8e32046a61`
  - `cl-4-lighthouse-geth--52506df2ee8242e8bf6e0dd5ae5e064d`
- **Capture time**: 2026-04-18 ~15:55 UTC (testnet running ~27 hours)
- **Slot range observed**: slot ~30969 → slot ~32530 (~1561 slots visible in log buffer)
- **Finality**: All nodes at finalized epoch 1014, current epoch 1016, identical `finalized_root: 0x3bf7…571e`.

## Error & warning counts

| Node | CRIT | ERRO | WARN |
|---|---|---|---|
| cl-1 | 0 | 0 | 1000 |
| cl-2 | 0 | 0 | 0 |
| cl-3 | 0 | 0 | 0 |
| cl-4 | 0 | 0 | 0 |

**Warning breakdown on cl-1**: All 1000 warnings are `status: 404 Not Found` on `/eth/v1/beacon/headers/<root>` — the validator client poll-before-import race. Benign; not a regression of this branch.

Zero `CRIT`/`ERRO` across all 4 nodes.

## Peer counts

All 4 nodes maintain `peers: 3` (maximum in a 4-node mesh).

## Slot timing samples (`Synced` per-slot tick)

**cl-1 latest**:
```
Apr 18 15:54:26.505 INFO  Synced  peers: "3", exec_hash: "0xac54…2661 (verified)",
    finalized_root: 0x3bf7…571e, finalized_epoch: 1014, epoch: 1016,
    block: "0x8f3dda36…af237f", slot: 32517
```

**cl-4 latest**:
```
Apr 18 15:55:05.501 INFO  Synced  peers: "3", exec_hash: "0x1b62…aa46 (verified)",
    finalized_root: 0x3bf7…571e, finalized_epoch: 1014, epoch: 1016,
    block: "0x58659490…f09ac5", slot: 32530
```

All 4 nodes agree on `finalized_root` / `finalized_epoch`.

## Block production (`Produced block on state`)

Recent samples per node (block_size in bytes):

| Node | Sample block sizes |
|---|---|
| cl-1 | 8623, 8623, 8623, 8623, 8828 |
| cl-2 | 8623, 8621, 8497, 8623, 8623 |
| cl-3 | 8498, 8623, 8623, 8622, 8623 |
| cl-4 | 8622, 8416, 8747, 8623, 8499 |

## Block import (`Valid block from HTTP API` — observer-side latency)

`block_delay` = time from slot start to when the node validated the block.

**cl-1** (5 most recent):
```
block_delay: 84.40ms, slot: 32512
block_delay: 77.79ms, slot: 32513
block_delay: 71.03ms, slot: 32515
block_delay: 83.07ms, slot: 32516
block_delay: 82.28ms, slot: 32517
```

**cl-2**:
```
block_delay: 82.39ms, slot: 32506
block_delay: 69.46ms, slot: 32507
block_delay: 78.31ms, slot: 32518
block_delay: 80.26ms, slot: 32519
block_delay: 71.25ms, slot: 32523
```

**cl-3**:
```
block_delay: 88.82ms, slot: 32509
block_delay: 87.15ms, slot: 32511
block_delay: 71.60ms, slot: 32514
block_delay: 79.32ms, slot: 32521
block_delay: 69.86ms, slot: 32522
```

**cl-4**:
```
block_delay: 77.22ms, slot: 32496
block_delay: 90.98ms, slot: 32505
block_delay: 90.01ms, slot: 32510
block_delay: 81.79ms, slot: 32525
block_delay: 80.14ms, slot: 32526
```

All block delays are well under 100ms (max ~91ms), comfortably within the 4s attestation deadline.

## Conclusion

**Healthy.** Testnet has been running ~27 hours. All 4 BNs finalize together (epoch 1014, identical finalized_root), zero CRIT/ERRO, block_delay avg ~80ms with max ~91ms. The 1000 WARN entries on cl-1 are all 404s from the validator's header poll race and are not a regression. Network is stable with full peer connectivity.
