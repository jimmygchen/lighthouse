# Light Client

## Future Tasks

- Impl light client [sync](https://github.com/ethereum/consensus-specs/blob/dev/specs/altair/light-client/light-client.md#light-client-sync-process).
- Impl spec helper functions
  - complete `initialize_light_client_store`
- Verify trusted block root
- Verify fork digest when connecting to beacon node via REST APIs.
- Add `config.genesis_state_url` fall back to `config.beacon_node` if this is not specified.
- Add timeouts for light client endpoints to `BeaconNodeHttpClient`.
- Add metrics.
- Add `beacon_nodes_tls_certs`.
- Remove `async-trait` crate.