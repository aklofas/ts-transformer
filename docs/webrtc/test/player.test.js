// Session lifecycle of the reference player against fakes (no browser):
// node --test 'docs/webrtc/test/*.test.js'
import { test } from "node:test";
import assert from "node:assert/strict";

function installFakes({ fetchImpl }) {
  const made = { pcs: [], workers: [] };
  globalThis.document = { baseURI: "http://localhost/" };
  globalThis.MediaStream = class { addTrack() {} };
  globalThis.RTCRtpScriptTransform = class {};
  globalThis.Worker = class {
    constructor() { this.terminated = false; made.workers.push(this); }
    terminate() { this.terminated = true; }
  };
  globalThis.RTCPeerConnection = class {
    constructor() {
      this.closed = false;
      this.localDescription = { sdp: "offer" };
      made.pcs.push(this);
    }
    createDataChannel() {}
    addTransceiver() { return { receiver: {} }; }
    async createOffer() { return { type: "offer", sdp: "offer" }; }
    async setLocalDescription() {}
    async setRemoteDescription(d) { if (this.rejectAnswer) throw new Error("bad answer"); }
    close() { this.closed = true; }
  };
  globalThis.fetch = fetchImpl;
  return made;
}

function fakeVideo() {
  const v = {
    srcObject: null,
    callbacks: new Map(),
    nextId: 1,
    requestVideoFrameCallback(cb) { const id = this.nextId++; this.callbacks.set(id, cb); return id; },
    cancelVideoFrameCallback(id) { this.callbacks.delete(id); },
    // Runs the registered callbacks as the browser would for one painted frame.
    paint(rtpTimestamp) {
      const cbs = [...this.callbacks.values()];
      this.callbacks.clear();
      for (const cb of cbs) cb(0, { rtpTimestamp });
    },
  };
  return v;
}

const answer = () => new Response("answer", { status: 201, headers: { Location: "/res/1" } });
const load = () => import("../whep-player.js?" + Math.random());

test("a DELETE that never settles does not delay local teardown and is aborted", async () => {
  let deleteSignal;
  const made = installFakes({
    fetchImpl: (u, opts) => {
      if (opts.method === "POST") return Promise.resolve(answer());
      deleteSignal = opts.signal;
      return new Promise((_, rej) => opts.signal.addEventListener("abort", () => rej(new Error("aborted"))));
    },
  });
  const { playWhep } = await load();
  const video = fakeVideo();
  const session = await playWhep({ video, url: "/whep/x", onKlv() {} });
  const stopping = session.stop();
  assert.equal(made.pcs[0].closed, true);
  assert.equal(made.workers[0].terminated, true);
  assert.equal(video.srcObject, null);
  assert.equal(video.callbacks.size, 0);
  await stopping; // resolves once the 2 s bound aborts the DELETE
  assert.equal(deleteSignal.aborted, true);
});

test("a second session on the same element is not disturbed by the first one's stop", async () => {
  const made = installFakes({
    fetchImpl: (u, opts) => Promise.resolve(opts.method === "POST" ? answer() : new Response(null, { status: 200 })),
  });
  const { playWhep } = await load();
  const video = fakeVideo();
  const got = [];
  const first = await playWhep({ video, url: "/whep/x", onKlv: (r) => got.push(["first", r]) });
  await first.stop();
  assert.equal(video.callbacks.size, 0, "stopped session leaves no paint callback");
  const second = await playWhep({ video, url: "/whep/x", onKlv: (r) => got.push(["second", r]) });
  const secondStream = video.srcObject;
  assert.equal(video.callbacks.size, 1);
  await first.stop(); // idempotent, must not clear the second session's stream
  assert.equal(video.srcObject, secondStream);
  assert.equal(made.pcs[1].closed, false);
  video.paint(1000);
  assert.equal(video.callbacks.size, 1, "the live session keeps repainting");
  await second.stop();
  assert.equal(video.callbacks.size, 0);
  assert.deepEqual(got, []);
});

test("a rejected answer runs teardown and deletes the created resource", async () => {
  const calls = [];
  const made = installFakes({
    fetchImpl: (u, opts) => {
      calls.push(opts.method);
      return Promise.resolve(opts.method === "POST" ? answer() : new Response(null, { status: 200 }));
    },
  });
  RTCPeerConnection.prototype.rejectAnswer = true;
  const { playWhep } = await load();
  const video = fakeVideo();
  await assert.rejects(playWhep({ video, url: "/whep/x", onKlv() {} }), /bad answer/);
  delete RTCPeerConnection.prototype.rejectAnswer;
  assert.equal(made.pcs[0].closed, true);
  assert.equal(made.workers[0].terminated, true);
  assert.equal(video.srcObject, null);
  assert.equal(video.callbacks.size, 0);
  assert.deepEqual(calls, ["POST", "DELETE"]);
});
