# tst-tcp integration-test map

The tst-tcp test suite uses **four standalone binaries** — one file per domain.
Unlike the crates that were reorganised into `#[path] mod` domain harnesses
(tst-rtp, tst-srt, …), tst-tcp's tests were small enough to stay as individual
top-level binaries; no test reorganisation has occurred. This file documents
what each binary covers.

## Test binaries

| Binary | Feature gates | What it covers |
|---|---|---|
| `loopback` | (default) | `TcpTransport` plain-TCP round-trip: listener→caller and caller→listener in both send and receive directions (4 combos). Smoke-tests the basic connection lifecycle. |
| `pipeline_round_trip` | (default) | Full pipeline shell: `MuxSender<TcpTransport>` → TCP loopback → `DemuxReceiver<TcpTransport>`. Verifies KLV + H.264 demux events arrive intact. |
| `partial_write` | (default) | CORR-22 / Q6: a peer that stops reading mid-message stalls the send but never desyncs it — the sender keeps writing the remainder and the peer sees one contiguous stream once it resumes; a cancel bounds the stalled send with `Closed`. |
| `tls_hostname` | `tls` | DA-NET-9: TLS caller dials by hostname (`localhost`) and rustls verifies the `dnsName` SAN. Also confirms the negative: an IP-literal dial against a `dnsName`-only cert fails at first I/O. |

## Equivalence note

No tests have been added, dropped, or renamed as part of any reorganisation.
