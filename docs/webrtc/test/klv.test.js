// Unit tests of the data-channel parser and the emission queue:
// node --test 'docs/webrtc/test/*.test.js'
import { test } from "node:test";
import assert from "node:assert/strict";
import { KlvQueue, parseKlvMessage, rtpAtOrBefore } from "../whep-klv.js";

function message({ version = 1, rtp = 0, pts = 0n, record = [1, 2, 3] }) {
  const b = new Uint8Array(13 + record.length);
  const dv = new DataView(b.buffer);
  b[0] = version;
  dv.setUint32(1, rtp);
  dv.setBigUint64(5, pts);
  b.set(record, 13);
  return b;
}

const rec = (via, rtp, pts = 1n, bytes = [1, 2, 3]) => ({ via, rtpTimestamp: rtp, pts, bytes: Uint8Array.from(bytes) });

test("parseKlvMessage reads version, RTP timestamp, PTS and record", () => {
  const got = parseKlvMessage(message({ rtp: 0xfedcba98, pts: 0x0123456789n, record: [9, 8] }).buffer);
  assert.deepEqual({ ...got, bytes: [...got.bytes] }, { rtpTimestamp: 0xfedcba98, pts: 0x0123456789n, bytes: [9, 8], via: "dc" });
  // A record of zero bytes is still a message; an offset view is honoured.
  const padded = new Uint8Array([0xff, ...message({ rtp: 7, record: [] })]).subarray(1);
  assert.deepEqual(parseKlvMessage(padded).rtpTimestamp, 7);
});

test("parseKlvMessage refuses a short body and another version", () => {
  assert.equal(parseKlvMessage(message({}).subarray(0, 12)), null);
  assert.equal(parseKlvMessage(new Uint8Array(0)), null);
  assert.equal(parseKlvMessage(message({ version: 2 })), null);
});

test("rtpAtOrBefore follows serial-number order across the wrap", () => {
  assert.ok(rtpAtOrBefore(100, 200));
  assert.ok(rtpAtOrBefore(200, 200));
  assert.ok(!rtpAtOrBefore(201, 200));
  assert.ok(rtpAtOrBefore(0xfffffff0, 0x10));
  assert.ok(!rtpAtOrBefore(0x10, 0xfffffff0));
});

test("on arrival: every record is emitted as pushed", () => {
  const out = [];
  const q = new KlvQueue({ onKlv: (r) => out.push(r.rtpTimestamp), paintLocked: false });
  q.push(rec("dc", 300)); q.push(rec("sei", 100, 2n));
  assert.deepEqual(out, [300, 100]);
});

test("paint-locked: a painted frame releases its records and every earlier frame's", () => {
  const out = [];
  const q = new KlvQueue({ onKlv: (r) => out.push([r.rtpTimestamp, r.via]), dedupe: false });
  q.push(rec("dc", 200, 2n)); q.push(rec("sei", 100, 1n)); q.push(rec("sei", 300, 3n));
  assert.deepEqual(out, []);
  q.painted(200);
  assert.deepEqual(out, [[100, "sei"], [200, "dc"]]); // oldest frame first
  q.painted(250);                                       // nothing due
  assert.deepEqual(out.length, 2);
  q.painted(300);
  assert.deepEqual(out.at(-1), [300, "sei"]);
});

test("paint-locked: the flush order survives the 32-bit wrap", () => {
  const out = [];
  const q = new KlvQueue({ onKlv: (r) => out.push(r.rtpTimestamp), dedupe: false });
  q.push(rec("sei", 0x10, 2n)); q.push(rec("sei", 0xfffffff0, 1n)); q.push(rec("sei", 0x20, 3n));
  q.painted(0x10);
  assert.deepEqual(out, [0xfffffff0, 0x10]);
});

test("paint-locked: the backstop emits the oldest frame past the cap", () => {
  const out = [];
  const q = new KlvQueue({ onKlv: (r) => out.push(r.rtpTimestamp), dedupe: false, pendingMax: 64 });
  for (let i = 1; i <= 64; i++) q.push(rec("sei", i * 10, BigInt(i)));
  assert.deepEqual(out, []);
  q.push(rec("sei", 650, 65n));
  assert.deepEqual(out, [10]);
  q.flushAll();
  assert.equal(out.length, 65);
  q.push(rec("sei", 700, 70n)); // on arrival from now on
  assert.equal(out.at(-1), 700);
});

test("dedupe: one record on both carriages is emitted once, SEI preferred", () => {
  const out = [];
  const q = new KlvQueue({ onKlv: (r) => out.push(r.via) });
  q.push(rec("dc", 100)); q.push(rec("sei", 100));
  q.painted(100);
  assert.deepEqual(out, ["sei"]);
  // Different bytes at the same PTS are two records.
  q.push(rec("dc", 200, 1n, [4, 5])); q.painted(200);
  assert.deepEqual(out, ["sei", "dc"]);
  // On arrival the first copy wins.
  const first = [];
  const p = new KlvQueue({ onKlv: (r) => first.push(r.via), paintLocked: false });
  p.push(rec("dc", 300)); p.push(rec("sei", 300));
  assert.deepEqual(first, ["dc"]);
  // dedupe off: both copies.
  const both = [];
  const n = new KlvQueue({ onKlv: (r) => both.push(r.via), dedupe: false, paintLocked: false });
  n.push(rec("dc", 300)); n.push(rec("sei", 300));
  assert.deepEqual(both, ["dc", "sei"]);
});
