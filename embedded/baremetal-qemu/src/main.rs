#![no_std]
#![no_main]
// `loop {}` after `debug::exit` is the no_std unreachable idiom (debug::exit
// returns `()`, so the loop satisfies `fn main() -> !`).
#![allow(clippy::empty_loop)]

extern crate alloc;

mod recv;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::MaybeUninit;

#[cfg(target_arch = "arm")]
use cortex_m_rt::entry;
#[cfg(target_arch = "arm")]
use cortex_m_semihosting::{debug, hprintln};
#[cfg(target_arch = "arm")]
use panic_semihosting as _;

#[cfg(target_arch = "riscv32")]
use riscv_rt::entry;
#[cfg(target_arch = "riscv32")]
use riscv_semihosting::{debug, hprintln};

use embedded_alloc::Heap;
use spin::Mutex;

/// riscv32's mirror of `panic-semihosting`'s ARM-only panic handler (that
/// crate's whole content is `#![cfg(target_arch = "arm")]` — built for any
/// other arch it compiles to an empty crate with no `#[panic_handler]`, so
/// it cannot cover this target). Same shape: print the panic message to the
/// host's semihosting stdout (`hstdout`), then exit QEMU with a failure
/// status so a panic surfaces as a non-zero process exit rather than a
/// silent hang.
#[cfg(target_arch = "riscv32")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    if let Ok(mut out) = riscv_semihosting::hio::hstdout() {
        let _ = writeln!(out, "{info}");
    }
    debug::exit(debug::EXIT_FAILURE);
    loop {}
}

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::transport::{BrokenCause, Transport, TransportError};
use tst_pipeline::MuxSender;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Loopback, Medium};
use smoltcp::socket::udp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr, IpEndpoint};

#[global_allocator]
static HEAP: Heap = Heap::empty();

const HEAP_SIZE: usize = 256 * 1024;
static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];

/// Committed, CI-guarded golden produced by `gen-scenarios`. Resolved relative
/// to this file: `embedded/baremetal-qemu/src/` → `../../../crates/tst-integration/...`.
static GOLDEN: &[u8] =
    include_bytes!("../../../crates/tst-integration/tests/fixtures/scenarios/video-roundtrip/output.ts");

/// Verbatim copy of `tst_integration::scenarios::synthetic_h264_idr()`.
/// (tst-integration is std-only and not a dependency; per spec decision A the
/// short sequence is duplicated, and the on-device byte-compare detects drift.)
fn synthetic_h264_idr() -> Vec<u8> {
    let mut buf = Vec::with_capacity(20);
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // Annex-B start code
    buf.push(0x65); // nal_ref_idc=11, nal_unit_type=5 (IDR)
    for i in 0u8..15 {
        buf.push(0xA5 ^ i);
    }
    buf
}

/// Verbatim copy of `tst_integration::scenarios::video_roundtrip_ts_bytes()`.
fn video_roundtrip_ts_bytes() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("valid muxer config")
    };
    let mut mux = Muxer::new(cfg).expect("muxer init");
    mux.push_video(&synthetic_h264_idr(), Pts90khz::new(0), /*key_frame=*/ true)
        .expect("push_video");

    // Drain (mirrors scenarios::drain_mux: 1316-byte = 7×188 chunks).
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Minimal in-memory `Transport`. `Transport: Send` is unconditional, so the
/// single-threaded sink uses `Arc<spin::Mutex<_>>` (Send/Sync) rather than the
/// `!Send` `Rc<RefCell<_>>`. Mirrors the std Task-3 sink in
/// `crates/tst-pipeline/tests/mux_sender_golden.rs`.
/// (Verbatim-local per spec decision A: the no_std binary cannot depend on the
/// std-only tst-integration; the on-device byte-compare detects drift.)
#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Transport for Sink {
    fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
        self.0.lock().extend_from_slice(b);
        Ok(())
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn close(&mut self) {}
    fn is_alive(&self) -> bool {
        true
    }
}

/// Local loopback address/port the transport sends to and receives from. With
/// the `Loopback` device, every transmitted frame is echoed back as RX, so a
/// datagram sent to our own bound port comes straight back to the same socket.
const LOOPBACK_PORT: u16 = 9000;

/// A `no_std` `Transport` that serializes each `send_bytes` message as one UDP
/// datagram through a real smoltcp IPv4 stack over a `phy::Loopback` device,
/// then drains the looped-back datagram and appends its recovered payload to a
/// shared accumulator. This upgrades the Check-2 `Vec` `Sink` from "bytes
/// copied" to "bytes serialized through a real UDP/IP stack and parsed back".
///
/// Owned by `MuxSender` by value, so the unconditional `Transport: Send`
/// supertrait costs nothing here (no `Arc<Mutex>` around the transport state).
/// The `acc` accumulator is the one shared cell — read back by Check 3 after
/// `MuxSender::close`, mirroring the Check-2 readback pattern.
struct SmoltcpUdpTransport {
    device: Loopback,
    iface: Interface,
    sockets: SocketSet<'static>,
    handle: SocketHandle,
    clock_ms: i64,
    endpoint: IpEndpoint,
    acc: Arc<Mutex<Vec<Vec<u8>>>>,
}

