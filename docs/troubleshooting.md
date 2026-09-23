# Troubleshooting

Common failure modes you'll hit when building or running this library, with diagnoses and fixes. If you're not finding your symptom here, check the per-module guide for the relevant area, or open an issue at https://github.com/aklofas/ts-transformer/issues.

## Build failures

**"could not find python3"**

mbedTLS's build system uses Python for code generation during the encrypted build path. The `mbedtls` cargo feature is on by default, so this trips first-time builders without Python on PATH.

Fix: `sudo apt-get install -y python3` on Debian/Ubuntu; on macOS Python 3 is preinstalled. If you don't need encryption, build with `--no-default-features` to skip the mbedTLS step entirely.

**"submodule X is empty" / "fatal: no submodule mapping"**

You cloned without submodules. `crates/srt-sys/vendor/srt`, `crates/mbedtls-src/vendor/mbedtls`, and `crates/rist-sys/vendor/librist` (if you're building the `rist` feature) are git submodules pinned to specific upstream tags, and the build script needs their contents.

Fix: from the repo root, run `git submodule update --init --recursive`.

**"could not find cmake"**

libsrt's build is CMake-based; the build script invokes it when the vendored fallback path is taken.

Fix: `sudo apt-get install cmake` on Debian/Ubuntu, `brew install cmake` on macOS.

**"could not find pkg-config"**

By default the build script tries `pkg-config srt` first to detect a system libsrt before falling back to the vendored build. The pkg-config probe itself needs the `pkg-config` binary on PATH.

Fix: install pkg-config, or set `SRT_FORCE_VENDORED=1` to skip the probe and go straight to the vendored compile.

**First build hangs at "Compiling tstrans-srt-sys"**

Not actually hung. libsrt and mbedTLS compile from source on a cold build, which takes 3-5 minutes on a typical workstation. Subsequent builds reuse the artifacts and finish in seconds.

Fix: wait it out. Run `cargo build -v` if you want to see what's actually executing.

**Linker error: undefined reference to libstdc++ symbols**

The cdylib needs C++ runtime linkage because libsrt is C++. For `cargo build -p tst-c` this is handled automatically.

Fix: if you're consuming `tstrans.h` from another build system, add `-lstdc++` (Linux) or `-lc++` (macOS) to your link line. The shipped `tstrans.pc` declares the correct `Libs.private`; using `pkg-config --static --libs tstrans` is the safest way to get the right flags.

## Connection failures

**Caller hangs on `connect()`**

Three usual suspects: the listener side isn't actually bound yet, a firewall is dropping UDP (SRT runs over UDP, not TCP), or the peer rejected the handshake but is taking a while to surface that.

Fix: confirm the listener is up with `ss -ulpn | grep <port>`; verify both sides agree on passphrase and key length; if you need a hard upper bound on `connect()`, set `send_timeout(Duration::from_secs(N))` on the `SocketBuilder` (libsrt uses the send timeout during the synchronous handshake path).

**`Listener::accept` blocks forever**

`accept()` is blocking by design and has no built-in deadline. Contrary to what you might expect, `ListenerBuilder::recv_timeout` / `Listener::set_recv_timeout` does *not* gate the accept call — libsrt's `srt_accept` ignores `SRTO_RCVTIMEO`. The recv timeout only applies to accepted sockets (it is inherited as their per-socket read deadline).

Fix: use `Listener::accept_timeout(Duration)` instead of `accept()`. It returns `Err(AcceptError::TimedOut)` when the duration elapses with no incoming connection, and `Ok((socket, peer))` on success:

```rust
use std::time::Duration;
use tst_srt::AcceptError;

loop {
    match listener.accept_timeout(Duration::from_secs(1)) {
        Ok((socket, peer)) => { /* handle */ }
        Err(AcceptError::TimedOut) => { /* check shutdown flag, retry */ }
        Err(AcceptError::ListenerClosed) => break,
        Err(e) => return Err(e.into()),
    }
}
```

Alternatively, run `accept()` on a dedicated thread and call `Listener::close` from your shutdown path — that wakes the blocked call with `AcceptError::ListenerClosed`.

**Connection establishes but no data arrives**

Call `socket.stats()` on both sides. If `bytes_sent` is increasing on the sender but `bytes_received` isn't moving on the listener, the link is up but packets are being dropped on the path. Most often this is an MTU / path-MTU issue.

Fix: confirm both sides are using SRT defaults (1316-byte payload), or set explicit `payload_size(...)` on both builders matching the actual path MTU minus the SRT/UDP/IP overhead.
(The receive side sizes its buffer to the SRT live-mode maximum, so a
peer with a larger `payload_size` no longer breaks the receiver — the
matching-sizes advice is about send-path MTU fit, not receiver limits.)

**`ConnectError::BadEncryption`**

Encryption configuration was rejected before the handshake even ran. Usually one side has a malformed passphrase (the `Passphrase::new` constructor enforces 10-79 ASCII-printable bytes, but raw FFI users can sometimes bypass that).

Fix: build the `Passphrase` through `Passphrase::new` and let the constructor validate.

**`ConnectError::Rejected { reason: RejectReason::BadSecret, .. }`**

Passphrase strings don't match between caller and listener. libsrt rejects with `SRT_REJ_BADSECRET` after the handshake confirms the keying material doesn't agree.

Fix: verify both sides pass byte-identical passphrase strings — mind shell quoting, trailing newlines from heredocs, and environment variables that include leading whitespace.

**`ConnectError::Rejected { reason: RejectReason::Unsecure, .. }` or `AcceptError::PeerRejected { reason: RejectReason::Unsecure, .. }`**

Caller and listener disagree on whether encryption is in use at all. Most common cause: one side built with `--no-default-features` (no `mbedtls` feature, encryption disabled) and the other built with the default feature set.

Fix: build both sides with the same feature configuration. If you need encryption on the link, neither side may be built `--no-default-features`.

**Sender hangs when dropping a `Socket`**

`Socket::Drop` blocks the calling thread for as long as `SocketConfig::linger` is set to. This is not libsrt's out-of-the-box behavior: its own default is `l_onoff=0` (linger off) — `srt_close` returns immediately and the queued backlog drains in the background. (The commonly-cited 180-second linger default belongs to libsrt's *file*-mode `DEF_LINGER_S`, which this library never reaches — every transport here is message/live mode.) A hang happens only if you (or a builder default) set a non-zero linger and the peer never ACKs the pending sends.

