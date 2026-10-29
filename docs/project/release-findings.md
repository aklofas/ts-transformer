# Release findings ledger

One row per defect that escaped the release qualification: found during a
release-candidate cycle, or within a month of a release. Each row names the
class of test that would have caught it and the test that now covers that
class, so the question this page answers is not only whether a defect was
fixed but whether the suite would catch the next defect of the same kind.

A finding is a defect in the library, its packaging, or the harness that
judges it. Flaky CI failures are not findings; they live in the
[CI flake ledger](/docs/project/ci-flakes.md) until their fix lands.

## Test classes

Each finding belongs to one of these classes. The list says where each class
is exercised today.

1. **Standards and timing conformance.** Bytes and clocks on the wire held to
   the specification by an analyzer that is not ours: the interop matrix's
   wire oracles in `crates/tst-interop/src/oracles.rs` and its
   `analyze/tsanalyze/*` cells, which gate TSDuck's per-PID PCR and PTS leap
   counters.
2. **Hostile network lifecycle.** A peer that closes mid-handshake, resets,
   stalls, or holds the port a socket needs: the `net_*` integration tests of
   each transport crate and of the C binding.
3. **Resource exhaustion.** A server held to its connection cap and its
   header, handshake and write deadlines, with a slow but well-behaved client
   that must survive the same bounds: the servers' admission tests.
4. **Packaged-consumer integration.** The shipped artefact used the way a
   consumer uses it: the JVM jar on the module path, the C package with its
   default features, the wheels in a fresh environment.
5. **Cross-binding behavioural parity.** One input judged identically through
   Rust, C, Python and the JVM: the URL corpus in `crates/tst-integration`
   and its C twin, and the kind-equivalence rails.
6. **Non-browser WebRTC interoperability.** Third-party WHEP clients beside
   the browsers. Today only browsers run in the interop matrix, so this class
   is covered by the server's own protocol tests until a non-browser client
   cell exists.

A harness finding (an oracle that judged a correct run wrong) has the class
**harness oracle**: the evidence tooling's own tests cover it.

## Severity

- **high**: the definition the `severity-high` label uses in the
  [1.0 exit criteria](/docs/project/1.0-exit-criteria.md): wrong bytes on the
  wire or in a decoded record, data lost without a counter or an error saying
  so, a crash or hang in library code, or memory unsafety.
- **medium**: a feature that fails loudly or is unusable in one
  configuration, with no wrong output and no lost data.
- **low**: no effect on library output; a harness or tooling defect.

## Findings

