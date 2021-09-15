#!/bin/bash
set -xeuo pipefail

# Prow jobs don't support adding emptydir today
export COSA_SKIP_OVERLAY=1
# And suppress depcheck since we didn't install via RPM
export COSA_SUPPRESS_DEPCHECK=1
cd $(mktemp -d)
cosa init https://github.com/coreos/fedora-coreos-config/
rsync -rlv /build/ overrides/rootfs/
cosa fetch
cosa build
cosa kola run 'ext.rpm-ostree.*'
