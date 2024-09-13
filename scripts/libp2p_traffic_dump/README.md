# `libp2p` Traffic Dump

This dump utility captures libp2p traffic between CL docker containers on a Kurtosis devnet.

Source Code: `beacon_node/lighthouse_network/src/main.rs:21`

Example Output: [libp2p_traffic_dump.log](libp2p_traffic_dump.log)

WIP:
- 🟨 RPC Request (partially working)
- 🟨 RPC Response (partially working)
- ❌ Gossip (not working)

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

## Run `tcpdump` and parse with a script (UNTESTED)

Note: Requires the `cl-network-packet-parser` binary to be pre-built.

```shell
./libp2p_traffic_dump.sh <cl_config_yaml> <0xgenesis_validator_root> <kt_enclave_name> <output_dir>
```
