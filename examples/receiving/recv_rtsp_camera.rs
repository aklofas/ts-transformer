//! Pull an MPEG-TS-over-RTP stream from an RTSP camera.
//!
//! This example demonstrates the full client-side workflow:
//! 1. Parse the `rtsp://` URL (with optional inline `user:pass@` credentials).
//! 2. OPTIONS + DESCRIBE — discover what the camera offers.
//! 3. `setup_mp2t_auto` — pick the unique PT=33 m-line and SETUP it.
//!    (If the camera advertises no MP2T m-line, the user-friendly thing
//!    would be to fall back to elementary-stream RTP — but that's a v2
//!    feature; this example just errors.)
//! 4. `into_recv_transport` — bridge the RTSP session into the existing
//!    [`DemuxReceiver`] shell.
//! 5. PLAY — start the RTP flow.
//! 6. Iterate the [`DemuxReceiver`] (it implements `Iterator`).
//! 7. Drop the client — sends TEARDOWN automatically.
//!
//! Why `setup_mp2t_auto` vs explicit `setup(&media)`?
//! The auto-pick is the right default for gimbaled-platform / STANAG
//! 4609 cameras, which advertise a single MPEG-TS m-line carrying
//! video + KLV + audio multiplexed. Cameras with separate audio +
//! video elementary-stream m-lines (PT=96 H.264 + PT=97 AAC etc.)
//! would need a different approach — that's a v2 feature.
//!
//! Why does this example mention both UDP and TCP-interleaved?
//! Many IP cameras are deployed behind NAT/firewalls that block UDP.
//! Adding `?transport=tcp` to the URL forces the TCP-interleaved path
//! (RFC 7826 §14), where RTP rides the same TCP connection as RTSP
//! control. On an `rtsp://` URL the negotiation in `setup_mp2t_auto`
//! tries UDP first and falls back to TCP-interleaved on a 461 response;
//! on an `rtsps://` URL it asks for TCP-interleaved only, so the media
//! stays inside the TLS connection (`?transport=udp` asks for UDP, whose
//! media is then unencrypted).
//!
//! Run with:
//!   cargo run -p tst-examples --example recv_rtsp_camera -- \
//!     rtsp://admin:secret@cam.local/h264
//!
//!   # Force TCP-interleaved (when UDP is blocked by NAT/firewall):
//!   cargo run -p tst-examples --example recv_rtsp_camera -- \
//!     'rtsp://admin:secret@cam.local/h264?transport=tcp'

use std::env;
use std::error::Error;

use tst_core::url::redact_url;
use tst_pipeline::DemuxReceiver;
use tst_rtp::RtspClient;

fn main() -> Result<(), Box<dyn Error>> {
    // (0) URL from argv. Default is an obvious placeholder — the example
    // is meant to be run against a real camera or a local ffmpeg/MediaMTX
    // server. The URL may carry inline credentials (`user:pass@host`) and
    // an optional `?transport=tcp` query knob.
    let args: Vec<String> = env::args().collect();
    let url = args
        .get(1)
        .map(|s| s.as_str())
        .unwrap_or("rtsp://admin:secret@cam.local/h264");
    // A URL can carry a password or a secret query value, so it is printed
    // through `redact_url`.
    eprintln!("Connecting to {}", redact_url(url));

    // (1) Connect — opens the TCP control connection. For `rtsps://`
    // this would go through rustls (requires the `tls` cargo feature on
    // tst-rtp); plain `rtsp://` is a bare TCP connection.
    let mut client = RtspClient::connect(url)?;

    // (2) OPTIONS — see what the server supports. Useful for debugging
    // camera quirks; many cameras advertise GET_PARAMETER, ANNOUNCE,
    // SET_PARAMETER beyond the OPTIONS/DESCRIBE/SETUP/PLAY/TEARDOWN
    // minimum. Some cameras don't advertise GET_PARAMETER even when they
    // implement it — keepalive falls back to OPTIONS in that case.
    let opts = client.options()?;
    eprintln!("Server supports: {:?}", opts.public_methods);

    // DESCRIBE — fetch the SDP. We get back the parsed media set; the
    // count of `m=` lines tells us how many elementary streams the
    // camera offers. For STANAG 4609 cameras this is usually 1 (a
    // single MP2T m-line); for cameras that expose separate video +
    // audio it can be 2 or 3.
    let sdp = client.describe()?;
    eprintln!("SDP: {} m-lines", sdp.media.len());

    // (3) SETUP — pick the unique MP2T m-line and negotiate transport.
    // On `rtsp://` the negotiator tries UDP unicast first (RTP_AVP), then
    // falls back to TCP-interleaved (RTP_AVP/TCP) on a 461 response; pass
    // `?transport=tcp` in the URL to skip UDP entirely. On `rtsps://` it
    // asks for TCP-interleaved only unless the URL says `?transport=udp`.
    let session = client.setup_mp2t_auto(&sdp)?;
    eprintln!("Negotiated transport: {:?}", session.transport_kind());

    // (4) Bridge — convert the RTSP session into an `RtpRecvTransport`
    // that `DemuxReceiver` knows how to drive. This consumes the
    // session; the RTSP control plane stays open on `client`.
    let recv = session.into_recv_transport();

    // (5) PLAY — server starts emitting RTP. The returned `RtpInfo` may
    // carry the first sequence number + RTP timestamp the server will
    // emit (helps callers that need an exact clock baseline). Many
    // cameras omit both fields; the client tolerates `None`.
    let info = client.play()?;
    eprintln!(
        "PLAY: first seq={:?}, first rtptime={:?}",
        info.seq, info.rtptime
    );

    // (6) Iterate the demuxer events. `DemuxReceiver` implements
    // `Iterator<Item = Result<DemuxEvent, _>>` so a plain `for` loop
    // works — `Ok(None)` (= EOF) terminates the iterator; `Err(_)` is
    // surfaced as a `Some(Err(_))` so callers can distinguish a clean
    // EOF from a transport-level break.
    //
    // We cap at 1000 events so the example terminates without external
    // coordination. A real consumer would loop until the camera goes
    // away (transport error / cancel handle fired / process signal).
    let mut demux = DemuxReceiver::new(recv);
    let mut event_count = 0;
    for ev in &mut demux {
        match ev {
            Ok(event) => {
                event_count += 1;
                if event_count % 100 == 0 {
                    eprintln!("Got {event_count} events; latest: {event:?}");
                }
                if event_count >= 1000 {
                    eprintln!("Stopping after 1000 events for demo purposes.");
                    break;
                }
            }
            Err(e) => {
                eprintln!("Demux error: {e:?}");
                break;
            }
        }
    }

    // (7) Close the client: it sends TEARDOWN, waits for the server's reply
    // (500 ms at most) and for the client's thread, so the connection is
    // shut when `close` returns. Dropping the client instead would write
    // TEARDOWN without waiting for the reply or the thread.
    //
    // Its error is reported, not returned: the receive loop has already
    // ended and the session is gone either way. A server that ended the
    // stream first makes it `Io(UnexpectedEof)`, a reply slower than
    // 500 ms makes it `Timeout`; neither is a failed run.
    match client.close() {
        Ok(()) => eprintln!("TEARDOWN answered"),
        Err(e) => eprintln!("TEARDOWN not confirmed ({e}); the session is closed anyway"),
    }
    Ok(())
}
