# Browser SDP offers

The offers a WHEP player's `createOffer()` produces for a receive-only video
transceiver plus a negotiated data channel (`klv`, id 0). The negotiation
tests and the `whep_sdp_select` fuzz seeds read them.

| File | Source |
|---|---|
| `chrome-155.sdp` | Captured from Google Chrome 155.0.8059.39 (headless, Linux x86_64) |
| `firefox-144.sdp` | Reconstructed, not captured |
| `safari-26.sdp` | Reconstructed, not captured |

## Chrome capture

A page that runs

```js
const pc = new RTCPeerConnection();
pc.addTransceiver('video', { direction: 'recvonly' });
pc.createDataChannel('klv', { negotiated: true, id: 0 });
const o = await pc.createOffer();
console.log(JSON.stringify(o.sdp));
```

was loaded with

```sh
google-chrome --headless=new --disable-gpu --no-sandbox \
  --enable-logging=stderr --log-level=0 --virtual-time-budget=5000 \
  file:///path/to/offer.html
```

and the SDP was taken verbatim from the `CONSOLE` line on stderr. The ICE
credentials and DTLS fingerprint belong to a throwaway peer connection.
A re-capture creates a plain data channel with a label other than `klv`
(`pc.createDataChannel('viewer')`): the server opens the `klv` channel
itself, and the offer only needs the `m=application` section.
Headless Chrome on Linux offers no H.265 payload.

## Reconstructed offers

Firefox and Safari were not available where these fixtures were made. The
two files follow each browser's documented default offer shape, with
placeholder ICE credentials and an all-`AA` fingerprint:

- `firefox-144.sdp`: VP8, VP9, H.264 `42e01f` in packetization mode 1 and
  mode 0 (the mode-0 payload has no `packetization-mode` parameter), AV1,
  RED, ULPFEC and RTX, with Firefox's `o=mozilla...` origin line.
- `safari-26.sdp`: H.264 `640c1f` and `42e01f` in mode 1 first, then H.265
  (`level-id=93;profile-id=1;tier-flag=0;tx-mode=SRST`), VP8, VP9, AV1, and
  the two H.264 profiles again in mode 0.

The tests assert what payload selection does with these payload lines, which
the reconstruction controls. Replace both files with live captures before
1.0.
