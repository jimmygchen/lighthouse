# Fullhouse POC (Lighthouse-Reth single binary)

Integration of Reth execution engine directly into Lighthouse-Reth as a single binary with direct function calls instead
of HTTP JSON-RPC.

Progress documentation: https://hackmd.io/@jimmygchen/rkx04EeeWl

## Architecture

### Existing: Multi-Process via HTTP

```
┌─────────────┐    HTTP JSON-RPC    ┌──────────────┐
│ Lighthouse  │ ←─────────────────→ │     Reth     │
│ (Consensus) │                     │ (Execution)  │
└─────────────┘                     └──────────────┘
```

### New: In-Process Direct Calls

```
┌─────────────────────────────────────────────────┐
│           Single Lighthouse-Reth Binary         │
│  ┌─────────────┐          ┌──────────────┐      │
│  │ Lighthouse  │ ←──────→ │     Reth     │      │
│  │ (Consensus) │  Direct  │ (Execution)  │      │
│  └─────────────┘  Calls   └──────────────┘      │
└─────────────────────────────────────────────────┘
```

## Implementation

### Key Files

- `beacon_node/execution_layer/src/reth_engine_api.rs` - Main integration

### Implemented Methods (Direct Integration)

- ✅ `forkchoice_updated`
- ✅ `new_payload`
- ✅ `get_payload`
- ✅ `get_blobs_v1`
- ✅ `get_blobs_v2`

Other methods fall back to Engine JSON API.

### Network Support

- Mainnet, Sepolia, Holesky, Hoodi
- Network automatically passed from Lighthouse's `--network` flag

### Data Storage

- Lighthouse: `~/.lighthouse/<network>/beacon/`
- Reth: `~/.lighthouse/<network>/beacon/reth/`

## Usage

```bash
# Run a hoodie node
cargo run --release --bin lighthouse -- bn --network hoodi --checkpoint-sync-url https://hoodi.beaconstate.info
```
