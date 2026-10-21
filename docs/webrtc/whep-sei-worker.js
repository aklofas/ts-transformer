// RTCRtpScriptTransform worker for the reference player: reads every
// encoded video frame before it is decoded, posts the tst KLV records found
// in its SEI messages, and passes the frame through unchanged.
//
// Loaded as a module worker by whep-player.js:
//   receiver.transform = new RTCRtpScriptTransform(worker, {});
// Each record is posted as { rtpTimestamp, pts, bytes }: the frame's RTP
// timestamp (the same value the data-channel message carries), the record's
// 90 kHz PTS as a BigInt, and the KLV bytes.

import { extractKlvSeis } from "./whep-sei-extract.js";

/** The RTP timestamp of an encoded frame, from getMetadata() where exposed. */
export function frameRtpTimestamp(frame) {
  const meta = typeof frame.getMetadata === "function" ? frame.getMetadata() : {};
  return meta.rtpTimestamp ?? frame.timestamp;
}

/** A TransformStream that posts each frame's KLV records to `post`. */
export function seiTransform(post) {
  return new TransformStream({
    transform(frame, controller) {
      const records = extractKlvSeis(frame.data);
      if (records.length > 0) {
        const rtpTimestamp = frameRtpTimestamp(frame);
        for (const { pts, bytes } of records) {
          post({ rtpTimestamp, pts, bytes }, [bytes.buffer]);
        }
      }
      controller.enqueue(frame);
    },
  });
}

if (typeof self !== "undefined" && "onrtctransform" in self) {
  self.onrtctransform = (event) => {
    const { readable, writable } = event.transformer;
    readable
      .pipeThrough(seiTransform((msg, transfer) => self.postMessage(msg, transfer)))
      .pipeTo(writable);
  };
}
