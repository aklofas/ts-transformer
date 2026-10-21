// Unit tests of the shared SEI extractor: node --test 'docs/webrtc/test/*.test.js'
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  TST_KLV_SEI_UUID, extractKlvSeis, nalUnits, parseKlvSeiRbsp, unescape,
} from "../whep-sei-extract.js";

// --- builders (the server's rules, written independently here) ------------

/** Emulation prevention: 0x03 before any byte 0x00..0x03 that follows two zeros. */
function escape(rbsp) {
  const out = [];
  let zeros = 0;
  for (const b of rbsp) {
    if (zeros >= 2 && b <= 3) { out.push(3); zeros = 0; }
    out.push(b);
    zeros = b === 0 ? zeros + 1 : 0;
  }
  return out;
}

function ffCoded(n) {
  const out = [];
  while (n >= 255) { out.push(0xff); n -= 255; }
  out.push(n);
  return out;
}

function ptsBytes(pts) {
  const out = [];
  for (let k = 7; k >= 0; k--) out.push(Number((pts >> BigInt(8 * k)) & 0xffn));
  return out;
}

/** One SEI NAL (header + escaped RBSP, no start code). */
function seiNal({ codec = "h264", type = 5, uuid = TST_KLV_SEI_UUID, version = 1, pts = 0n, klv }) {
  const body = [...uuid, version, ...ptsBytes(pts), ...klv];
  const rbsp = [...ffCoded(type), ...ffCoded(body.length), ...body, 0x80];
  const header = codec === "h264" ? [0x06] : [0x4e, 0x01];
  return Uint8Array.from([...header, ...escape(rbsp)]);
}

const SPS = Uint8Array.from([0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01, 0x40, 0x16, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40, 0x00, 0x00, 0x0f, 0x03, 0xc5, 0x8b, 0xb8]);
const PPS = Uint8Array.from([0x68, 0xce, 0x3c, 0x80]);
const SLICE = Uint8Array.from([0x65, 0x88, 0x84, 0x00, 0x33, 0xff, 0xfe, 0xf6, 0xf0]);

function annexB(nals) {
  const out = [];
  nals.forEach((n, i) => { out.push(...(i === 0 ? [0, 0, 0, 1] : [0, 0, 1]), ...n); });
  return Uint8Array.from(out);
}

function lengthPrefixed(nals) {
  const out = [];
  for (const n of nals) out.push((n.length >>> 24) & 0xff, (n.length >>> 16) & 0xff, (n.length >>> 8) & 0xff, n.length & 0xff, ...n);
  return Uint8Array.from(out);
}

// A short ST 0601-shaped record: UL, BER length, two items, checksum item.
const KLV = Uint8Array.from([
  0x06, 0x0e, 0x2b, 0x34, 0x02, 0x0b, 0x01, 0x01, 0x0e, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
  0x0b, 0x02, 0x02, 0x00, 0x00, 0x03, 0x01, 0x41, 0x01, 0x02, 0x12, 0x34,
]);

// --- tests ------------------------------------------------------------------

test("hand-built H.264 SEI: the 9-byte prefix and an escaped record", () => {
  // Written out byte by byte, not through the builders above. The record
  // 00 00 01 forces one emulation byte; the UUID's 00 01 does not (one zero).
  const nal = Uint8Array.from([
    0x06,                   // NAL header: type 6, nal_ref_idc 0
    0x05,                   // payloadType 5
    0x1c,                   // payloadSize 28 = 16 + 1 + 8 + 3
    0x74, 0x73, 0x74, 0x4b, 0x4c, 0x56, 0x00, 0x01, 0x8c, 0xa0, 0x4e, 0x0d, 0x9b, 0x2f, 0x11, 0xe9,
    0x01,                   // version
    0x00, 0x00, 0x00, 0x03, 0x01, 0x23, 0x45, 0x67, 0x89, // PTS 0x0123456789 with one emulation byte
    0x00, 0x00, 0x03, 0x01, // record 00 00 01, escaped
    0x80,                   // rbsp_trailing_bits
  ]);
  const got = extractKlvSeis(annexB([SPS, PPS, nal, SLICE]), "h264");
  assert.equal(got.length, 1);
  assert.equal(got[0].pts, 0x0123456789n);
  assert.deepEqual([...got[0].bytes], [0, 0, 1]);
});

test("emulation prevention at every alignment, Annex B and length-prefixed", () => {
  // Zero runs ending in 00..03 at eight offsets, so the inserted 0x03 lands on
  // every byte position relative to the prefix and the record.
  for (let pad = 0; pad < 8; pad++) {
    const klv = Uint8Array.from([...new Array(pad).fill(0x41), 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 0, 0, 0, 1, 0xff, 0, 0]);
    const pts = 90000n * 3n + BigInt(pad);
    const nal = seiNal({ pts, klv });
    assert.ok(nal.includes(0x03), `pad ${pad} produced no emulation byte`);
    for (const frame of [annexB([SPS, PPS, nal, SLICE]), lengthPrefixed([SPS, PPS, nal, SLICE])]) {
      const got = extractKlvSeis(frame);
      assert.equal(got.length, 1, `pad ${pad}`);
      assert.equal(got[0].pts, pts);
      assert.deepEqual([...got[0].bytes], [...klv], `pad ${pad}`);
    }
  }
});