Fix: leave `linger` unset (`None`) for libsrt's immediate-return default, or set `SocketConfig::linger = Some(Duration::ZERO)` explicitly, or use the `SocketBuilder::linger(Duration)` setter to bound the wait. The `tst-c` connect path (`bindings/c/core/src/sender/connect.rs::connect_srt`) defaults to 5 seconds — long enough to drain a small backlog, short enough to never block reconnect noticeably.

## TCP / TLS (`tcps://`)

**`tcps://` fails with a certificate error**

TLS verifies the server certificate against exactly what you dialed:

- If you dialed a **hostname** (`tcps://relay.example.com:7001`), the
  certificate must carry a `dnsName` SubjectAltName for that hostname.
- If you dialed an **IP literal** (`tcps://192.168.1.10:7001`), the
  certificate must carry an `iPAddress` SubjectAltName for that address.

A common mistake is generating a cert with only an `iPAddress` SAN and then
dialing a hostname (or vice versa). The fix is to dial what the cert says, or
regenerate the cert to match what you want to dial.

Generate a cert for hostname dialing:
```bash
openssl req -x509 -nodes -newkey rsa:2048 -subj "/CN=relay.example.com" \
  -addext "subjectAltName=DNS:relay.example.com" -out server.crt -keyout server.key
```

Generate a cert for IP-literal dialing:
```bash
openssl req -x509 -nodes -newkey rsa:2048 -subj "/CN=server" \
  -addext "subjectAltName=IP:192.168.1.10" -out server.crt -keyout server.key
```

Reference this certificate with `?cert=server.crt&key=server.key` on the
listener URL, and add the CA to the caller's trust store (`?ca=ca.crt`) or to
the OS native trust store. Listener bind addresses still require IP literals
(`0.0.0.0` / `::`); the listener-vs-caller asymmetry is intentional.

## KLV decode rejection

**`decode_strict_compliance` rejects**