/// Shared smoltcp loopback UDP/IP stack: a `Loopback` device + `Interface`
/// bound to 127.0.0.1/8, with one UDP socket bound to `LOOPBACK_PORT`.
/// Factored out so both the send-side (`SmoltcpUdpTransport`, check 3) and
/// receive-side (`recv::LoopbackRecvTransport`, check 4) harnesses build an
/// identical stack without duplicating the setup — each gets its own
/// independent `Loopback` device instance, so both may bind the same local
/// port with no conflict.
fn new_loopback_udp_stack() -> (Loopback, Interface, SocketSet<'static>, SocketHandle) {
    let mut device = Loopback::new(Medium::Ethernet);

    // Locally-administered fake MAC; loopback resolves its own ARP.
    let config = Config::new(EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]).into());
    let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
    iface.update_ip_addrs(|addrs| {
        addrs
            .push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8))
            .expect("ip addr slot");
    });

    // Owned (Vec-backed) packet buffers — available because the smoltcp
    // `alloc` feature is enabled. 16 metadata slots / 4 KiB payload each is
    // ample for the 564-byte golden (one ~564-byte datagram per chunk).
    let rx_buf = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 4096]);
    let tx_buf = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 4096]);
    let mut socket = udp::Socket::new(rx_buf, tx_buf);
    socket.bind(LOOPBACK_PORT).expect("udp bind");

    let mut sockets = SocketSet::new(vec![]);
    let handle = sockets.add(socket);

    (device, iface, sockets, handle)
}

impl SmoltcpUdpTransport {
    fn new(acc: Arc<Mutex<Vec<Vec<u8>>>>) -> Self {
        let (device, iface, sockets, handle) = new_loopback_udp_stack();
        Self {
            device,
            iface,
            sockets,
            handle,
            clock_ms: 0,
            endpoint: IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), LOOPBACK_PORT),
            acc,
        }
    }

    /// Advance the manual clock and poll the interface once, then drain any
    /// datagrams the loopback delivered back into the accumulator.
    fn poll_and_drain(&mut self) {
        self.clock_ms += 1;
        self.iface
            .poll(Instant::from_millis(self.clock_ms), &mut self.device, &mut self.sockets);

        let socket = self.sockets.get_mut::<udp::Socket>(self.handle);
        let mut tmp = [0u8; 1500];
        while socket.can_recv() {
            if let Ok((n, _meta)) = socket.recv_slice(&mut tmp) {
                self.acc.lock().push(tmp[..n].to_vec());
            } else {
                break;
            }
        }
    }
}

impl Transport for SmoltcpUdpTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        {
            let socket = self.sockets.get_mut::<udp::Socket>(self.handle);
            if !socket.can_send() {
                // Send buffer full this pass — retry-identical-slice contract.
                return Err(TransportError::Backpressure {
                    msg: String::from("udp send buffer full"),
                    errno_code: None,
                });
            }
            socket
                .send_slice(msg, self.endpoint)
                .map_err(|_| TransportError::Broken {
                    msg: String::from("udp send_slice failed"),
                    errno_code: None, cause: BrokenCause::Unspecified,
                })?;
        }
        // 16 polls is generous: the first send needs a couple of passes for
        // neighbor resolution + flush, then the datagram loops back; subsequent
        // sends need ~1-2.
        for _ in 0..16 {
            self.poll_and_drain();
        }
        Ok(())
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn close(&mut self) {}

    fn is_alive(&self) -> bool {
        true
    }
}

/// Same config as `video_roundtrip_ts_bytes`, driven through `MuxSender`
/// instead of a bare `Muxer`. Output must equal the same golden.
fn mux_sender_roundtrip_ts_bytes() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("valid muxer config")
    };
    let collected = Arc::new(Mutex::new(Vec::new()));
    let sender = MuxSender::new(Sink(Arc::clone(&collected)), cfg).expect("mux sender");
    sender
        .send_video(&synthetic_h264_idr(), Pts90khz::new(0), /*key_frame=*/ true)
        .expect("send_video");
    sender.close();
    // The `let` binding is load-bearing: it drops the `MutexGuard` temporary
    // at end-of-statement, before `collected` itself drops. Inlining as a tail
    // expression extends the guard's borrow past `collected`'s drop (E0597).
    let out = collected.lock().clone();
    out
}

