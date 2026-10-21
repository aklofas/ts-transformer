// Reference WHEP player for tst-webrtc: plays a mount in a <video> and
// delivers each KLV record to `onKlv` from both carriages, the SEI inside
// the frame (read before decode) and the `klv` data channel.
//
//   import { playWhep } from "./whep-player.js";
//   const session = await playWhep({ video, url, token, onKlv });
//   ...
//   await session.stop();
//
// `onKlv` receives { pts, bytes, via, rtpTimestamp }: the record's 90 kHz
// PTS (BigInt), its bytes, "sei" or "dc", and the RTP timestamp of the
// frame it is bound to. Records are emitted when their frame is painted
// where the browser exposes the painted frame's RTP timestamp
// (requestVideoFrameCallback metadata), else on arrival. With `dedupe`
// (the default) a record delivered by both carriages is emitted once, the
// SEI copy preferred; pass `dedupe: false` to see both.
//
// ES2020 module, no dependencies, no bundler: serve this directory as-is.

import { extractKlvSeis } from "./whep-sei-extract.js";

const PENDING_FRAMES_MAX = 64; // frames never painted still get their KLV
const SEEN_KEYS_MAX = 4096; // dedupe memory

export async function playWhep({ video, url, token, onKlv, dedupe = true }) {
  const scriptTransform = typeof RTCRtpScriptTransform === "function";
  // Chrome's createEncodedStreams needs the option; other browsers ignore it.
  const pc = new RTCPeerConnection(scriptTransform ? {} : { encodedInsertableStreams: true });
  const stream = new MediaStream(); // one stream: the answer gives each media its own msid
  video.srcObject = stream;
  let worker = null;

  // --- KLV emission -------------------------------------------------------
  const pending = new Map(); // rtpTimestamp -> [record]
  const seen = new Set();
  let paintLocked = typeof video.requestVideoFrameCallback === "function";

  const key = (rec) => `${rec.pts}:${fnv1a(rec.bytes)}`;
  function emit(rec) {
    if (dedupe) {
      const k = key(rec);
      if (seen.has(k)) return;
      seen.add(k);
      if (seen.size > SEEN_KEYS_MAX) seen.delete(seen.values().next().value);
    }
    onKlv(rec);
  }
  function emitFrame(list) {
    // The SEI copy first, so with dedupe on it is the one that wins.
    for (const rec of list) if (rec.via === "sei") emit(rec);
    for (const rec of list) if (rec.via !== "sei") emit(rec);
  }
  function queue(rec) {
    if (!paintLocked || rec.rtpTimestamp === undefined) { emit(rec); return; }
    let list = pending.get(rec.rtpTimestamp);
    if (!list) pending.set(rec.rtpTimestamp, (list = []));
    list.push(rec);
    while (pending.size > PENDING_FRAMES_MAX) { // a frame the decoder dropped
      const [ts, old] = pending.entries().next().value;
      pending.delete(ts);
      emitFrame(old);
    }
  }
  if (paintLocked) {
    const tick = (_now, meta) => {
      if (meta.rtpTimestamp === undefined) { // this browser does not expose it
        paintLocked = false;
        for (const list of pending.values()) emitFrame(list);
        pending.clear();
        return;
      }
      const list = pending.get(meta.rtpTimestamp);
      if (list) { pending.delete(meta.rtpTimestamp); emitFrame(list); }
      video.requestVideoFrameCallback(tick);
    };
    video.requestVideoFrameCallback(tick);
  }

  // --- data channel: opened by the server, arrives by label -------------
  pc.createDataChannel("viewer"); // any label but "klv": brings up SCTP in the offer
  pc.ondatachannel = ({ channel }) => {
    if (channel.label !== "klv") return;
    channel.binaryType = "arraybuffer";
    channel.onmessage = ({ data }) => {
      const b = new Uint8Array(data);
      if (b.length < 13 || b[0] !== 1) return; // version 1 only
      const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
      queue({ rtpTimestamp: dv.getUint32(1), pts: dv.getBigUint64(5), bytes: b.slice(13), via: "dc" });
    };
  };

  // --- media: video (and audio, when the mount passes Opus through) ------
  pc.addTransceiver("video", { direction: "recvonly" });
  pc.addTransceiver("audio", { direction: "recvonly" });
  pc.ontrack = ({ track, receiver }) => {
    stream.addTrack(track);
    if (track.kind !== "video") return;
    const onRecord = ({ rtpTimestamp, pts, bytes }) => queue({ rtpTimestamp, pts, bytes, via: "sei" });
    if (scriptTransform) {
      worker = new Worker(new URL("./whep-sei-worker.js", import.meta.url), { type: "module" });
      worker.onmessage = (m) => onRecord(m.data);
      receiver.transform = new RTCRtpScriptTransform(worker, {});
    } else if (typeof receiver.createEncodedStreams === "function") {
      const { readable, writable } = receiver.createEncodedStreams();
      readable.pipeThrough(new TransformStream({
        transform(frame, controller) {
          const meta = typeof frame.getMetadata === "function" ? frame.getMetadata() : {};
          const rtpTimestamp = meta.rtpTimestamp ?? frame.timestamp;
          for (const rec of extractKlvSeis(frame.data)) onRecord({ rtpTimestamp, ...rec });
          controller.enqueue(frame);
        },
      })).pipeTo(writable);
    } // else: no encoded-frame access; KLV arrives on the data channel only
  };

  // --- WHEP signalling ---------------------------------------------------
  await pc.setLocalDescription(await pc.createOffer());
  const headers = { "Content-Type": "application/sdp" };
  if (token) headers.Authorization = `Bearer ${token}`;
  const res = await fetch(url, { method: "POST", headers, body: pc.localDescription.sdp });
  if (res.status !== 201) {
    pc.close();
    throw new Error(`WHEP ${res.status}: ${await res.text()}`);
  }
  const location = new URL(res.headers.get("Location"), url);
  await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });

  return {
    pc,
    async stop() {
      try { await fetch(location, { method: "DELETE", headers: token ? { Authorization: `Bearer ${token}` } : {} }); } catch { /* best effort */ }
      pc.close();
      if (worker) worker.terminate();
      video.srcObject = null;
    },
  };
}

/** FNV-1a (32-bit) over the record bytes, for the dedupe key. */
function fnv1a(bytes) {
  let h = 0x811c9dc5;
  for (let i = 0; i < bytes.length; i++) h = Math.imul(h ^ bytes[i], 0x01000193);
  return h >>> 0;
}