test("foreign SEIs and non-SEI NAL units are skipped", () => {
  const otherUuid = Uint8Array.from(TST_KLV_SEI_UUID); otherUuid[15] ^= 1;
  const foreign = [
    seiNal({ klv: KLV, uuid: otherUuid }),         // another UUID
    seiNal({ klv: KLV, type: 1 }),                 // pic_timing, not user data
    seiNal({ klv: KLV, version: 2 }),              // a version this parser does not read
    seiNal({ klv: KLV }).subarray(0, 20),          // cut short inside the prefix
    Uint8Array.from([0x65, ...TST_KLV_SEI_UUID]),  // a slice that happens to hold the UUID
  ];
  const ours = seiNal({ klv: KLV, pts: 42n });
  const got = extractKlvSeis(annexB([SPS, PPS, ...foreign, ours, SLICE]), "h264");
  assert.equal(got.length, 1);
  assert.equal(got[0].pts, 42n);
  assert.deepEqual([...got[0].bytes], [...KLV]);
});

test("a payload above 255 bytes uses 0xFF size runs", () => {
  const klv = new Uint8Array(600).map((_, i) => (i * 7) & 0xff);
  const nal = seiNal({ klv, pts: 1n << 40n });
  assert.deepEqual([...nal.subarray(2, 5)], [0xff, 0xff, 625 - 510]);
  const [rec] = extractKlvSeis(annexB([nal, SLICE]));
  assert.equal(rec.pts, 1n << 40n);
  assert.deepEqual([...rec.bytes], [...klv]);
});

test("H.265 PREFIX_SEI is read under h265 and with no codec, never as h264", () => {
  const nal = seiNal({ codec: "h265", klv: KLV, pts: 7n });
  assert.deepEqual([...nal.subarray(0, 2)], [0x4e, 0x01]);
  const vps = Uint8Array.from([0x40, 0x01, 0x0c, 0x01]);
  const frame = annexB([vps, nal, Uint8Array.from([0x26, 0x01, 0xaf])]);
  assert.equal(extractKlvSeis(frame, "h265").length, 1);
  assert.equal(extractKlvSeis(frame).length, 1);
  assert.equal(extractKlvSeis(frame, "h264").length, 0);
  const h264 = annexB([SPS, seiNal({ klv: KLV }), SLICE]);
  assert.equal(extractKlvSeis(h264, "h265").length, 0);
});

test("two records in one frame come back in NAL order", () => {
  const a = seiNal({ klv: KLV, pts: 1n });
  const b = seiNal({ klv: Uint8Array.from([9, 9]), pts: 2n });
  const got = extractKlvSeis(annexB([SPS, PPS, a, b, SLICE]));
  assert.deepEqual(got.map((r) => r.pts), [1n, 2n]);
});

test("unescape drops only the 0x03 after two zeros", () => {
  assert.deepEqual([...unescape(Uint8Array.from([0, 0, 3, 0, 0, 3, 1]))], [0, 0, 0, 0, 1]);
  assert.deepEqual([...unescape(Uint8Array.from([0, 3, 3, 0, 0, 3, 3]))], [0, 3, 3, 0, 0, 3]);
});

test("nalUnits: start codes of both lengths, trailing zeros trimmed", () => {
  const nals = nalUnits(Uint8Array.from([0, 0, 0, 1, 0x67, 0xaa, 0, 0, 0, 1, 0x68, 0xbb, 0, 0, 1, 0x65]));
  assert.deepEqual(nals.map((n) => [...n]), [[0x67, 0xaa], [0x68, 0xbb], [0x65]]);
  // Two zero bytes before a 3-byte start code: the second one is not the
  // code's, and still does not belong to the NAL unit.
  const trimmed = nalUnits(Uint8Array.from([0, 0, 1, 0x67, 0xaa, 0, 0, 0, 0, 1, 0x65]));
  assert.deepEqual(trimmed.map((n) => [...n]), [[0x67, 0xaa], [0x65]]);
  assert.equal(parseKlvSeiRbsp(Uint8Array.from([5])), null);
});

test("nalUnits: length-prefixed only when the length walk is exact", () => {
  // A length of 300 starts 00 00 01 2C, the bytes of a start code: the walk
  // consumes the buffer exactly, so it is read as lengths, not Annex B.
  const sei = seiNal({ klv: new Uint8Array(270).fill(0x55), pts: 5n });
  assert.ok(sei.length >= 256 && sei.length <= 511);
  const frame = lengthPrefixed([sei, SLICE]);
  assert.deepEqual([...frame.subarray(0, 3)], [0, 0, 1]);
  assert.equal(nalUnits(frame).length, 2);
  assert.equal(extractKlvSeis(frame)[0].pts, 5n);
  // A length past the end means Annex B, which finds nothing here; a
  // "length" of 1 followed by a header with the forbidden bit set is read
  // as a 4-byte start code and the NAL unit after it.
  assert.deepEqual(nalUnits(Uint8Array.from([0, 0, 0, 9, 0x68])), []);
  assert.deepEqual(nalUnits(Uint8Array.from([0, 0, 0, 1, 0xe5])).map((n) => [...n]), [[0xe5]]);
  // Annex B with 3-byte start codes is never mistaken for lengths.
  assert.equal(extractKlvSeis(Uint8Array.from([0, 0, 1, ...SPS, 0, 0, 1, ...seiNal({ klv: KLV }), 0, 0, 1, ...SLICE])).length, 1);
});
