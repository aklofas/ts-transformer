#!/usr/bin/env bash
# Parameterized firmware build for the freertos-srt product. Sourced by the
# top-level build.sh dispatcher, which sets the per-target knobs:
#   LWIP=0|1  LIBSRT=0|1  NETIF=none|lossy|lan9118  ENCRYPT=0|1
#   APP="<app .cpp ...>"  DEFS="<extra -D...>"
# All generated artifacts (objects, firmware.elf, generated headers, the staged
# libsrt source, and the cross-built libsrt/mbedTLS trees) land under a single
# ignored build/ directory so the product root stays easy to scan. `REBUILD=1`
# forces a full wipe of the staged source, BOTH ENCRYPT variants of srt-install*,
# and mbedTLS, then rebuilds from scratch. The cross-build also self-invalidates
# via a stamp that covers this script, the toolchain/cmake files, all substrate
# config/shim headers (posix-shims/**, freertos/*.h, lwip/lwipopts.h + lwip/arch/*.h,
# mbedtls-user-config.h), and all 5 vendored submodule HEADs (srt, mbedtls,
# freertos-kernel, freertos-plus-posix, lwip). Wipe everything with:
# rm -rf build/  (or build.sh clean).
set -euo pipefail
PROD=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)   # embedded/freertos-srt
SUB="$PROD/substrate"
ROOT=$(cd "$PROD/../.." && pwd)                          # workspace root
TOOLCHAIN="$(sed -nE 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' "$ROOT/rust-toolchain.toml")"
cd "$PROD"

# Single ignored output root. obj/ = compiled objects; generated/ = golden.h;
# srt-src/ = staged pristine libsrt source (patched there, not in crates/srt-sys/vendor/srt);
# srt-build*/srt-install*/mbedtls-* = the cross-built dependency trees.
BUILD="$PROD/build"
OBJ="$BUILD/obj"
GEN="$BUILD/generated"
mkdir -p "$BUILD" "$OBJ" "$GEN"

K=$ROOT/embedded/vendor/freertos-kernel
P=$ROOT/embedded/vendor/freertos-plus-posix
PS=$P/FreeRTOS-Plus-POSIX/source
L=$ROOT/embedded/vendor/lwip
SRT=$ROOT/crates/srt-sys/vendor/srt
MBED=$ROOT/crates/mbedtls-src/vendor/mbedtls
CC=arm-none-eabi-gcc; CXX=arm-none-eabi-g++

ARCH="-mcpu=cortex-m4 -mthumb -mfloat-abi=hard -mfpu=fpv4-sp-d16"
OPT="-Os -ffunction-sections -fdata-sections -g"

# Generate golden.h (564B video-roundtrip) into build/generated/ — regenerated
# every build via the shared atomic generator (see gen-golden-h.sh for why).
GOLDEN_TS=$ROOT/crates/tst-integration/tests/fixtures/scenarios/video-roundtrip/output.ts
bash "$ROOT/embedded/scripts/lib/gen-golden-h.sh" "$GOLDEN_TS" "$GEN/golden.h"

# Base includes: substrate roots (lwipopts.h, FreeRTOSConfig.h, arch/, golden.h,
# srt_opts.h, drivers) + FreeRTOS + FreeRTOS-Plus-POSIX. posix-shims + lwIP +
# srt includes are appended only for the layers that need them.
INC="-I$SUB -I$GEN -I$SUB/freertos -I$SUB/lwip -I$SUB/drivers \
     -I$K/include -I$K/portable/GCC/ARM_CM4F \
     -I$P/include -I$P/include/private \
     -I$P/FreeRTOS-Plus-POSIX/include -I$P/FreeRTOS-Plus-POSIX/include/portable"

