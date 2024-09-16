#!/bin/bash
#
# This dump utility captures libp2p traffic between CL docker containers on a Kurtosis devnet.
#
# Notes:
# - Requires the `libp2p-packet-parser` binary.
# - The tcpdump usage here only works on linux.

set -e
set -u
set -o pipefail

ENCLAVE=${1:-local-testnet}

function to_container_name() {
  while IFS= read -r log_line; do
    # Extract the source and destination IPs from the log line using improved regex
    src_ip=$(echo "$log_line" | grep -oP 'Source: \K[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+')
    dst_ip=$(echo "$log_line" | grep -oP 'Dest: \K[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+')

    # Only proceed if both IPs were found
    if [[ -n "$src_ip" && -n "$dst_ip" ]]; then
      # Find the container names for the source and destination IPs
      src_container=$(docker inspect -f '{{.Name}}' $(docker ps -q) | while read -r container; do
        if [[ "$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' $container)" == "$src_ip" ]]; then
          echo $container | sed 's/^\///' | sed 's/--.*//' # Strip everything after '--'
          break
        fi
      done)

      dst_container=$(docker inspect -f '{{.Name}}' $(docker ps -q) | while read -r container; do
        if [[ "$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' $container)" == "$dst_ip" ]]; then
          echo $container | sed 's/^\///' | sed 's/--.*//' # Strip everything after '--'
          break
        fi
      done)

      # Replace the IPs and ports with the container names in the log line
      updated_log_line=$(echo "$log_line" | sed "s/$src_ip\.[0-9]*/$src_container/" | sed "s/$dst_ip\.[0-9]*/$dst_container/")

      # Output the result
      echo "$updated_log_line"
    else
      # If IPs not found, just print the original log line
      echo "$log_line"
    fi
  done
}

rm -rf el_cl_genesis_data genesis_validators_root && \
  kurtosis files download $ENCLAVE el_cl_genesis_data && \
  kurtosis files download $ENCLAVE genesis_validators_root && \
  GENESIS_VALIDATORS_ROOT=$(cat ./genesis_validators_root/genesis_validators_root.txt) && \
  NETWORK_NAME=br-$(docker network ls --filter name=kt-$ENCLAVE -q)
  tcpdump -i $NETWORK_NAME 'tcp port 9000 or tcp port 13000' -s 0 -x | \
  libp2p-packet-parser ./el_cl_genesis_data/config.yaml $GENESIS_VALIDATORS_ROOT | to_container_name
