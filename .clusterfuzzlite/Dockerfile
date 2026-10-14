# ClusterFuzzLite builder image for ts-transformer.
#
# Twin of oss-fuzz/Dockerfile. The one structural difference: OSS-Fuzz
# clones the repo inside the image, ClusterFuzzLite copies the checkout it
# was invoked on (the PR head or the scheduled main) — so the source is
# whatever git state the workflow saw, not a fresh clone of main.
FROM gcr.io/oss-fuzz-base/base-builder-rust

# cmake builds the vendored libsrt + mbedTLS; pkg-config and libclang serve
# the -sys crates' build scripts (bindgen).
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    pkg-config \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

COPY . $SRC/ts-transformer
WORKDIR $SRC/ts-transformer

# The ClusterFuzzLite action checks the commit out with every submodule
# initialised recursively, so the vendored trees arrive with the COPY. A
# local image build from a checkout that never fetched them would fail
# much later inside cmake; refuse up front instead. build.sh needs libsrt
# and mbedTLS (with mbedTLS's nested `framework` submodule) and nothing
# from the embedded-only submodules.
RUN test -f crates/srt-sys/vendor/srt/CMakeLists.txt \
    && test -f crates/mbedtls-src/vendor/mbedtls/framework/CMakeLists.txt \
    || { echo "vendored submodules missing: run 'git submodule update --init --recursive crates/srt-sys/vendor/srt crates/mbedtls-src/vendor/mbedtls' in the checkout first" >&2; exit 1; }

COPY ./.clusterfuzzlite/build.sh $SRC/