| Finding | Subsystem | Severity | Found by | Why the suite missed it | Class now covered by | Fixed in |
|---|---|---|---|---|---|---|
| The muxer wrote a PCR equal to the PTS of the access unit it preceded, so decoders got no decode lead, and the PCR could step back after a PCR sent with KLV, audio or data | MPEG-TS muxer | high | the pre-release code review | every player tested plays the stream; the matrix gated TSDuck's global counters only, and no wire oracle read the PCR against the DTS | standards and timing conformance: the `analyze/tsanalyze/*` cells' per-PID leap gate, the `pcr_lead` wire oracle, and the muxer tests `pcr_leads_explicit_dts_by_pcr_lead` and `keyframe_pcr_never_steps_back_after_klv_push` | 0.9.1 |
| An `rtsps://` client spun at full CPU, with no timeout, when the server closed the TCP connection during the TLS handshake | RTSP client | high | the pre-release code review | no test had the server close the connection during the handshake | hostile network lifecycle: `rtsps_trickled_handshake_hits_the_connect_timeout` in tst-rtp and `rtsps_connect_to_a_peer_closing_mid_handshake_is_rtsp_tls` in the C binding | 0.9.1 |
| A RIST receiver with `?profile=simple` crashed the process when its RTCP port (port + 1) was already in use | RIST | high | the pre-release code review | no test held the RTCP port before the receiver bound | hostile network lifecycle: `simple_profile_rtcp_port_collision_is_an_error_not_a_crash` in tst-rist | 0.9.1 |
| The HLS server had no connection limit and no request-header or TLS handshake deadline | HLS server | medium | the pre-release code review | the HTTP tests checked routing, authentication and shutdown, never many or silent clients | resource exhaustion: `connections_over_the_cap_are_closed_and_a_slot_frees_on_timeout`, `a_silent_connection_is_closed_after_the_header_timeout` and `a_slow_reader_within_the_deadline_is_served` in tst-hls | 0.9.1 |
| The C RTSP client builder dropped the URL's credentials and options (userinfo, `?transport=`, `?rtsp_version=`, `?tcp_keepalive=`, `?recv_timeout=`) | C binding, RTSP | medium | the pre-release code review | no test ran the URL corpus's RTSP rows through the C builder | cross-binding parity: `url_corpus_rtsp_rows_match_through_c` in the C binding against the shared corpus that `url_corpus_covers_every_documented_key` keeps complete | 0.9.1 |
| A C RTSP session converted to a demux receiver stopped after the media already queued | C binding, RTSP | high | the pre-release code review | no C test sent media after the conversion | cross-binding parity and lifecycle: `bridged_tcp_receiver_gets_media_sent_after_the_bridge` and `bridged_udp_receiver_gets_media_sent_after_the_bridge` in the C binding | 0.9.1 |
| JVM `module-info` did not export `org.tstrans.shells`, `org.tstrans.hls` and `org.tstrans.webrtc`, so module-path users could not use them | JVM binding | medium | the pre-release code review | every JVM test ran on the classpath, where exports are not enforced | packaged-consumer integration: the `modulePathSmoke` Gradle task, a named module run against the assembled jar on the module path as part of `check` | 0.9.1 |
| The C package refused every encrypted `rist://` URL with `TST_E_RIST_ENCRYPTION_DISABLED`; it failed closed | C binding, RIST | medium | the pre-release code review | no test opened an encrypted RIST URL through the C binding built as the package ships it | packaged-consumer integration: `rist_recv_open_aes256_opens_when_mbedtls_is_on` in the C binding, with its default `mbedtls` feature | 0.9.1 |
| The WHEP server advertised PATCH without sending an `ETag`, so mediamtx's WHEP client took the trickle-ICE path and rejected the answer | WebRTC (WHEP) | medium | interop with mediamtx's WHEP client | the matrix's WHEP cells run browsers and the reference player only, which never take that path | non-browser WebRTC interoperability: `preflight_patch_and_unknown_mount` in tst-webrtc pins the advertised methods and the 405 for PATCH; a non-browser client cell in the matrix is still open | 0.9.1 |
| The AV1 carriage oracle did not excuse PES-start damage the corruption tap had injected and attributed, so a correct RIST leg failed | soak harness | low | the 0.9.0 release soak | the ADTS oracle had the excusal and the AV1 oracle did not; no test fed the AV1 oracle attributed damage | harness oracle: `av1_carriage_excuses_attributed_prefix_damage` in tst-interop | 0.9.1 |
| An external publisher that failed to send its closing message after end of stream was counted as a failed generation, though its media was delivered | soak harness | low | the 0.9.0 release soak | the publisher loop judged only gst-launch's exit code | harness oracle: the `interop-publish-generation.sh` rail, which drives the publisher generation with a stand-in for gst-launch | 0.9.1 |
| One KLV placement excursion of 2.3 ms over a sender-report interval failed the elementary publish leg, though the library applied the sender's own time mapping | soak harness | low | the 0.9.0 release soak | the placement oracle held every record to 1 ms and had no notion of a report-interval excursion | harness oracle: `klv_placement_excuses_one_sr_interval_excursion_under_10_ms` and `klv_placement_fails_a_second_excursion_or_an_excursion_over_10_ms` in tst-interop | 0.9.1 |
