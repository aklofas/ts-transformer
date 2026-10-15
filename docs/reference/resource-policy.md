# Resource policy

How much memory an RTSP server can be made to hold, which bound limits
each queue, and what happens when a queue is full. New server surfaces
(the WebRTC viewer queues) are designed against this page: each new queue
gets a row in the queue table before it ships.

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

An elementary (H.264, optionally with KLV) publisher can hold, at worst:

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

## Adding a queue

State the queue's bound and unit, its full-queue policy and its counter in
the table above before the queue ships. Take a reservation before claiming
any per-session resource. Release it on every terminal transition.
