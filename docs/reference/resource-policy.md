# Resource policy

How much memory an RTSP or WHEP server can be made to hold, which bound
limits each queue, and what happens when a queue is full. New server
surfaces are designed against this page: each new queue gets a row in a
queue table before it ships.

## Bounds

| Bound | Default | Where |
|---|---|---|
| Sessions per server | 64 | `RtspServerBuilder::max_sessions` |
| Largest elementary access unit | 8 MiB | depacketizer `max_au_bytes` |
| KLV alignment hold | 4 096 units / 4 MiB | publish path |
| On-demand publish mounts not yet taken | 64 | `accept_unregistered_publishers` |
| On-demand publish mounts in the table | 256 | `accept_unregistered_publishers` |
| Application queue of a publish mount | 6 577 frames (about 8.7 MB) | one maximum access unit plus 64 frames |
| Reader fan-out per mount | 256 frames | `RtspServerBuilder::fanout_capacity` |

## Memory a connected publisher can hold

An elementary (H.264, optionally with KLV) publisher's media buffers can
hold, at worst:

| Buffer | Bytes |
|---|---:|
| the access unit being reassembled | 8 388 608 |
| the re-muxer's packet queue (91 182 × 188 B) | 17 142 216 |
| the re-muxer's PES scratch | 8 388 608 |
| the KLV alignment hold | 4 194 304 |
| the application queue (6 577 × 1 328 B) | 8 734 256 |
| **total** | **46 847 992 (about 46.8 MB)** |

At the default 64 sessions that is about 3.0 GB. These figures are computed
and test-pinned in one place in the source; change a bound there and this
table with it.

The total counts the media buffers only. Each session also holds its RTSP
read buffer, which a request can grow to about 1.1 MB (64 KiB of headers
plus a body of up to 1 MiB), and a publisher whose tracks use UDP holds two
64 KiB receive buffers (RTP and RTCP) per track. Allocator slack, parsed SDP
and the parameter-set caches come on top.

## Reservations

A reservation is taken before the resource it guards is claimed and is
released by a guard on every terminal path, so a refusal or a failure never
leaks a slot.

**Session slot.** The accept loop reserves one of the `max_sessions` slots
with a compare-exchange before it spawns the session task, so the counter
never exceeds the cap even for a moment. A connection that finds the cap
reached is closed and logged. On success the reservation moves into a
`SessionSlot` guard owned by the session task. The guard's `Drop` releases
the slot on every exit, including a TLS handshake that fails or times out
before any session runs.

**Publisher slot.** A publish mount allows one publisher at a time. ANNOUNCE
claims the slot (`PublisherSlot`, a generation number under one mutex) and
refuses a second claimant with `403`. RECORD re-checks that the
session still holds the claimed generation and answers `455` if not. The slot is released by
`end_publisher` when the publisher's session ends for any reason, and by the
mount's `close` when `remove_mount`, `stop()` or `Drop` closes the mount;
`close` also marks the slot closed so no later claim can succeed. Releasing
the slot does not close the application transport; closing the mount does.

## Queues

| Queue | Bound | When full | Counter |
|---|---|---|---|
| Sessions | `max_sessions`, 64 | refuse: the new connection is closed | none; a warning is logged and `ServerStats::active_sessions` reads the occupancy |
| Reader fan-out, per mount | `fanout_capacity`, 256 frames | drop oldest for the lagging reader; other readers are unaffected | `frames_dropped_total` on a local mount, `frames_dropped_readers` on a publish mount |
| Publish-mount application queue | 6 577 frames | drop newest; the publisher is never blocked | `frames_dropped_app` |
| KLV alignment hold | 4 096 units / 4 MiB | drop oldest | `klv_units_dropped` |
| Access unit being reassembled | 8 MiB | drop the access unit | `aus_dropped` |
| On-demand publish queue | 64 mounts | refuse: ANNOUNCE answers `503` and no mount is created | none; a debug line is logged |
| On-demand mounts in the table | 256 | refuse: ANNOUNCE answers `503` until a mount is removed | none; a debug line is logged |

## WHEP server

A `WhepServer` has one media thread that owns every viewer session, and one
accept thread that serves the signalling connections one after another.
Mounts push into a queue shared by the whole server; the media thread fans
each frame out to the viewers' own queues. A viewer queue holds references
to the shared frame, not copies.