/// Same config as Check 2, but driven through `MuxSender<SmoltcpUdpTransport>`:
/// every TS chunk is sent as a UDP datagram, looped back through the smoltcp
/// stack, recovered, and accumulated. Returns each received datagram as its own
/// `Vec<u8>` so the caller can assert both byte content and datagram boundaries.
fn mux_sender_over_udp_loopback() -> Vec<Vec<u8>> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("valid muxer config")
    };
    let acc = Arc::new(Mutex::new(Vec::new()));
    let transport = SmoltcpUdpTransport::new(Arc::clone(&acc));
    let sender = MuxSender::new(transport, cfg).expect("mux sender");
    sender
        .send_video(&synthetic_h264_idr(), Pts90khz::new(0), /*key_frame=*/ true)
        .expect("send_video");
    sender.close();
    // `let` binding drops the MutexGuard at end-of-statement before `acc`
    // drops, same as Check 2.
    let out = acc.lock().clone();
    out
}

/// Print a richer FAIL line for a golden mismatch: byte counts plus the first
/// offset where the two slices diverge (or where one ends early) with the
/// expected and actual byte values in hex. Keeps the PASS token byte-identical.
fn report_mismatch(label: &str, got: &[u8], want: &[u8]) {
    let first = got
        .iter()
        .zip(want.iter())
        .position(|(g, w)| g != w)
        .unwrap_or_else(|| got.len().min(want.len()));
    let exp_byte = want.get(first).copied().unwrap_or(0);
    let got_byte = got.get(first).copied().unwrap_or(0);
    hprintln!(
        "FAIL[{}]: produced {} bytes, golden {} bytes; first mismatch at offset {} (expected 0x{:02x}, got 0x{:02x})",
        label,
        got.len(),
        want.len(),
        first,
        exp_byte,
        got_byte,
    );
}

#[entry]
fn main() -> ! {
    unsafe { HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) }

    // Check 1 — bare muxer (P7(a) regression guard).
    let muxer_out = video_roundtrip_ts_bytes();
    if muxer_out != GOLDEN {
        report_mismatch("muxer", &muxer_out, GOLDEN);
        debug::exit(debug::EXIT_FAILURE);
        loop {}
    }

    // Check 2 — MuxSender shell over an in-memory transport (P7(b)).
    let sender_out = mux_sender_roundtrip_ts_bytes();
    if sender_out != GOLDEN {
        report_mismatch("mux_sender", &sender_out, GOLDEN);
        debug::exit(debug::EXIT_FAILURE);
        loop {}
    }

    // Check 3 — MuxSender over a real smoltcp UDP/IP loopback transport (P7c).
    let datagrams = mux_sender_over_udp_loopback();
    // Check 3a — flatten and byte-compare, mirroring the prior golden check.
    let udp_out: Vec<u8> = datagrams.iter().flat_map(|d| d.iter().copied()).collect();
    if udp_out != GOLDEN {
        report_mismatch("udp_loopback", &udp_out, GOLDEN);
        debug::exit(debug::EXIT_FAILURE);
        loop {}
    }

    // Check 3b — datagram boundaries must mirror MuxSender's max_payload
    // chunking. The flatten-compare above can't see a chunking regression
    // (e.g. splitting mid-TS-packet) as long as the bytes still concatenate.
    // The 564-byte golden fits one ≤1316-byte send, so expected == [GOLDEN].
    let expected: Vec<&[u8]> = GOLDEN.chunks(1316).collect();
    if datagrams.len() != expected.len()
        || datagrams.iter().zip(&expected).any(|(g, w)| g.as_slice() != *w)
    {
        hprintln!(
            "FAIL[udp_datagram_boundaries]: got {} datagrams (lens {:?}), want {} (lens {:?})",
            datagrams.len(),
            datagrams.iter().map(|d| d.len()).collect::<Vec<_>>(),
            expected.len(),
            expected.iter().map(|d| d.len()).collect::<Vec<_>>(),
        );
        debug::exit(debug::EXIT_FAILURE);
        loop {}
    }

    // Check 4 — no_std DemuxReceiver<LoopbackRecvTransport> recovers the
    // pushed video AUs over a real smoltcp UDP/IP receive-side stack: the
    // runtime proof for the receiver-path no_std work. Builds its own
    // dedicated send-side dataset (see recv.rs module docs for why — the
    // 3-packet check-3 golden is one confirming sync byte short of what
    // Receiver's sync-lock heuristic needs); compares against the same
    // synthetic AU the send side pushed, repeated once per push — not
    // GOLDEN, since DemuxReceiver hands back the demuxed video payload, not
    // raw TS bytes.
    let recv_out = recv::udp_recv_check();
    let expected_au = synthetic_h264_idr().repeat(recv::CHECK4_AU_PUSHES);
    if recv_out != expected_au {
        report_mismatch("udp_recv", &recv_out, &expected_au);
        debug::exit(debug::EXIT_FAILURE);
        loop {}
    }

    hprintln!(
        "PASS: muxer + mux_sender + udp_loopback match golden ({} bytes); \
         udp_recv recovered {} AU(s) byte-exact via DemuxReceiver",
        GOLDEN.len(),
        recv::CHECK4_AU_PUSHES
    );
    debug::exit(debug::EXIT_SUCCESS);
    loop {}
}
