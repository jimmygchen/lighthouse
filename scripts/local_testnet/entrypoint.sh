#!/bin/bash

# Set ulimit for core dumps
ulimit -c unlimited

# Set the core pattern to save core dumps in /core_dumps if it is writable
if [ -w /proc/sys/kernel/core_pattern ]; then
    echo "/core_dumps/core.%e.%p" > /proc/sys/kernel/core_pattern
fi

# Create and change to the core dump directory
mkdir -p /core_dumps
cd /core_dumps

# Execute the passed command
exec "$@"
