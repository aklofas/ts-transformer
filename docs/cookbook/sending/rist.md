# Send MPEG-TS over RIST

> **When to use this:** ARQ-recovered UDP for lossy WAN links — VideoLAN librist with Simple + Main profiles and AES-128/192/256 PSK encryption.

> **Related:**
> - [Receive MPEG-TS over RIST](/docs/cookbook/receiving/rist.md)
> - [Pipeline guide](/docs/guides/pipeline.md)
> - [Send MPEG-TS over UDP](/docs/cookbook/sending/udp.md) — lower-level alternative without recovery
> - [SRT guide](/docs/guides/srt.md) — heavier alternative with a bigger feature set

RIST (Reliable Internet Stream Transport, VSF TR-06-1 / TR-06-2) adds ARQ
retransmission to UDP — useful for moderate-loss links (cellular, congested
WiFi) where SRT's full handshake-and-encryption shape is too heavy.
ffmpeg's `librist` muxer/demuxer, `ristsender`, and `ristreceiver` are the
common counterparts.

## Code (Simple Profile, unencrypted)

```rust
use tst_core::mpegts::mux::MuxerConfig;
use tst_pipeline::MuxSender;
use tst_rist::{RistProfile, RistTransportBuilder};
use std::time::Duration;

let tx = RistTransportBuilder::new("rist://10.0.0.5:9000")?
    .profile(RistProfile::Simple)
    .buffer(Duration::from_millis(200))
    .connect()?;
let cfg = MuxerConfig::builder()
    // ... configure programs/streams as needed
    .build()?;
let mut sender = MuxSender::new(tx, cfg)?;
sender.send_video(&nal_bytes, pts, key_frame)?;
sender.send_klv(&klv_bytes, pts)?;
```

**Important:** Simple Profile receivers REQUIRE an EVEN port (librist uses
`port` for RTP data and `port+1` for RTCP). Sender side has no parity
requirement.

## Main Profile + AES-256 encryption

```rust
use tst_rist::{EncryptionKey, RistProfile, RistTransportBuilder};

let tx = RistTransportBuilder::new("rist://video.example.com:9000")?
    .profile(RistProfile::Main)
    .encryption(EncryptionKey::aes256("shared-psk-here"))
    .cname("uav-12")
    .connect()?;
```

Calling `.encryption(...)` automatically promotes profile to Main; Simple
doesn't carry encryption.

## URL parameters

| Param                    | Meaning                                              |
| ------------------------ | ---------------------------------------------------- |
| `profile=simple\|main`   | RIST profile override                                |
| `bandwidth=N`            | kbps target throughput cap                           |
| `buffer=N`               | Recovery buffer in milliseconds                      |
| `aes-type=128\|192\|256` | AES key size; needs `secret` too (else rejected)     |
| `secret=...`             | AES PSK (URL-encoded); alone selects AES-256         |
| `cname=...`              | RTCP CNAME identifier                                |
| `recovery_maxbitrate=N`  | Retransmit bandwidth cap (kbps)                      |
| `session_timeout=N`      | Receiver session timeout (ms)                        |
| `compression=1`          | Enable NULL-packet deletion                          |

Either encryption key forces the Main profile. `secret` on its own is
enough — it selects AES-256, librist's own default. `aes-type` on its own
is rejected at parse: naming a cipher without a PSK used to be accepted
and then silently configure a **plaintext** link.

## Verify with ffmpeg

```bash
ffmpeg -i 'rist://@:9000?profile=simple&buffer=200' -c copy out.ts
```

For Main Profile + AES:

```bash
ffmpeg -i 'rist://@:9000?profile=main&aes-type=256&secret=shared-psk-here' \
    -c copy out.ts
```
