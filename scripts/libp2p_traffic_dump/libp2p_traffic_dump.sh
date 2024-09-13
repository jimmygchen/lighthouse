#!/bin/bash
#
# This dump utility captures libp2p traffic between CL docker containers on a Kurtosis devnet.
# Output can be found in `libp2p_traffic_dump.log`.
#
# Limitation: The tcpdump usage here only works on linux.
#
# Usage:
# ./libp2p_traffic_dump.sh <cl_config_yaml> <0xgenesis_validator_root> <kt_enclave_name> <output_dir>
#
# Example:
# ./libp2p_traffic_dump.sh config.yaml 0x0ac76415092e45ba9068ffc47402b95f95cb0b1f1f5ab818b6ba5ddbe9e22c9e local-testnet .
#
# Note: Requires the `cl-network-packet-parser` binary.
# Source code: beacon_node/lighthouse_network/src/main.rs:21

set -e

CONFIG_FILE=$1
GENESIS_VALIDATORS_ROOT=$2
ENCLAVE_NAME=${3:-local-testnet}
OUTPUT_DIR=${4:-.}
DUMP_DURATION=20s

mkdir -p "$OUTPUT_DIR"

# Find the network ID of the Docker network named "kt-<enclave_name>"
network_id=$(docker network ls --filter name=kt-${ENCLAVE_NAME} -q)

# Find the corresponding bridge interface
if [ -n "$network_id" ]; then
    network_id=br-$network_id
else
    echo "'kt-local-testnet' network not found"
fi

echo "Running tcpdump for $DUMP_DURATION.."
TCP_DUMP_OUT=$OUTPUT_DIR/tcp_dump.out

# tcpdump requires sudo
sudo -v
timeout $DUMP_DURATION sudo tcpdump -i $network_id 'tcp port 9000 or tcp port 13000' -s 0 -x > $TCP_DUMP_OUT

echo "tcpdump complete."

OUTPUT_FILE=$OUTPUT_DIR/libp2p_traffic_dump.log
cl-network-packet-parser $TCP_DUMP_OUT "$OUTPUT_FILE" "$CONFIG_FILE" "$GENESIS_VALIDATORS_ROOT"

echo "Output generated at $OUTPUT_FILE".
