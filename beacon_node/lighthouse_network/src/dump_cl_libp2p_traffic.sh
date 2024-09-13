#!/bin/bash
#
# This dump utility captures libp2p traffic between CL docker containers on a Kurtosis devnet.
# Output can be found in `libp2p_dump.log`.
#
# Usage:
# ./dump_cl_libp2p_traffic.sh <CL config YAML> <0xgenesis_validator_root> <output_dir>
#
# Example:
# ./dump_cl_libp2p_traffic.sh config.yaml 0x0ac76415092e45ba9068ffc47402b95f95cb0b1f1f5ab818b6ba5ddbe9e22c9e .

CONFIG_FILE=$1
GENESIS_VALIDATORS_ROOT=$2
OUT_BASE_DIR=$3

SUB_DIR=cl-libp2p-traffic-dump
OUTPUT_DIR=${OUT_BASE_DIR}/${SUB_DIR}

mkdir -p "$OUTPUT_DIR"

# Find the network ID of the Docker network named "kt-local-testnet"
network_id=$(docker network ls --filter name=kt-local-testnet -q)

# Find the corresponding bridge interface
if [ -n "$network_id" ]; then
    network_id=br-$network_id
else
    echo "'kt-local-testnet' network not found"
fi

TCP_DUMP_OUT=$OUTPUT_DIR/tcpdump.out
timeout 120s tcpdump -i $network_id 'tcp port 9000 or tcp port 13000' -s 0 -x > $TCP_DUMP_OUT

cl-network-packet-parser $TCP_DUMP_OUT $OUTPUT_DIR/libp2p_dump.log "$CONFIG_FILE" "$GENESIS_VALIDATORS_ROOT"

echo "Done"
