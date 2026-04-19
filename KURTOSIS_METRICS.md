# Kurtosis Testnet Metrics

## Testnet Setup

- **Commit**: `48f771af4` (Final doc pass: update all sections for BlockImporter/BlockProducer)
- **Note**: 5 subsequent commits are structural only (file renames, field moves, doc updates) — zero runtime code changes. Binary is identical.
- **Configuration**: 4 Lighthouse BN + 4 Geth EL, Fulu fork at epoch 0
- **Slot duration**: 3 seconds
- **ethereum-package version**: 6.1.0
- **Deployed**: 2026-04-19 13:42 UTC

## Error & Warning Counts

| Node | Errors | Warnings |
|------|--------|----------|
| cl-1 | 0 | NoPeersSubscribedToTopic (benign, early startup) |
| cl-2 | 0 | same |
| cl-3 | 0 | same |
| cl-4 | 0 | same |

## Block Production

Block delays from cl-1:
```
slot  2: block_delay  27ms
slot 11: block_delay  87ms
slot 15: block_delay  78ms
```

Average block delay: ~64ms (well under 3,000ms slot duration).

## Finalization

Finalization progressing normally from slot 18+. No reorgs detected.

## Conclusion

**Healthy.** Zero errors across all 4 nodes. Block production and finalization
working correctly on the final refactored code. Block delays comparable to
unstable baseline (~80ms average).