| Bound | Default | Where |
|---|---|---|
| Viewers per server, across all mounts | 10 (1 to 64) | `max_viewers` URL key |
| Viewer queue | 2 048 RTP payloads (at least 16) | `MountConfig::viewer_queue_packets` |
| KLV record | 256 KiB | `KLV_DC_LIMIT` |
| KLV records waiting for a frame, per mount | 1 024 records (at most 256 MiB at the record limit) | fixed |
| SCTP send buffer, per viewer | 512 KiB (32 MiB at 64 viewers) | fixed |
| Source addresses before ICE and DTLS complete, per session | 16 | fixed |
| Signalling request | 16 KiB of headers, 64 KiB of body, 5 s to arrive | fixed |

| Queue | Bound | When full | Counter |
|---|---|---|---|
| Viewers | `max_viewers`, 10 | refuse: the offer is answered `503` with `Retry-After: 5` | `ServerStats::sessions_rejected_cap` |
| Media queue, shared by every mount | 4 096 messages | drop newest; the push still returns `Ok`. A dropped frame also drops the mount's later frames until the next keyframe, because they may reference it | `MountStats::viewer_packets_dropped_queue_full` (video payloads and Opus packets), `MountStats::klv_dc_dropped_viewer` (KLV messages) |
| Control queue (offers, DELETEs, statistics reads) | 64 requests | refuse: an offer or DELETE is answered `503`; `stats()` returns the last snapshot it read | none |
| Viewer queue | `viewer_queue_packets`, 2 048 | a frame that does not fit is dropped whole for that viewer, and the viewer waits for the next keyframe. A keyframe larger than the whole queue is admitted when the queue is empty, so the queue can exceed its bound by one keyframe; dropping it would drop every later keyframe of the same size, and the viewer would never start. A viewer whose queue stays full, with nothing written, for 30 s is closed | `SessionStats::packets_dropped_queue_full` and `SessionStats::dc_messages_dropped` per viewer, summed into the mount's two counters above; a closed viewer counts in `ServerStats::sessions_reaped_disconnected` |
| KLV records waiting for a frame | 1 024 per mount | drop oldest | `MountStats::klv_records_dropped_unbound` |
| KLV record above the limit | 256 KiB | refuse: the push returns `PushError::KlvTooLarge` | `MountStats::klv_dc_dropped_oversize` |
| SCTP send buffer | 512 KiB per viewer | the KLV message is dropped for that viewer; the video is sent regardless | `SessionStats::dc_messages_dropped`, `MountStats::klv_dc_dropped_viewer` |
| Source addresses before the handshake | 16 per session | the datagram is dropped | `ServerStats::datagrams_unrouted` |

The viewer cap needs no reservation: the media thread is the only code that
opens a session, so the check and the insert happen on one thread.

## Send-path ladder

A message sent through `MuxSender` meets three bounds in turn, from the
muxer outward. There is no `max_pending_bytes` knob: the first and third
rows already bound what a caller can hold, and the middle row, the bare
shell's retry buffer, never holds more than one call's muxer output.

| Queue | Bound | When full | Counter |
|---|---|---|---|
| Muxer output, per `push_*` call | `MuxerConfig::buffer_packets`, 10 000 TS packets | the push fails with `BufferFull` and nothing is muxed | none; the error is the report |
| `MuxSender` pending chunk queue | one `send_*` call's muxer output, so the row above bounds it | the next `send_*` drains it before muxing; if that drain fails, the call returns `input_consumed: Some(false)` without muxing its input | none |
| `ManagedTransport` gap buffer | `ReconnectPolicy::gap_buffer_capacity`, 256 messages | `overflow_policy`: `DropOldest` evicts the oldest message, `Reject` refuses the new one | `gap_messages_dropped` and `gap_bytes_dropped` in the transport's stats |

`gap_buffer_capacity` counts messages, not bytes. A send larger than the
transport's `max_payload` is refused before it is queued, so the gap buffer
holds at most `gap_buffer_capacity × max_payload` bytes; a caller that needs
a byte ceiling on the send path sizes the capacity from that product. One
exception: under `DropOldest` with a capacity of 1, the message being sent
is never the eviction victim, so for the length of that send the buffer
holds two.

## Adding a queue

State the queue's bound and unit, its full-queue policy and its counter in
the table above before the queue ships. Take a reservation before claiming
any per-session resource. Release it on every terminal transition.
