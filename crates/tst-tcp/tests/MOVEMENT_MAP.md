# tst-tcp integration-test map

The tst-tcp test suite uses **eight standalone binaries** — one file per domain.
Unlike the crates that were reorganised into `#[path] mod` domain harnesses
(tst-rtp, tst-srt, …), tst-tcp's tests were small enough to stay as individual
top-level binaries; no test reorganisation has occurred. This file documents
what each binary covers.

## Current binaries

| Binary | Feature gates | What it covers |
|---|---|---|
| `net_loopback` | (default) | `TcpTransport` plain-TCP round-trip: listener→caller and caller→listener in both send and receive directions (4 combos). Smoke-tests the basic connection lifecycle. |
| `net_pipeline_round_trip` | (default) | Full pipeline shell: `MuxSender<TcpTransport>` → TCP loopback → `DemuxReceiver<TcpTransport>`. `ProgramMap` smoke: the demuxer recovers the program map. Also a cancel test for a managed TCP receive against a silent peer. |
| `net_partial_write` | (default) | A peer that stops reading mid-message stalls the send but never desyncs it — the sender keeps writing the remainder and the peer sees one contiguous stream once it resumes; a cancel bounds the stalled send with `ExplicitClose`. |
| `net_tls_hostname` | `tls` | TLS caller dials by hostname (`localhost`) and rustls verifies the `dnsName` SAN. Also confirms the negative: an IP-literal dial against a `dnsName`-only cert is rejected at the handshake, inside `connect`. Also handshake-timeout and explicit-close cases. |
| `net_tls_cancel` | `tls` | `tcps://` receive path: a cancel reaches a recv parked inside a TLS read, whole plaintext is still delivered one socket read at a time, and `close_notify` is a clean EOF. |
| `net_connect_timeout` | `tls`, Linux | A `tcps://` connect that runs out `connect_timeout` reports `ConnectTimeout`, like `tcp://`, against a listener with a full accept queue. |
| `net_conformance` | (default) | The tst-core transport conformance kit over plain `tcp://` loopback pairs, including the cancel rows and the peer-EOF-is-not-a-cancel row. |
| `deprecated_aliases` | (default) | The old names the crate still accepts compile and name the new types. |

## Equivalence note

History only; the table above is the current layout.

No tests have been added, dropped, or renamed as part of any reorganisation.
