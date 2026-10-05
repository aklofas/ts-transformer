# Example: SRT egress out a real NIC

The flagship `freertos-srt` target: a bare-metal SRT **caller** on a Cortex-M
(FreeRTOS + lwIP) streams the 564-byte video-roundtrip golden ×64 out a real
**lan9118** Ethernet controller, across QEMU's SLIRP user-net, to a **host**
`tst-srt` listener that reconstructs the stream and verifies it **byte-exact** —
unencrypted and with mbedTLS AES-128 + passphrase.

This is the egress off-device hop (the `tests/` are on-device
loopback). It proves the bytes actually leave the chip through a NIC driver and
arrive intact at an independent SRT receiver.

```
  firmware caller            QEMU SLIRP user-net           host listener
  (lan9118 @ 10.0.2.15) ───── 10.0.2.2 (gateway) ─────►  tst-srt on :9000
        main.cpp                                            host/src/main.rs
```

## Pieces

- `main.cpp` — the firmware: brings up the lan9118 netif, opens an SRT caller,
  applies `substrate/srt_opts.h`, and sends the golden ×64. It prints
  `s4_*_sent` after sending; it **cannot self-verify** (the bytes are gone), so
  the verdict is the host's.
- `host/` — a small, workspace-detached Rust package (`freertos-srt-host`) that opens a
  `tst-srt` listener, receives the stream, and prints `s4_host_plain` /
  `s4_host_aes` on a byte-exact match. It depends on `tst-srt` by relative path
  only, so it never shows up in the main workspace's metadata.

## Production crypto warning

> The AES-128 phase here is a **reference**, not a secure deployment. Its entropy
> comes from a deterministic fixed-seed LCG in `substrate/syscalls_stub.c`
> (`_getentropy` / `mbedtls_hardware_poll`) so the gate is reproducible in CI.
> **Production encrypted firmware must replace those hooks with a real
> hardware-RNG-backed entropy source** before trusting the encryption.

## Run it end-to-end

```bash
# from the workspace root — builds firmware + host, orchestrates both phases
bash embedded/scripts/check/freertos-srt.sh example
```

The gate starts the host listener, waits for its `host-ready` line, launches the
QEMU caller (`-nic user,model=lan9118`), then bounded-joins the host and asserts
its PASS token — for plain and AES-128 in turn.

## Notes that bit during bring-up

- **LIVE mode, not FILE.** `tst-srt`'s listener is a LIVE-streaming receiver
  with no FILE-mode knob, so the caller uses `SRTT_LIVE` (a FILE caller vs. a
  LIVE listener is rejected at handshake). Over the lossless SLIRP path LIVE
  delivers the golden byte-exact — no too-late-packet-drop fires without loss.
- **Drain before close.** LIVE `srt_close` does not linger, so the caller waits
  for its send buffer to drain (plus a short grace) before closing, or the tail
  of the stream is lost.
- **Receive buffer ≥ payload size.** The message API rejects a recv buffer
  smaller than the ~1456-byte payload (`"Incorrect use of Message API"`), even
  though each application message is only 564 bytes — the host receives into a
  full-size temp buffer and copies out.
- **Encryption passphrase** must match on both ends; the gate sets the host's to
  the same value the firmware is compiled with.
