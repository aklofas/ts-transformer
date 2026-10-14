#!/bin/bash -eu
# ClusterFuzzLite entry point. The base image's `compile` runs $SRC/build.sh;
# the real build lives in oss-fuzz/build.sh so the fleet bundle and the
# in-repo ClusterFuzzLite bundle cannot drift apart. That script expects to
# run from the workspace root (it pushd's into each fuzz crate).
cd "$SRC/ts-transformer"
exec bash oss-fuzz/build.sh