# Substrate C always present: startup, clock, stubs, FreeRTOS kernel + POSIX.
# (net_shim.c is the libsrt-on-lwIP __wrap_setsockopt shim — it needs lwIP
# headers and is only meaningful on the data-plane targets, so it rides with the
# netif drivers below, exactly as in loopback-arq/example. The non-netif targets never call
# setsockopt, so -Wl,--wrap=setsockopt stays inert for them.)
C_SRC="$SUB/startup.c $SUB/diag.c $SUB/newlib_lock.c $SUB/clock_shim.c $SUB/syscalls_stub.c $SUB/atomic64_stub.c \
       $K/tasks.c $K/list.c $K/queue.c $K/timers.c $K/event_groups.c \
       $K/portable/GCC/ARM_CM4F/port.c $K/portable/MemMang/heap_4.c \
       $PS/FreeRTOS_POSIX_pthread.c $PS/FreeRTOS_POSIX_pthread_mutex.c \
       $PS/FreeRTOS_POSIX_pthread_cond.c $PS/FreeRTOS_POSIX_pthread_barrier.c \
       $PS/FreeRTOS_POSIX_clock.c $PS/FreeRTOS_POSIX_utils.c \
       $PS/FreeRTOS_POSIX_sched.c $PS/FreeRTOS_POSIX_unistd.c \
       $PS/FreeRTOS_POSIX_timer.c $PS/FreeRTOS_POSIX_semaphore.c"

if [ "${LWIP:-0}" = "1" ]; then
  INC="$INC -I$L/src/include -I$L/src/include/compat/posix"
  C_SRC="$C_SRC $SUB/lwip/sys_arch.c $L/src/core/*.c $L/src/core/ipv4/*.c $L/src/core/ipv6/*.c $L/src/api/*.c $L/src/netif/ethernet.c"
fi
case "${NETIF:-none}" in
  lossy)   C_SRC="$C_SRC $SUB/drivers/lossy_netif.c $SUB/net_shim.c" ;;
  lan9118) C_SRC="$C_SRC $SUB/drivers/lan9118_netif.c $SUB/net_shim.c" ;;
esac

# TSTC=1 (srt-recv only): link the offline tst_demuxer_* C ABI, via the same
# thumbv7em-none-eabihf libtstrans_firmware.a embedded/baremetal-qemu-c builds
# for the firmware-qemu.sh gate (tst-c-core no_std + a global allocator
# forwarding to newlib memalign/free + an abort()-on-panic handler — see that
# crate's src/lib.rs). Reused as-is rather than duplicated: the glue is
# RTOS-agnostic (Cortex-M critical-section + newlib heap calls), so it
# composes with FreeRTOS's own newlib_lock.c-backed heap_4 allocator here
# exactly as it does standalone there.
TSTC_LIB=""
if [ "${TSTC:-0}" = "1" ]; then
  TSTC_CRATE="$ROOT/embedded/baremetal-qemu-c"
  # Self-heal the Rust target, mirroring firmware-qemu.sh (which builds this
  # same crate): a box with the ARM toolchain + QEMU + cmake + cargo but
  # without the rustup target installed would otherwise hard-fail on a cargo
  # error instead of skipping cleanly — a rustup target isn't a binary on
  # PATH, so the `need` guards in freertos-srt.sh structurally can't cover it.
  rustup target add thumbv7em-none-eabihf --toolchain "$TOOLCHAIN" >/dev/null 2>&1 || true
  ( cd "$TSTC_CRATE" && cargo build --release --locked )
  TSTC_LIB="$TSTC_CRATE/target/thumbv7em-none-eabihf/release/libtstrans_firmware.a"
  [ -f "$TSTC_LIB" ] || { echo "FATAL: $TSTC_LIB missing after cargo build" >&2; exit 1; }
  INC="$INC -I$ROOT/bindings/c/include"
fi

