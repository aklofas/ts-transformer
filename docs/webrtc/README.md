# Reference WHEP player

The browser side of `tst-webrtc`: four plain ES modules, no bundler, no
dependencies. A page loads them with `<script type="module">` from any web
server that serves this directory as static files (file paths must stay
together: the player imports its three siblings by relative URL).

| File | Role |
|---|---|
| `whep-player.js` | `playWhep({ video, url, token, onKlv, dedupe })`: WHEP signalling, one `MediaStream`, the `klv` data channel, KLV emission |
| `whep-sei-worker.js` | the `RTCRtpScriptTransform` worker that reads each encoded frame's KLV SEI before decode |
| `whep-sei-extract.js` | the SEI parser both of the above share (also usable on its own) |
| `whep-klv.js` | the data-channel message parser and the emission queue (paint-locked delivery, dedupe), with no browser in it |

```html
<video id="v" autoplay playsinline muted></video>
<script type="module">
  import { playWhep } from "./whep-player.js";
  const session = await playWhep({
    video: document.getElementById("v"),
    url: "http://192.0.2.10:8889/cam1",     // the mount's WHEP endpoint
    onKlv: ({ pts, bytes, via }) => console.log(via, pts, bytes.length),
  });
  // later: await session.stop();
</script>
```

`onKlv` gets `{ pts, bytes, via, rtpTimestamp }` per record. With `dedupe`
on (the default) a record that arrives both in the SEI and on the data
channel is delivered once; `dedupe: false` delivers both copies, which is
what the interop page uses to prove each carriage. The guide
(`/docs/guides/webrtc.md`) explains the carriages, the timing rules and the
browser support table.

These files are exercised in CI, not only documented, in two ways: the
interop matrix's browser cells load them in headless Chrome (with the
player's de-duplication off, so each carriage is proved on its own), and the
`node --test` unit tests under `test/` run in the `ci` workflow and cover
the default path the cells do not: the SEI extractor, the message parser,
the queue and the de-duplication. Run them from the repository root with
`node --test 'docs/webrtc/test/*.test.js'`.
