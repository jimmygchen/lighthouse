# `libp2p` Traffic Dump

This dump utility captures libp2p traffic between CL docker containers on a Kurtosis devnet.

Source Code: [beacon_node/lighthouse_network/src/main.rs](../../beacon_node/lighthouse_network/src/main.rs)

Example Output: [libp2p_traffic_dump.log](libp2p_traffic_dump.log)

WIP:
- 🟨 RPC Request (partially working)
- 🟨 RPC Response (partially working)
- ❌ Gossip (not working)

## Basic Usage with Kurtosis

```shell
# Install the binary
cargo install --path beacon_node/lighthouse_network --bin libp2p-packet-parser --force --locked

# Make sure a Kurtosis local testnet is running, replace the enclave name and run:
./watch-libp2p-traffic.sh [ENCLAVE_NAME]
```

## Manually Run `tcpdump` 

Capture a tcpdump (terminate by Ctrl+C):

```shell
tcpdump -i br-$(docker network ls --filter name=kt-${ENCLAVE_NAME} -q) 'tcp port 9000 or tcp port 13000' -s 0 -x > tcpdump.out`
```

Replace `config.yaml` and `genesis_validators_root`, then run:

```shell
cargo run --bin libp2p-packet-parser --manifest-path ../../beacon_node/lighthouse_network/Cargo.toml -- \
    ./tcp_dump.out ./libp2p_traffic_dump.log ./config.yaml <0xgenesis_validators_root>
```