# libsrt (+ optional mbedTLS) cross-build, keyed on ENCRYPT so plain/AES trees
# don't clobber. Only for LIBSRT targets. Everything lands under build/.
SRT_LIB=""; MBED_LIBS=""; MBED_INSTALL="$BUILD/mbedtls-install"
if [ "${LIBSRT:-0}" = "1" ]; then
  SRT_INSTALL="$BUILD/srt-install${ENCRYPT:-0}"; SRT_BUILD="$BUILD/srt-build${ENCRYPT:-0}"
  SRT_SRC="$BUILD/srt-src"

  # Cache invalidation: a stamp hashing every input the cross-build consumes —
  # this script, the toolchain/cmake files, the patch set, the substrate
  # config/shim headers that arm-none-eabi.cmake -include's/-I's into the
  # cross-compile (posix-shims/**, freertos/*.h, lwip/lwipopts.h + lwip/arch/*.h,
  # mbedtls-user-config.h), and the vendored submodule HEADs (srt, mbedtls, and
  # the embedded freertos-kernel/freertos-plus-posix/lwip trees whose headers
  # the cross-build includes). When the stamp changes — or under REBUILD=1 —
  # blow away the staged source + BOTH ENCRYPT trees + mbedTLS so a stale
  # static lib can never survive an input edit. Hashing is fail-loud: a
  # missing input dir/file aborts the build rather than silently hashing less.
  STAMPFILE="$BUILD/.cross-build-stamp"
  for d in "$SUB/posix-shims" "$SUB/freertos" "$SUB/lwip" "$SUB/patches"; do
    [ -d "$d" ] || { echo "FATAL: stamp input dir missing: $d" >&2; exit 1; }
  done
  mapfile -t STAMP_FILES < <(
    printf '%s\n' "${BASH_SOURCE[0]}" "$SUB/arm-none-eabi.cmake" \
      "$SUB/mbedtls/mbedtls-toolchain.cmake" "$SUB/mbedtls/mbedtls-user-config.h"
    find "$SUB/patches" -type f -name '*.patch' | sort
    find "$SUB/posix-shims" "$SUB/freertos" "$SUB/lwip" -type f -name '*.h' | sort
  )
  # Digests only — sha256sum's raw output embeds absolute paths, which would
  # bust the stamp on a checkout move despite identical contents. Ordering
  # stays deterministic (STAMP_FILES is sorted), and pipefail (active in the
  # sourcing shell) keeps a sha256sum failure fail-loud through the pipe.
  FILE_HASHES=$(sha256sum -- "${STAMP_FILES[@]}" | awk '{print $1}')
  VENDOR_HEADS=$(for r in "$SRT" "$MBED" "$K" "$P" "$L"; do
                   git -C "$r" rev-parse HEAD || exit 1
                 done)
  NEWSTAMP=$(printf '%s\n%s\n' "$FILE_HASHES" "$VENDOR_HEADS" | sha256sum | awk '{print $1}')
  # Sweep doomed trees left by an interrupted earlier wipe (see rename-first below).
  rm -rf "$BUILD"/.srt-src.doomed.*
  if [ "$NEWSTAMP" != "$(cat "$STAMPFILE" 2>/dev/null || true)" ] || [ "${REBUILD:-0}" = "1" ]; then
    echo "freertos-srt: cross-build inputs changed (or REBUILD=1) -> rebuilding libsrt/mbedTLS"
    # srt-src is wiped rename-first: mv is atomic, so srt-src only ever exists
    # as a complete tree — an interrupted rm of the renamed dir is re-swept on
    # the next run instead of a half tree being silently reused. The build/
    # install trees are guarded by their key-artifact existence checks below
    # (a half-deleted install tree fails loudly at compile time).
    if [ -d "$SRT_SRC" ]; then mv "$SRT_SRC" "$BUILD/.srt-src.doomed.$$"; fi
    rm -rf "$BUILD"/srt-build* "$BUILD"/srt-install* "$BUILD/mbedtls-build" "$MBED_INSTALL" \
           "$BUILD"/.srt-src.doomed.*
  fi

  SRT_ENC_FLAGS="-DENABLE_ENCRYPTION=OFF"
  if [ "${ENCRYPT:-0}" = "1" ]; then
    if [ ! -f "$MBED_INSTALL/lib/libmbedcrypto.a" ]; then
      # CMAKE_WARN_DEPRECATED=OFF: the pinned submodule declares
      # cmake_minimum_required 3.5.1 — newer CMakes print a deprecation
      # banner about it that we can't act on without a submodule bump.
      cmake -S "$MBED" -B "$BUILD/mbedtls-build" \
        -DCMAKE_TOOLCHAIN_FILE="$SUB/mbedtls/mbedtls-toolchain.cmake" \
        -DCMAKE_INSTALL_PREFIX="$MBED_INSTALL" -DCMAKE_BUILD_TYPE=MinSizeRel \
        -DUSE_SHARED_MBEDTLS_LIBRARY=OFF -DUSE_STATIC_MBEDTLS_LIBRARY=ON \
        -DENABLE_TESTING=OFF -DENABLE_PROGRAMS=OFF -DMBEDTLS_FATAL_WARNINGS=OFF \
        -DCMAKE_WARN_DEPRECATED=OFF \
        -DMBEDTLS_USER_CONFIG_FILE="$SUB/mbedtls/mbedtls-user-config.h"
      cmake --build "$BUILD/mbedtls-build" --target install -j"$(nproc)"
    fi
    SRT_ENC_FLAGS="-DENABLE_ENCRYPTION=ON -DUSE_ENCLIB=mbedtls -DCMAKE_PREFIX_PATH=$MBED_INSTALL -DCMAKE_FIND_ROOT_PATH=$MBED_INSTALL"
    MBED_LIBS="-L$MBED_INSTALL/lib -lmbedtls -lmbedx509 -lmbedcrypto"
  fi

  # Stage a pristine copy of the pinned libsrt source and patch it THERE — never
  # mutate crates/srt-sys/vendor/srt (shared submodule; an interrupted in-place patch left it
  # dirty for every other consumer). `git archive HEAD` gives exactly the tracked
  # source at the pinned commit. Apply with plain `patch`, NOT `git apply`: the
  # staged tree sits under build/ inside this repo's worktree, so `git apply`
  # would discover the parent .git and apply in repo-context (a silent no-op on
  # the staged files). `patch -p1` is repo-agnostic and writes the files directly.
  #
  # Crash-safe: stage + patch into a temp dir, then atomically rename into place.
  # srt-src therefore only ever exists as a complete, fully-patched tree — an
  # interrupted run leaves only the temp dir (wiped on the next run), so a partial
  # tree can never be silently reused.
  if [ ! -d "$SRT_SRC" ]; then
    STAGE="$BUILD/.srt-src.staging"
    rm -rf "$STAGE" "$SRT_SRC"; mkdir -p "$STAGE"
    git -C "$SRT" archive HEAD | tar -x -C "$STAGE"
    for p in "$SUB"/patches/*.patch; do
      [ -e "$p" ] || continue
      echo "applying $(basename "$p") to staged srt-src"
      ( cd "$STAGE" && patch -p1 -s < "$p" )
    done
    mv "$STAGE" "$SRT_SRC"
  fi

  if [ ! -f "$SRT_INSTALL/lib/libsrt.a" ]; then
    rm -rf "$SRT_BUILD" "$SRT_INSTALL"
    # CMAKE_WARN_DEPRECATED=OFF: pinned libsrt declares cmake_minimum_required
    # 3.5 — same unactionable deprecation banner as the mbedTLS build above.
    cmake -S "$SRT_SRC" -B "$SRT_BUILD" -DCMAKE_TOOLCHAIN_FILE="$SUB/arm-none-eabi.cmake" \
      -DCMAKE_INSTALL_PREFIX="$SRT_INSTALL" -DGNU=ON -DCMAKE_BUILD_TYPE=MinSizeRel \
      -DENABLE_APPS=OFF -DENABLE_SHARED=OFF -DENABLE_STATIC=ON \
      -DENABLE_UNITTESTS=OFF -DENABLE_TESTING=OFF -DENABLE_BONDING=OFF \
      -DENABLE_HEAVY_LOGGING=OFF -DENABLE_LOGGING=OFF $SRT_ENC_FLAGS \
      -DCMAKE_WARN_DEPRECATED=OFF \
      -DENABLE_STDCXX_SYNC=OFF -DENABLE_MONOTONIC_CLOCK=OFF -DENABLE_SOCK_CLOEXEC=OFF
    cmake --build "$SRT_BUILD" --target install -j"$(nproc)"
  fi
  printf '%s\n' "$NEWSTAMP" > "$STAMPFILE.tmp.$$" && mv "$STAMPFILE.tmp.$$" "$STAMPFILE"
  SRT_LIB=$(echo "$SRT_INSTALL"/lib*/libsrt.a)
  INC="$INC -I$SRT_INSTALL/include"
