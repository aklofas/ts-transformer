// KLV extraction from an encoded video frame: finds every tst KLV SEI
// message in the frame's NAL units and returns the records it carries.
//
// Worker-safe: no DOM, no globals beyond ES2020. The player imports it on
// the main thread (createEncodedStreams) and the worker imports it for
// RTCRtpScriptTransform.
//
// SEI layout (one message per SEI NAL unit, after emulation prevention is
// undone): payload type 5 (user data unregistered), the payload size, a
// 16-byte UUID, one version byte (1), the record's 90 kHz PTS as 8 bytes
// big-endian, then the KLV record bytes exactly as pushed.

/** The UUID that marks a tst KLV SEI: "tstKLV", 00 01, eight fixed bytes. */
export const TST_KLV_SEI_UUID = new Uint8Array([
  0x74, 0x73, 0x74, 0x4b, 0x4c, 0x56, 0x00, 0x01,
  0x8c, 0xa0, 0x4e, 0x0d, 0x9b, 0x2f, 0x11, 0xe9,
]);

/** The version byte that follows the UUID; any other version is skipped. */
export const SEI_VERSION = 1;

const USER_DATA_UNREGISTERED = 5;
const PREFIX_LEN = 16 + 1 + 8; // UUID + version + PTS

/**
 * Every tst KLV record in one encoded frame, in NAL-unit order.
 *
 * `data` is the frame as WebRTC hands it (ArrayBuffer or Uint8Array):
 * Annex B, or 4-byte length-prefixed NAL units when the buffer parses as
 * exactly that (see nalUnits). `codec` is "h264" or "h265"; when omitted both SEI NAL types
 * are accepted (H.264 type 6, H.265 PREFIX_SEI type 39), which is safe
 * because neither header byte is an SEI under the other codec.
 *
 * Returns [{ pts: BigInt, bytes: Uint8Array }]. Foreign SEIs (another
 * payload type, UUID or version) and damaged candidates are skipped.
 */
export function extractKlvSeis(data, codec) {
  const buf = data instanceof Uint8Array ? data : new Uint8Array(data);
  const out = [];
  for (const nal of nalUnits(buf)) {
    const hdr = seiHeaderLen(nal, codec);
    if (hdr === 0) continue;
    const rec = parseKlvSeiRbsp(nal.subarray(hdr));
    if (rec) out.push(rec);
  }
  return out;
}

/** The NAL header length when `nal` is an SEI NAL of `codec`, else 0. */
function seiHeaderLen(nal, codec) {
  if (nal.length < 2) return 0;
  const h264 = (nal[0] & 0x1f) === 6 && (nal[0] & 0x80) === 0;
  const h265 = ((nal[0] >> 1) & 0x3f) === 39 && (nal[0] & 0x80) === 0;
  if (codec === "h264") return h264 ? 1 : 0;
  if (codec === "h265") return h265 ? 2 : 0;
  return h264 ? 1 : h265 ? 2 : 0;
}

/**
 * Splits a frame into NAL units (header first, no start code or length).
 *
 * Annex B is the normal form. The 4-byte length-prefixed reading is taken
 * only when a length walk consumes the buffer exactly and every NAL header
 * has its forbidden bit clear, since a length of 256 to 511 starts with the
 * bytes of a start code.
 */
export function nalUnits(buf) {
  return lengthPrefixedNalUnits(buf) ?? annexBNalUnits(buf);
}

/** The NAL units of an exact 4-byte-length-prefixed buffer, else null. */
function lengthPrefixedNalUnits(buf) {
  const out = [];
  let i = 0;
  while (i < buf.length) {
    if (i + 4 > buf.length) return null;
    const len = ((buf[i] << 24) | (buf[i + 1] << 16) | (buf[i + 2] << 8) | buf[i + 3]) >>> 0;
    i += 4;
    if (len === 0 || i + len > buf.length || (buf[i] & 0x80) !== 0) return null;
    out.push(buf.subarray(i, i + len));
    i += len;
  }
  return out.length > 0 ? out : null;
}

/** 3 or 4 for a start code at `i`, else 0. */
function startCodeAt(buf, i) {
  if (i + 3 <= buf.length && buf[i] === 0 && buf[i + 1] === 0) {
    if (buf[i + 2] === 1) return 3;
    if (i + 4 <= buf.length && buf[i + 2] === 0 && buf[i + 3] === 1) return 4;
  }
  return 0;
}

function annexBNalUnits(buf) {
  const out = [];
  let start = -1;
  let i = 0;
  while (i < buf.length) {
    const sc = startCodeAt(buf, i);
    if (sc === 0) { i++; continue; }
    if (start >= 0) out.push(trimTrailingZeros(buf.subarray(start, i)));
    i += sc;
    start = i;
  }
  if (start >= 0 && start < buf.length) out.push(trimTrailingZeros(buf.subarray(start)));
  return out.filter((n) => n.length > 0);
}

/** Zero bytes before a start code belong to the start code, not the NAL. */
function trimTrailingZeros(nal) {
  let end = nal.length;
  while (end > 0 && nal[end - 1] === 0) end--;
  return nal.subarray(0, end);
}

/**
 * Parses one SEI NAL body (header stripped, emulation prevention still in
 * place). Returns { pts, bytes } for a tst KLV SEI, else null. Only the
 * first SEI message of the NAL unit is read; the server writes one per NAL.
 */
export function parseKlvSeiRbsp(escaped) {
  const r = unescape(escaped);
  const pos = { i: 0 };
  if (readFfCoded(r, pos) !== USER_DATA_UNREGISTERED) return null;
  const size = readFfCoded(r, pos);
  if (size === null || size < PREFIX_LEN || pos.i + size > r.length) return null;
  const body = r.subarray(pos.i, pos.i + size);
  for (let k = 0; k < 16; k++) if (body[k] !== TST_KLV_SEI_UUID[k]) return null;
  if (body[16] !== SEI_VERSION) return null;
  let pts = 0n;
  for (let k = 17; k < PREFIX_LEN; k++) pts = (pts << 8n) | BigInt(body[k]);
  return { pts, bytes: body.slice(PREFIX_LEN) };
}

/** A 0xFF per full 255 then the final byte; null when the buffer ends first. */
function readFfCoded(r, pos) {
  let value = 0;
  for (;;) {
    if (pos.i >= r.length) return null;
    const b = r[pos.i++];
    value += b;
    if (b !== 0xff) return value;
  }
}

/** Drops each 0x03 that follows two zero bytes. */
export function unescape(src) {
  const out = new Uint8Array(src.length);
  let n = 0;
  let zeros = 0;
  for (let i = 0; i < src.length; i++) {
    const b = src[i];
    if (zeros >= 2 && b === 3) { zeros = 0; continue; }
    out[n++] = b;
    zeros = b === 0 ? zeros + 1 : 0;
  }
  return out.subarray(0, n);
}