The record violates one of ST 0601.8-09 / -11 / -12's mandatory rules: Tag 2 (timestamp) must be the first element, Tag 1 (checksum) must be the last element, Tag 65 (UAS LS Version) must be present. The corresponding `KlvDecodeError` variants are `Tag2NotFirst`, `Tag1NotLast`, and `MissingTag65`.

Fix: walk the strictness ladder — fall back to `decode_strict` (validates UL family + checksum but not ordering) or plain `decode` (validates checksum only) to inspect the record despite non-compliance. If the producer is yours, fix the producer to emit the mandatory tags in the correct order. Worked example: [../examples/klv-metadata/klv_decode_file.rs](../examples/klv-metadata/klv_decode_file.rs).

**`decode` rejects with `KlvDecodeError::ChecksumMismatch`**

Either Tag 1 (checksum) wasn't emitted by the producer, or the bytes were corrupted in transit.

Fix: try `decode_unchecked` to get a parsed record without checksum validation. If `decode_unchecked` returns sensible values, transit corruption is the likely cause. If it returns nonsense values (out-of-range coordinates, truncated strings), the producer is broken — investigate that side instead.

**`decode_strict` rejects with `KlvDecodeError::UnexpectedUniversalLabel`**

The record's 16-byte universal label isn't in the ST 0601 family that `decode_strict` accepts.

Fix: use plain `decode` (no UL family check) if you're handling non-ST-0601 records, or validate the UL upstream before dispatching.

**`KlvDecodeError::DuplicateTag`**

The record contains the same tag twice in its top-level list. ST 0601 disallows duplicates within a single LS.

Fix: this is almost always a producer bug; fix the producer.

**`NonConformantIssue::MultiCellAu` events on a sync KLV PID**

A multi-cell AU reassembly attempt failed on the named PID. `reason` discriminates the failure mode:

- `Orphan` — a `Middle` or `Last` cell arrived without a prior `First`. Either the stream started mid-AU (e.g. seek into a recording) or a `First` cell was lost upstream. (Note: the producer-side CFI malformation pattern — encoders shipping `0b00` (Middle) on single-cell AUs — is rescued by the default-on `cfi_tolerance` knob and does NOT produce an Orphan event under default config. You will only see Orphan here for legitimate fragmentation losses or if you explicitly set `cfi_tolerance: false`.)
- `SequenceGap` — a buffered AU's continuation cell had the wrong `sequence_number`. A cell was lost between the buffered `First`/`Middle` and the arriving cell.
- `ConcurrentFirst` — a new `First` arrived while the previous AU was still buffering (its `Last` never appeared). The partial buffer is dropped before the new `First` is processed.
- `Overflow` — the accumulated inner-byte total would exceed `DemuxerConfig::au_cell_cap_per_pid` (default 1 MiB). Tune the cap via `DemuxerConfigBuilder::au_cell_cap_per_pid(bytes)`.

Fix: for `SequenceGap` and `Overflow`, investigate the upstream sender. If `ts-transformer`'s muxer is the sender, this is automatic — `Muxer::push_klv*` always emits `Complete` cells. Legitimate multi-cell streams reassemble transparently into a single `MetadataKind::KlvSyncAuCell` event with `was_reassembled = true` and `cell_count = N`.

**I see `MultiCellAu{Orphan}` events but zero typed KLV from a malformed encoder**

This shouldn't happen under default configuration — the producer-side CFI malformation (encoders shipping `0b00` (Middle) on what are actually single complete KLV records) is rescued by the default-on `cfi_tolerance` knob. If you are seeing `Orphan` events with zero KLV, check whether you have explicitly opted into strict mode:

```rust,ignore
Demuxer::with_config(DemuxerConfig::builder().cfi_tolerance(false).build())  // strict — disables the rescue
```

