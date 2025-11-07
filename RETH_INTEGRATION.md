# Reth In-Process Integration

This document describes the integration of Reth execution engine directly into Lighthouse as a single binary, replacing the traditional HTTP JSON-RPC communication with direct in-process function calls.

## Overview

Lighthouse can now run with an embedded Reth execution engine in the same process, eliminating HTTP overhead and enabling direct memory-based communication via Rust channels.

**Status**: Proof of concept implementation - compiles and demonstrates the architecture.

## Architecture

### Traditional Multi-Process Model
```
┌─────────────┐    HTTP JSON-RPC    ┌──────────────┐
│ Lighthouse  │ ←─────────────────→ │     Reth     │
│ (Consensus) │    Engine API       │ (Execution)  │
└─────────────┘                     └──────────────┘
  Separate process                   Separate process
```

### New In-Process Model
```
┌─────────────────────────────────────────────────┐
│              Single Lighthouse Binary            │
│                                                  │
│  ┌─────────────┐          ┌──────────────┐     │
│  │ Lighthouse  │          │     Reth     │     │
│  │ (Consensus) │ ←──────→ │ (Execution)  │     │
│  └─────────────┘  Rust    └──────────────┘     │
│                   Channels                       │
└─────────────────────────────────────────────────┘
      Same process - Direct function calls
```

## Implementation Details

### Key Components

**RethEngineApi** (`beacon_node/execution_layer/src/reth_engine_api.rs`)
- Stores `ConsensusEngineHandle<EthEngineTypes>` - Reth's channel-based engine handle
- Implements the same Engine API interface as HttpJsonRpc
- Converts types between Lighthouse and Reth representations
- Launches Reth node and extracts the consensus engine handle

**Integration Points**
- `engines.rs`: Engine enum now contains `RethEngineApi` instead of `HttpJsonRpc`
- `lib.rs`: ExecutionLayer instantiates `RethEngineApi::new()` instead of HTTP client

### Message Flow

```
Lighthouse calls: api.forkchoice_updated(state, attrs)
         ↓
RethEngineApi converts Lighthouse types → Reth types
         ↓
Calls: self.reth_handle.fork_choice_updated(...)
         ↓
ConsensusEngineHandle sends BeaconEngineMessage via tokio channel
         ↓
Reth engine task receives and processes message
         ↓
Response sent back through channel
         ↓
RethEngineApi converts Reth types → Lighthouse types
         ↓
Returns to Lighthouse
```

### Type Conversions

The integration handles conversion between Lighthouse and Reth types:

- **ForkchoiceState**: ExecutionBlockHash (Lighthouse) ↔ B256 (Reth/Alloy)
- **PayloadStatus**: PayloadStatusV1 (Lighthouse) ↔ PayloadStatus (Reth)
- **PayloadId**: [u8; 8] (Lighthouse) ↔ PayloadId (Reth)

All conversions are zero-copy where possible, converting references rather than cloning data.

## Current Limitations

### Development Mode
- Uses `.testing_node()` with in-memory database for development
- Uses `.dev()` mode which is not suitable for production
- Database is ephemeral and lost on restart

### TODO for Production
1. **Persistent Database**: Replace `testing_node()` with proper database setup
   - Open/create MDBX or RocksDB database on disk
   - Handle database initialization and migrations
   - Configure proper data directory from Lighthouse config

2. **Configuration Integration**:
   - Pass data directory from Lighthouse CLI
   - Support different chain specs (mainnet, sepolia, holesky)
   - Configure database backend choice
   - Network configuration (P2P, discovery)

3. **Missing Engine API Methods**:
   - `new_payload()` - needs full implementation
   - `get_payload()` - needs full implementation
   - Block/blob retrieval methods

4. **Error Handling**:
   - Proper error propagation from Reth
   - Graceful shutdown coordination
   - Handle Reth initialization failures

5. **Payload Attributes Conversion**:
   - Currently stubbed with `todo!()`
   - Needs complete conversion implementation

## Dependencies

Added Reth crates (from git, main branch):
- `reth-engine-primitives` - Core engine types
- `reth-ethereum` - Ethereum-specific node implementation
- `reth-ethereum-engine-primitives` - Ethereum engine types
- `reth-node-builder` - Node construction utilities
- `reth-chainspec` - Chain specification
- `reth-db` - Database interfaces
- `reth-tasks` - Task management
- `alloy-rpc-types-engine` - Alloy Engine API types

## Performance Benefits (Expected)

- **Latency**: ~10x reduction (from 2-5ms to 0.1-0.5ms per Engine API call)
- **Throughput**: Higher message throughput via channels vs HTTP
- **Resource Usage**: Single process reduces memory and CPU overhead
- **Deployment**: Simpler deployment with single binary

## Testing

The integration includes a stub mode (`RethEngineApi::new_stub()`) that can be used for testing without launching a real Reth node. The stub processes messages and returns mock responses, useful for validating the architecture and message flow.

## Building

The integration compiles successfully:
```bash
cargo check --package execution_layer
```

Full Lighthouse build:
```bash
cargo build --release
```

## Future Work

This is a proof-of-concept demonstrating the feasibility of in-process integration. To make it production-ready:

1. Implement persistent database setup
2. Add proper configuration passing from Lighthouse
3. Complete remaining Engine API method implementations
4. Add comprehensive error handling
5. Implement graceful shutdown
6. Add metrics and monitoring
7. Thorough testing with testnet sync

## References

- Reth ConsensusEngineHandle: `reth/crates/engine/primitives/src/message.rs`
- Reth NodeBuilder examples: `reth/examples/`
- Lighthouse Engine API: `beacon_node/execution_layer/src/engine_api.rs`
