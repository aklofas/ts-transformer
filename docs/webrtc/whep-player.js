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
import { KlvQueue, parseKlvMessage } from "./whep-klv.js";

export async function playWhep({ video, url, token, onKlv, dedupe = true }) {
  const endpoint = new URL(url, document.baseURI);
  const scriptTransform = typeof RTCRtpScriptTransform === "function";
  // Chrome's createEncodedStreams needs the option; other browsers ignore it.
  const pc = new RTCPeerConnection(scriptTransform ? {} : { encodedInsertableStreams: true });
  const stream = new MediaStream(); // one stream: the answer gives each media its own msid
  video.srcObject = stream;
  let worker = null;

  // --- KLV emission -------------------------------------------------------
  const paintLocked = typeof video.requestVideoFrameCallback === "function";
  const queue = new KlvQueue({ onKlv, dedupe, paintLocked });
  if (paintLocked) {
    const tick = (_now, meta) => {
      if (meta.rtpTimestamp === undefined) { queue.flushAll(); return; } // not exposed here
      queue.painted(meta.rtpTimestamp);
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
      const rec = parseKlvMessage(data);
      if (rec) queue.push(rec);
    };
  };

  // --- media: video (and audio, when the mount passes Opus through) ------
  const videoTransceiver = pc.addTransceiver("video", { direction: "recvonly" });
  pc.addTransceiver("audio", { direction: "recvonly" });
  const onRecord = (rec) => queue.push({ ...rec, via: "sei" });
  if (scriptTransform) {
    // Set on the receiver before signalling: the transform must exist before
    // the remote description is applied, or Chrome never routes frames to
    // it (one set in ontrack starts its worker and then sees no frame).
    // The spec allows it here in every browser.
    worker = new Worker(new URL("./whep-sei-worker.js", import.meta.url), { type: "module" });
    worker.onmessage = (m) => onRecord(m.data);
    videoTransceiver.receiver.transform = new RTCRtpScriptTransform(worker, {});
  }
  pc.ontrack = ({ track, receiver }) => {
    stream.addTrack(track);
    if (scriptTransform) return;
    if (typeof receiver.createEncodedStreams === "function") {
      // Legacy Chrome: createEncodedStreams is called here, in ontrack, before
      // any frame arrives (where that browser documents it, and where it
      // receives every frame), and every receiver's streams must be piped or
      // its frames stall.
      const { readable, writable } = receiver.createEncodedStreams();
      if (track.kind !== "video") { readable.pipeTo(writable); return; }
      readable.pipeThrough(new TransformStream({
        transform(frame, controller) {
          try {
            const meta = typeof frame.getMetadata === "function" ? frame.getMetadata() : {};
            const rtpTimestamp = meta.rtpTimestamp ?? frame.timestamp;
            for (const rec of extractKlvSeis(frame.data)) onRecord({ rtpTimestamp, ...rec });
          } catch { /* a parse failure must not stop the video */ }
          controller.enqueue(frame);
        },
      })).pipeTo(writable);
    } // else: no encoded-frame access; KLV arrives on the data channel only
  };

  // --- WHEP signalling ---------------------------------------------------
  let location;
  try {
    await pc.setLocalDescription(await pc.createOffer());
    const headers = { "Content-Type": "application/sdp" };
    if (token) headers.Authorization = `Bearer ${token}`;
    const res = await fetch(endpoint, { method: "POST", headers, body: pc.localDescription.sdp });
    if (res.status !== 201) throw new Error(`WHEP ${res.status}: ${await res.text()}`);
    location = new URL(res.headers.get("Location"), endpoint);
    await pc.setRemoteDescription({ type: "answer", sdp: await res.text() });
  } catch (e) {
    pc.close();
    if (worker) worker.terminate();
    video.srcObject = null;
    throw e;
  }

  return {
    pc,
    async stop() {
      const headers = token ? { Authorization: `Bearer ${token}` } : {};
      try { await fetch(location, { method: "DELETE", headers }); } catch { /* best effort */ }
      pc.close();
      if (worker) worker.terminate();
      video.srcObject = null;
    },
  };
}