fi

rm -rf "$OBJ"; mkdir -p "$OBJ"
# Substrate C compiles plain (no shim env), exactly as the staged builds did.
# $DEFS (the per-target lwipopts/behavior knobs the staged builds baked into
# their config headers — e.g. -DLWIP_NETIF_LOOPBACK=1) MUST reach these C files
# too: lwipopts.h is read by the lwIP core here, not just by the C++ app, so a
# -D that only hit the app would leave lwIP compiled with the superset default.
for f in $C_SRC; do
  $CC $ARCH $OPT $INC ${DEFS:-} -std=gnu11 -c "$f" -o "$OBJ/$(basename "${f%.c}").o"
done

# App + the C++ glue. cxa_override is always linked (per-task eh globals); the
# libsrt shim env (posix-shims force-include + __GNU__ + pthread_key_shim) is
# added only for LIBSRT targets (they include <srt/srt.h>).
SHIM_DEFS="${DEFS:-}"
CXX_SRC="$APP $SUB/cxa_override.cpp"
if [ "${LIBSRT:-0}" = "1" ]; then
  INC="-I$SUB/posix-shims $INC"
  SHIM_DEFS="$SHIM_DEFS -include $SUB/posix-shims/shim_prefix.h -D__GNU__=1"
  [ "${ENCRYPT:-0}" = "1" ] && SHIM_DEFS="$SHIM_DEFS -DSRT_PASSPHRASE=\"freertos-srt-secret-1\""
  $CC $ARCH $OPT $INC $SHIM_DEFS -std=gnu11 -c "$SUB/pthread_key_shim.c" -o "$OBJ/pthread_key_shim.o"