To restore tolerance, either remove the `.cfi_tolerance(false)` call or set it back to `true`. The demuxer then payload-validates the orphan cell as one complete KLV unit (SMPTE 336M UL prefix + BER length match) and, if it passes, emits the cell as `KlvSyncAuCell{Complete}` plus a `NonConformantIssue::CfiTolerated { pid, observed_cfi, treated_as }` diagnostic so the malformation remains visible to telemetry. See [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md#malformed-cell_fragment_indication-tolerance-default-on) for the full contract.

**I'm running a conformance suite and want spec-strict CFI handling**

Set `cfi_tolerance: false` on the `DemuxerConfig`. Orphan Middle/Last cells then surface as `NonConformantIssue::MultiCellAu { reason: MultiCellAuReason::Orphan }` per H.222.0 V9 §2.12.4.2 Table 2-157 with no metadata event.

## TS framing issues

**`Sender` in `TsFramingMode::Strict` errors on the first push**

Strict mode requires the input bytes to start with a TS sync byte (`0x47`) at offset 0 with the standard 188-byte cadence. If your upstream producer emits a partial packet at the boundary or has any byte-level offset, strict mode rejects rather than realigning.

Fix: switch to `TsFramingMode::Recover` to auto-resync, or fix the producer to emit aligned bytes from the start. See [guides/pipeline.md](/docs/guides/pipeline.md) for the framing state machine details.

**`Sender` in `TsFramingMode::Recover` (the default) returns `TsFramingError::NoSyncAfterLimit`**

RECOVER mode scanned more than `SenderConfig::max_unsynced_bytes` bytes (default 18,800, ≈100 packets' worth) without finding a TS sync byte — the input doesn't look like a TS stream at all, not just misaligned. The count accumulates across `send_ts` calls until sync is acquired OR the watchdog fires, resetting to zero either way — so it can fire mid-stream on a producer that goes silent or starts emitting non-TS bytes, and a persistently non-TS source trips it again every `max_unsynced_bytes`, not just once at startup.

Fix: check the upstream producer is actually emitting MPEG-TS (188-byte-aligned packets starting with `0x47`) on this connection. If the threshold is simply too tight for a legitimately noisy startup, raise `SenderConfig::max_unsynced_bytes`, or set it to `usize::MAX` to disable the watchdog. `send_ts`'s `input_consumed` on this error is `Some(true)` — do not resend the same bytes (the garbage is already in the framing scan buffer; resending duplicates it and re-trips the watchdog sooner). `flush()` does **not** help recover here: it only emits a buffered bundle once sync has been acquired, and `NoSyncAfterLimit` only fires while still scanning for sync, so there is nothing queued for it to drain. If the source keeps emitting non-TS bytes, pushing more of them just fires the watchdog again every `max_unsynced_bytes` — the actual fix is correcting or disconnecting the upstream producer, not retrying.

**Receiver gets garbled TS**

After the run, check `Sender::stats()` and inspect `bytes_skipped_for_sync` and `resync_events`. If either is nonzero in production, the producer is emitting non-aligned bytes intermittently. In `Recover` mode the sender still emits a clean stream (it realigns silently), so the receiver should be fine; in `Strict` mode you'd have already errored.

Fix: if the receiver is still seeing garble despite zero stats, the corruption is happening downstream of the sender — check the network path and any intermediate transcoders.

**`Receiver` / `DemuxReceiver` emits nothing on a very short stream, or the last few packets after a corruption never arrive**

The receive-side syncer locks only after it has seen four aligned TS packets (four `0x47` bytes at 188-byte strides — 752 bytes) and emits nothing before that. The confirmation is peek-only, so on a healthy stream nothing is lost. But a stream shorter than four packets never emits, and after any sync loss the last one to three packets before end-of-stream stay buffered awaiting confirmation and are dropped when the transport closes — on `DemuxReceiver` that can be the final access unit. `ReceiverStats::resync_events` counts each lock (initial and re-locks); `bytes_skipped_for_sync` counts HUNT and failed-VERIFY drops combined.

Fix: feed at least four packets per session (test fixtures included) and treat the final packets after a mid-stream corruption as best-effort. If you need every packet of a finite capture, feed the bytes to `tst_core::mpegts::demux::Demuxer::feed` directly — it accepts an initial `0x47` with no confirmation and only demands a 5-of-7 stride check after a sync loss.

**Receiver sees double-wrapped KLV (legacy callers from older library versions)**

If you previously passed pre-wrapped bytes to `Muxer::push_klv` for a `KlvStreamType::SynchronousMetadata` stream (older library versions where the caller had to wrap), the muxer now double-wraps. Strip the outer wrapper and let the muxer wrap once.

Fix: pass raw KLV LS bytes (16-byte SMPTE UL + BER length + body) to `Muxer::push_klv` / `MuxSender::send_klv`; the muxer auto-prepends a 5-byte `Metadata_AU_cell` header per ITU-T H.222.0 V9 § 2.12.4.2 (Tables 2-155+2-156). PTS lives in the PES header (§ 2.12.4.1). Asynchronous KLV streams (`KlvStreamType::PrivateData`) pass the raw 0601 LS bytes through unchanged. See [guides/mpegts-mux.md](/docs/guides/mpegts-mux.md) for the synchronous vs. asynchronous distinction.

**`MuxError::KlvTooLarge`**

Your KLV blob exceeds the PES_packet_length ceiling (65532 bytes without PTS, 65527 with PTS). ST 0601 packs are typically <2 KB so this is a sanity check, not a normal failure mode.

Fix: investigate why your producer emitted a multi-KB metadata blob; this is almost always a bug.

## Reconnect loops

**`ManagedTransport` keeps reconnecting fast in a tight loop**

Backoff is set to `BackoffStrategy::Constant(Duration::ZERO)`, or `Exponential` with a too-low base.

Fix: use the default `BackoffStrategy::Exponential { base: Duration::from_millis(100), max: Duration::from_secs(10) }` (this is what `ReconnectPolicy::default()` returns), or tune the base up if your transport factory is itself expensive.

**Reconnect appears to succeed but no data flows after**

The gap buffer overflowed during the disconnect window. With the default `OverflowPolicy::DropOldest` newer messages displace older ones; with `OverflowPolicy::Reject` new sends fail outright. Either way, some messages were lost between the break and the reconnect.

Fix: size `gap_buffer_capacity` to your worst-case disconnect window times your send rate. The default of 256 messages is fine for a 1 Hz KLV stream over a 4-minute outage; for higher-rate video you'll want to budget more aggressively. See [guides/pipeline.md](/docs/guides/pipeline.md) for the sizing math.

**`max_attempts` exhausted; a send returns `TransportError::Broken`**

The policy's retry budget for this outage is spent. On the send side (`ManagedTransport`) this is not a dead end: the wrapper doesn't latch closed, so the very next `send_bytes` call starts a fresh reconnect cycle (with a fresh attempt budget) against the still-queued backlog. Under `ReconnectMode::Background`, the give-up itself surfaces exactly once — as a `Broken` on the *following* `send_bytes` call after the worker quits, and that call's own bytes are not queued. (The receive side behaves differently: `ManagedRecvTransport` has no gap buffer to retry against, so give-up latches the decorator closed and every subsequent `recv_bytes` returns `TransportError::Closed`.)

Fix: increase `max_attempts`, or set it to `None` to retry forever — only safe if your transport factory is itself rate-limited, otherwise a permanent peer outage produces a hot reconnect loop. The default is `Some(10)` which gives roughly 10 attempts with exponential backoff, on the order of a few minutes of real time before giving up.

**Sends succeed but nothing arrives (Background mode)**

Under `ReconnectMode::Background`, `send_bytes` returns `Ok(())` the instant the worker accepts bytes into the gap buffer — that is not confirmation the sink received them. If the sink stays down long enough for the outage to outlast `gap_buffer_capacity`, the default `OverflowPolicy::DropOldest` silently evicts queued messages to make room for new ones.

Fix: poll `ManagedTransport::stats_handle()`. `reconnecting: true` means a worker is actively trying to reconnect right now; a rising `gap_messages_dropped` / `gap_bytes_dropped` confirms bytes are being lost to eviction, not just delayed (this also counts a queued message that no longer fits the rebuilt transport's `max_payload` — `Blocking` mode would instead surface that one as `TooLarge` synchronously). If `max_attempts` is finite and the worker gave up, the next `send_bytes` call after that surfaces a one-shot `TransportError::Broken` (see the entry above) — that's the signal the outage outlasted the retry budget, not a normal `Ok`. The message text tells you which kind of give-up it was: `"reconnect gave up after N attempts"` means the retry budget ran out normally; `"background reconnect aborted (worker terminated abnormally)"` means the worker itself panicked or hit an unrecoverable poisoned lock. With `OverflowPolicy::Reject`, once the worker gives up and the retained backlog is already at capacity, subsequent sends return `TransportError::Backpressure` from the full buffer instead of restarting a reconnect cycle — deliberate parity with `Blocking` mode's refuse-before-reconnect behavior (see the `max_attempts` entry above) — and since `ManagedTransport` exposes no manual drain, the only way out of that stuck state is constructing a fresh wrapper with an empty gap buffer.

## Build-script behaviors

**Want to use a system libsrt instead of the vendored copy**

Leave `SRT_FORCE_VENDORED` unset (the default) and ensure `pkg-config srt --modversion` returns 1.5.0 or newer. The build script probes pkg-config first and uses the system copy when available. If the probe fails or the version is too old, it transparently falls back to the vendored build.

**Want to force the vendored build for reproducibility**

Set `SRT_FORCE_VENDORED=1` (equivalent: `SRT_NO_PKG_CONFIG=1`). This skips pkg-config entirely and always compiles `crates/srt-sys/vendor/srt` from source. Use this in CI and release builds where you want bit-for-bit reproducibility independent of whatever libsrt is installed on the build host.

**Want to skip the encryption build to iterate faster**

Run `cargo build --no-default-features`. This disables the `mbedtls` feature, drops the mbedTLS submodule from the build, and compiles libsrt with `ENABLE_ENCRYPTION=OFF`. Cold builds become roughly 1-2 minutes faster. Both peers must be built the same way — see [Connection failures](#connection-failures) above for the symptom when they disagree.

## Performance and reliability

**High `pktRcvLossTotal` on a stable network**

SRT reports loss / retransmits on a network you know is healthy. Cause: kernel UDP socket buffer overflow, common above ~25 Mbps. The kernel drops UDP packets before SRT can drain them; SRT sees the gaps as transmission losses and triggers ARQ retransmits.

Diagnosis: `cat /proc/net/udp` (or `ss -unp`) — non-zero `drops` column on the SRT port confirms.

Fix: set `SocketConfig::udp_recv_buffer_bytes = Some(12_500_000)` (or higher) for the receiver. For 100 ms RTT @ 25 Mbps, ~12.5 MB is the recommended floor. Linux clamps to `net.core.rmem_max` — raise with `sysctl -w net.core.rmem_max=33554432` if needed.

## All `UnpairedVideo`, zero `Paired`

**Symptom:** Using `tst_pipeline::Pairer::with_config` with `PairerMode::Realtime`,
the stats report `paired = 0` and `unpaired_video` matches your video event count.
KLV events are present (PMT shows the stream, demux events arrive).

**Most likely cause:** the encoder interleaves the KLV PES *after* its
matching video PES on the wire. Realtime mode's past-only history search
sees no KLV when the video event arrives, so every video pairs as
`UnpairedVideo`. The KLV then arrives, ingests into history, and never
finds a video that needs it (Realtime doesn't look back at past videos).

**Fix:** switch to `PairerMode::Buffered { max_lag: Duration::from_secs(2) }`
and bump `max_buffered_video` to ≈60 (≈2 s @ 30 fps). Buffered mode holds
video briefly to look ahead for KLV; the trade-off is up to
`max_buffered_video` × frame-period of pairing-induced latency.

```rust,ignore
use std::time::Duration;
let mut opts = PairerConfig::default();
opts.mode = PairerMode::Buffered { max_lag: Duration::from_secs(2) };
opts.tolerance = Duration::from_millis(300);
opts.max_buffered_klv = 32;
opts.max_buffered_video = 60; // ≈2 s @ 30 fps
let pairer = Pairer::with_config(video_pid, klv_pid, opts);
```

If `paired` is still zero after switching, the cause is not interleave
order — check the PIDs, tolerance, and `MetadataKind` distribution
(`KlvSyncAuCell` vs `KlvAsync` are both treated as KLV candidates, so
filtering by kind is not the issue).

## UDP / RIST receive cancellation

**A UDP or RIST `recv` blocks forever and cannot be stopped from another thread (Rust)**

**Symptom:** a thread parked in `UdpRecvTransport::recv_bytes` (or the
Python equivalent) does not return when another thread tries to shut it
down.

**Diagnosis:** you are on an older release — the handles land in 0.7.0,
which is still in development — or you never obtained the cancel handle.
From 0.7.0 every transport has one —
`UdpTransport::cancel_handle()` / `UdpRecvTransport::cancel_handle()`
return a `UdpCancelHandle`, and the RIST pair a `RistCancelHandle`
(`SrtCancelHandle` / `RtpCancelHandle` / `TcpCancelHandle` are the
siblings). The handle must be obtained BEFORE the transport is moved into
the parked thread; a `recv_bytes` already parked then returns
`TransportError::ExplicitClose` at its next ~100 ms poll tick. Before
0.7.0 there was no handle and the only shutdown was cooperative: a finite
per-call timeout plus a stop flag checked between calls (`recv_timeout`
on `UdpRecvTransport`; the 100 ms `Backpressure` poll on
`RistRecvTransport`). That shape still works and Python still uses it
internally, which is why `tstrans.udp.RecvTransport.close()` and its
rist twin have always ended a parked `recv()` from another thread with
`UdpError(CLOSED)` / `RistError(CLOSED)` — they now fire the real handle
first.

**Fix (Rust, 0.7.0 onward):** obtain `cancel_handle()` before moving the
transport into its thread, then fire `cancel()` from anywhere; the parked
`recv_bytes` returns `ExplicitClose`. **Fix (Python):** call
`cancel_handle().cancel()` (or `close()`) from the stopping thread.
**Fix (older releases, and still valid on any):** use `recv_timeout`
(UDP) or catch
`Backpressure` (RIST) for a bounded per-call deadline and check a stop
flag in the caller loop — the owning thread calls `close()` once it
decides to stop, between `recv` calls:

```rust,ignore
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

let stop = Arc::new(AtomicBool::new(false));
let stop_for_signal = Arc::clone(&stop);

// Signal thread: set the flag. The recv loop notices on the next
// timeout tick (within `timeout_ms`). No reference to the transport
// is needed here — cooperative stop via an AtomicBool.
std::thread::spawn(move || {
    std::thread::sleep(std::time::Duration::from_secs(30));
    stop_for_signal.store(true, Ordering::Release);
});

// Recv loop on the owning thread — the ONLY thread that calls recv or close.
let mut buf = vec![0u8; 65535];
loop {
    if stop.load(Ordering::Acquire) {
        recv_transport.close(); // safe: called by the owning thread
        break;
    }
    match recv_transport.recv_timeout(&mut buf, std::time::Duration::from_millis(200)) {
        Ok(Some(n)) => { /* process buf[..n] */ }
        Ok(None) => continue,  // timeout tick — loop back and check stop
        Err(e) => return Err(e.into()),
    }
}
```

If you need to interrupt the recv from a thread that does not own the
transport, consider switching to SRT, RTP, or TCP — all three expose a
cloneable cancel handle that is safe to store and fire from any context.
See [srt-cancel-handle.md](/docs/reference/srt-cancel-handle.md) for the
cancel-handle pattern.

## Why did my RTSP stream end?

**`recv_au()` returned `Ok(None)`, a receiver shell (`DemuxReceiver` / `Receiver` / `RawReceiver`) returned `Ok(None)` / `EndOfStream`, or any of them surfaced an error, and it's not obvious why**

`RtpRecvTransport` and `H264Receiver` built from an RTSP session
(`RtspSession::into_recv_transport` / `into_h264_receiver`) record a
structured `StreamEndReason` the moment the interleaved pump or the
keepalive thread observes the session ending, distinguishing a handful
of cases that otherwise all look like the same disconnect from the
caller's side: a clean
server-initiated TEARDOWN (`CleanTeardown`), a keepalive `454 Session
Not Found` (`SessionExpired`), a keepalive ping that failed to encode
or write (`KeepaliveFailed`), a hard TCP read error (`TransportFailed`),
a malformed or flooding peer (`ProtocolError`), or the caller's own
`close()` / cancel-handle fire (`Cancelled`).

Fix: call `end_reason()` on the transport or receiver after a recv
returns `None`/`EndOfStream` or a terminal error, and match on the
result:

```rust,ignore
match transport.end_reason() {
    Some(StreamEndReason::CleanTeardown) => { /* expected — server hung up */ }
    Some(StreamEndReason::SessionExpired) => { /* re-`describe()` + `setup()` */ }
    Some(StreamEndReason::KeepaliveFailed { msg }) => eprintln!("keepalive died: {msg}"),
    Some(StreamEndReason::TransportFailed { msg }) => eprintln!("wire broke: {msg}"),
    Some(StreamEndReason::ProtocolError { msg }) => eprintln!("peer misbehaved: {msg}"),
    Some(StreamEndReason::Cancelled) => { /* we asked for this */ }
    None => { /* session hasn't ended, or ended through an uninstrumented path */ }
}
```

`end_reason_handle()` returns a cloneable, `Send + Sync` handle for
polling the same recorded value from a watchdog thread that doesn't
own the transport (mirrors the `cancel_handle()` pattern above). `None`
means either the session hasn't ended yet, or it ended through a path
this arc doesn't instrument — most notably a plain `rtp://` transport
with no owning `RtspClient`, which only records `Cancelled` (via its
own `close()` / cancel-handle) and nothing else.

If you're wrapping the transport in `DemuxReceiver` / `Receiver` /
`RawReceiver`, obtain `end_reason_handle()` **before** moving the
transport into the shell — the transport itself is unreachable once
the shell owns it, same obtain-before-move rule as `cancel_handle()`:

```rust,ignore
let handle = transport.end_reason_handle();
let mut demux = DemuxReceiver::new(transport);
// ... later, once recv_event() returns Ok(None) or an error ...
match handle.get() { /* same match as above */ }
```

Keep the owning `RtspClient` alive, or drain the receiver, until
end-of-stream is observed: dropping the client races the
classification, since its `Drop` cancels the pump, which may record
`Cancelled` before the server's EOF is read — that race reproduces the
old `TransportBroken` shape.

**Python** (`tstrans.rtp`) mirrors this: `Receiver`, `DemuxReceiver`,
and `H264Receiver` each expose `end_reason()` (a `StreamEndReason`
member, or `None`) and `end_detail()` (the free-text message for
`KEEPALIVE_FAILED` / `TRANSPORT_FAILED` / `PROTOCOL_ERROR`, `None`
otherwise) — both stay readable after `close()`:

```python
from tstrans.rtp import RtspClient, RtspClientConfig, StreamEndReason

with RtspClient.connect_h264(RtspClientConfig("rtsp://cam.local/h264")) as session:
    with session.into_h264_receiver() as rx:
        for au in rx:
            ...
        # loop exited (EOS or a caught error) — ask why
        reason = rx.end_reason()
        if reason == StreamEndReason.SESSION_EXPIRED:
            pass  # re-DESCRIBE + SETUP
        elif reason in (StreamEndReason.KEEPALIVE_FAILED, StreamEndReason.TRANSPORT_FAILED):
            print(f"stream died: {rx.end_detail()}")
```

Set `TSTRANS_LOG=tst_rtp=debug` (`EnvFilter` syntax, same as
`RUST_LOG`) before `import tstrans` to also see the underlying
pump/keepalive `tracing` events on stderr — the same signal
`end_reason()` distills into a single enum member.

**JVM** (`org.tstrans.rtp`) mirrors this too: `Receiver`,
`DemuxReceiver`, and `H264Receiver` each expose `endReason()` (a
`StreamEndReason` member, or `null`) and `endDetail()` (the free-text
message for `KEEPALIVE_FAILED` / `TRANSPORT_FAILED` /
`PROTOCOL_ERROR`, `null` otherwise) — both stay readable after
`close()`:

```java
import org.tstrans.rtp.*;

try (RtspSession session = RtspClient.connectH264(cfg);
     H264Receiver rx = session.intoH264Receiver()) {
    for (var au : rx) {
        // ...
    }
    // loop exited (EOS or a caught error) — ask why
    StreamEndReason reason = rx.endReason();
    if (reason == StreamEndReason.SESSION_EXPIRED) {
        // re-DESCRIBE + SETUP
    } else if (reason == StreamEndReason.KEEPALIVE_FAILED || reason == StreamEndReason.TRANSPORT_FAILED) {
        System.out.println("stream died: " + rx.endDetail());
    }
}
```

Set `TSTRANS_LOG=tst_rtp=debug` (`EnvFilter` syntax, same as
`RUST_LOG`) in the process environment before the JVM loads
`libtstjni` (the bridge installs from `JNI_OnLoad`) to also see the
underlying pump/keepalive `tracing` events on stderr.

See the "Python/JVM `tracing` diagnostics bridge + structured
stream-end reason" entry in
[deferred-features.md](/docs/project/deferred-features.md) for the
bindings-parity history.
