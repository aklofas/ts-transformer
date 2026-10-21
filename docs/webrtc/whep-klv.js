// The player's KLV logic with no browser in it, so node can test it: the
// data-channel message parser and the emission queue (paint-locked or
// on-arrival delivery, dedupe across the two carriages).
//
// Records are { rtpTimestamp, pts, bytes, via }: the bound frame's RTP
// timestamp, the record's 90 kHz PTS as a BigInt, the KLV bytes, and
// "sei" or "dc".

export const PENDING_FRAMES_MAX = 64; // backstop: record-bearing frames never painted (about 6 s at 10 Hz KLV on 30 fps video)
const SEEN_KEYS_MAX = 4096; // dedupe memory

/**
 * Parses one `klv` data-channel message: version 1, the bound frame's
 * 4-byte RTP timestamp, the 8-byte KLV PTS (both big-endian), the record.
 * Returns { rtpTimestamp, pts, bytes, via: "dc" }, or null for a body that
 * is too short or of another version.
 */
export function parseKlvMessage(data) {
  const b = data instanceof Uint8Array ? data : new Uint8Array(data);
  if (b.length < 13 || b[0] !== 1) return null;
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  return { rtpTimestamp: dv.getUint32(1), pts: dv.getBigUint64(5), bytes: b.slice(13), via: "dc" };
}

/** True when RTP timestamp `a` is at or before `b` in serial-number order. */
export function rtpAtOrBefore(a, b) {
  return ((b - a) >>> 0) < 0x80000000;
}

/** FNV-1a (32-bit) over the record bytes, for the dedupe key. */
export function fnv1a(bytes) {
  let h = 0x811c9dc5;
  for (let i = 0; i < bytes.length; i++) h = Math.imul(h ^ bytes[i], 0x01000193);
  return h >>> 0;
}

/**
 * Orders KLV delivery to `onKlv`.
 *
 * Paint-locked (`paintLocked: true`): a record waits until `painted(ts)`
 * reports a frame at or after its own in serial-number order, then every
 * waiting record up to that frame is emitted, oldest frame first. The
 * server sends frames in presentation order, so a record whose own frame
 * is never painted goes out when a later frame is. As a backstop, when
 * more than `pendingMax` record-bearing frames wait, the oldest is emitted
 * (about 6 s at 10 Hz KLV on 30 fps video). On-arrival
 * (`paintLocked: false`, or after `flushAll()`): a record is emitted as it
 * is pushed.
 *
 * With `dedupe` a record delivered by both carriages (same PTS and bytes)
 * is emitted once. Within a frame the SEI copies go first, so the SEI copy
 * wins; on arrival the first copy wins.
 */
export class KlvQueue {
  constructor({ onKlv, dedupe = true, paintLocked = true, pendingMax = PENDING_FRAMES_MAX }) {
    this.onKlv = onKlv;
    this.dedupe = dedupe;
    this.paintLocked = paintLocked;
    this.pendingMax = pendingMax;
    this.pending = new Map(); // rtpTimestamp -> [record], in arrival order
    this.seen = new Set();
  }

  push(rec) {
    if (!this.paintLocked || rec.rtpTimestamp === undefined) { this.emit(rec); return; }
    let list = this.pending.get(rec.rtpTimestamp);
    if (!list) this.pending.set(rec.rtpTimestamp, (list = []));
    list.push(rec);
    while (this.pending.size > this.pendingMax) {
      const [ts, old] = this.pending.entries().next().value;
      this.pending.delete(ts);
      this.emitFrame(old);
    }
  }

  /** A frame with RTP timestamp `ts` was painted. */
  painted(ts) {
    const due = [...this.pending.keys()].filter((t) => rtpAtOrBefore(t, ts));
    due.sort((a, b) => ((a - ts) | 0) - ((b - ts) | 0)); // oldest frame first
    for (const t of due) {
      const list = this.pending.get(t);
      this.pending.delete(t);
      this.emitFrame(list);
    }
  }

  /** Leaves paint-locked mode for good and emits everything waiting. */
  flushAll() {
    this.paintLocked = false;
    for (const list of this.pending.values()) this.emitFrame(list);
    this.pending.clear();
  }

  emitFrame(list) {
    for (const rec of list) if (rec.via === "sei") this.emit(rec);
    for (const rec of list) if (rec.via !== "sei") this.emit(rec);
  }

  emit(rec) {
    if (this.dedupe) {
      // The bound frame is part of the key: on an MP2T mount every
      // asynchronous record carries PTS 0, so without it a repeated set
      // (a static security set, a producer's unchanged record) would be
      // dropped on every later frame.
      const k = `${rec.rtpTimestamp}:${rec.pts}:${rec.bytes.length}:${fnv1a(rec.bytes)}`;
      if (this.seen.has(k)) return;
      this.seen.add(k);
      if (this.seen.size > SEEN_KEYS_MAX) this.seen.delete(this.seen.values().next().value);
    }
    this.onKlv(rec);
  }
}