fi
for f in $CXX_SRC; do
  $CXX $ARCH $OPT $INC $SHIM_DEFS -std=gnu++11 -fexceptions -c "$f" -o "$OBJ/$(basename "${f%.cpp}").o"
done

# --wrap=clock_gettime + --wrap=gettimeofday: their wrappers live in clock_shim.c
# + syscalls_stub.c (always compiled), so always safe. --wrap=setsockopt's
# wrapper is __wrap_setsockopt in net_shim.c, compiled only for the data-plane
# targets (NETIF != none); the non-netif targets must NOT wrap setsockopt —
# libsrt-smoke pulls setsockopt from libsrt.a and would hit an undefined
# __wrap_setsockopt otherwise (matching libsrt-smoke, which wrapped only clock_gettime).
WRAPS="-Wl,--wrap=clock_gettime -Wl,--wrap=gettimeofday"
[ "${NETIF:-none}" != "none" ] && WRAPS="$WRAPS -Wl,--wrap=setsockopt"
$CXX $ARCH $OPT --specs=rdimon.specs -T "$SUB/mps2_an386.ld" -Wl,--gc-sections \
  $WRAPS \
  -Wl,--start-group "$OBJ"/*.o $SRT_LIB $TSTC_LIB $MBED_LIBS -lstdc++ -Wl,--end-group \
  -o "$BUILD/firmware.elf"
arm-none-eabi-size "$BUILD/firmware.elf"
