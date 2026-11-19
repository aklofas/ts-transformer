//! Pushes over `rtsps://` into an `RtspServer` with a TLS acceptor: an
//! MPEG-TS push arrives byte for byte, an H.264 + KLV push is aligned from
//! its sender reports, and a server whose certificate the client does not
//! trust never sees a request.

use std::time::Duration;

use tst_core::net::tls::load_root_store;
use tst_core::transport::{RecvTransport as _, TransportError};
use tst_rtp::{
    ClockAlignment, PUSH_MP2T_MAX_PAYLOAD, PublishMountConfig, PushDescription, RtspClientBuilder,
    RtspError, RtspPush, RtspServerBuilder,
};

use crate::common::*;
use crate::fixtures::publish_mount::{klv_events, video_samples};
use crate::fixtures::raw_rtsp::scaled;
use crate::fixtures::raw_rtsp_publisher::{klv_unit, ts_fixture_packets};
use crate::fixtures::tls_certs::SelfSignedCert;

/// A started `rtsps://` server with a publish mount on [`PATH`], and the
/// trust store that accepts its certificate.
struct TlsServed {
    served: Served,
    roots: rustls::RootCertStore,
    _cert: SelfSignedCert,
}

impl TlsServed {
    fn with(configure: impl FnOnce(&mut RtspServerBuilder), mount: PublishMountConfig) -> Self {
        let cert = SelfSignedCert::generate();
        let served = Served::at(
            "rtsps://127.0.0.1:0",
            |b| {
                b.tls_cert(cert.cert_path.clone(), cert.key_path.clone());
                configure(b);
            },
            mount,
        );
        let roots = load_root_store(cert.root_pem.as_bytes()).expect("trust bundle");
        Self {
            served,
            roots,
            _cert: cert,
        }
    }

    /// A client builder for `path` on this server, trusting `roots`.
    fn builder_at(&self, path: &str, roots: rustls::RootCertStore) -> RtspClientBuilder {
        RtspClientBuilder::new(&format!("rtsps://127.0.0.1:{}{path}", self.served.port))
            .unwrap()
            .tls_root_certs(roots)
    }

    /// A push to `path` from a client that trusts the server.
    fn push_at(&self, path: &str, desc: &PushDescription) -> RtspPush {
        RtspPush::connect(self.builder_at(path, self.roots.clone()), desc).unwrap()
    }
}

#[test]
fn an_rtsps_mp2t_push_arrives_byte_identical() {
    let s = TlsServed::with(
        |_| {},
        PublishMountConfig::new().end_on_publisher_exit(true),
    );
    let mut app = s.served.app();
    let mut push = s.push_at(PATH, &PushDescription::Mp2t);
    assert!(s.served.mount.publisher().is_some());
    let ts: Vec<u8> = ts_fixture_packets(210).concat();
    for bundle in ts.chunks(7 * 188) {
        push.send_ts(bundle).unwrap();
    }
    push.close().unwrap();
    // Everything the mount yields until it ends with its publisher.
    let got = read_ts_to_end(&mut app);
    assert_same_bytes(&got, &ts, "the mount's stream");
    let stats = s.served.mount.stats();
    assert_eq!(stats.rtp_packets_received, ts.chunks(7 * 188).len() as u64);
    assert_eq!(stats.malformed_packets, 0);
}

/// The KLV units sit half a frame after their access unit, so the server
/// can place them only from the sender reports. As over `rtsp://`, the
/// server maps a KLV time to the video clock through the reports' NTP
/// times in 32.32 fixed point and rounds down to a whole tick, so a unit
/// may land one tick early.
#[test]
fn an_rtsps_h264_klv_push_aligns() {
    const AUS: usize = 30;
    const KLV_OFFSET: i64 = AU_STEP / 2;
    let s = TlsServed::with(
        |_| {},
        PublishMountConfig::new().end_on_publisher_exit(true),
    );
    let app = s.served.app();
    let mut push = s.push_at(PATH, &elementary(true));
    let units: Vec<Vec<u8>> = (0..AUS as u32).map(klv_unit).collect();
    for (i, unit) in units.iter().enumerate() {
        let at = 9_000 + i as i64 * AU_STEP;
        push.send_video(&access_unit(i), pts(at)).unwrap();
        push.send_klv(unit, pts(at + KLV_OFFSET)).unwrap();
    }
    s.served.stats_when("both tracks' reports", |st| {
        st.alignment == ClockAlignment::SenderReport
    });
    push.close().unwrap();
    let events = demux_to_end(app);
    let stats = s.served.mount.stats();
    assert_eq!(stats.alignment, ClockAlignment::SenderReport);
    assert_eq!(stats.alignment_steps, 0);
    assert_eq!(stats.klv_units_emitted, AUS as u64);
    assert_eq!(stats.klv_units_dropped, 0);
    assert_eq!(stats.malformed_packets, 0);

    let video: Vec<i64> = video_samples(&events).iter().map(|v| v.0).collect();
    let klv = klv_events(&events);
    assert_eq!(video.len(), AUS, "video samples demuxed");
    assert_eq!(klv.len(), AUS, "KLV units demuxed");
    for (i, ((at, payload), unit)) in klv.iter().zip(&units).enumerate() {
        let sent = i as i64 * AU_STEP + KLV_OFFSET;
        let early = sent - (at - video[0]);
        assert!(
            (0..=1).contains(&early),
            "KLV unit {i} at {} past the first video sample, sent at {sent}",
            at - video[0]
        );
        assert_eq!(payload, unit, "bytes of KLV unit {i}");
    }
}

/// The server takes ANNOUNCEs for paths it has no mount for, and queues
/// each mount such an ANNOUNCE creates, in order. A client that does not
/// trust the server's certificate fails in the handshake, before any
/// request: a trusted push made after it is the first and only ANNOUNCE
/// the server saw.
#[test]
fn an_rtsps_push_to_an_untrusted_certificate_is_refused_before_announce() {
    let s = TlsServed::with(
        |b| {
            b.accept_unregistered_publishers(true);
        },
        PublishMountConfig::new(),
    );
    let stranger = SelfSignedCert::generate();
    let other_roots = load_root_store(stranger.root_pem.as_bytes()).expect("trust bundle");
    let refused = RtspPush::connect(
        s.builder_at("/untrusted", other_roots),
        &PushDescription::Mp2t,
    );
    match refused {
        Err(RtspError::Tls(msg)) => assert!(msg.contains("certificate"), "{msg}"),
        other => panic!("expected RtspError::Tls, got {other:?}"),
    }

    let control = s.push_at("/control", &PushDescription::Mp2t);
    let server = &s.served.server;
    let first = server
        .next_publisher(scaled(Duration::from_secs(5)))
        .unwrap()
        .expect("the trusted push's ANNOUNCE created a mount");
    assert_eq!(first.mount_path(), "/control");
    assert!(
        server
            .next_publisher(Duration::from_millis(200))
            .unwrap()
            .is_none(),
        "a second ANNOUNCE reached the server"
    );
    let stats = server.stats();
    assert_eq!(stats.mounts, 2, "{stats:?}");
    assert_eq!(stats.active_publishers, 1, "{stats:?}");
    assert!(s.served.mount.publisher().is_none());
    control.close().unwrap();
}

/// The payload rate of the throughput measurement.
#[cfg(target_os = "linux")]
const MEASURE_BITS_PER_SECOND: f64 = 20e6;

/// How long the throughput measurement pushes, per scheme.
#[cfg(target_os = "linux")]
const MEASURE_FOR: Duration = Duration::from_secs(30);

/// A send that takes longer than this counts as parked: it waited for room
/// in the media queue, or its thread was not scheduled for that long.
#[cfg(target_os = "linux")]
const PARKED: Duration = Duration::from_millis(5);

/// User plus system CPU time of `/proc/self/<path>/stat`, in clock ticks.
#[cfg(target_os = "linux")]
fn cpu_ticks(path: &str) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/self/{path}stat")).unwrap();
    // The command name, in parentheses, may hold spaces: count fields from
    // after it. `utime` and `stime` are fields 14 and 15, the state field 3.
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    let fields: Vec<&str> = rest.split(' ').collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

/// The ids of this process's threads named `name`.
#[cfg(target_os = "linux")]
fn threads_named(name: &str) -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|e| {
            let tid = e.ok()?.file_name().into_string().ok()?;
            let comm = std::fs::read_to_string(format!("/proc/self/task/{tid}/comm")).ok()?;
            (comm.trim_end() == name).then_some(tid)
        })
        .collect()
}

/// Push [`MEASURE_FOR`] of MPEG-TS at [`MEASURE_BITS_PER_SECOND`], whole
/// [`PUSH_MP2T_MAX_PAYLOAD`] payloads paced against the monotonic clock, through a client
/// `builder` makes, to `served`'s mount, and describe what the sends and
/// the client's thread did.
#[cfg(target_os = "linux")]
fn measure_push(served: &Served, builder: RtspClientBuilder) -> String {
    use std::time::Instant;

    let mut app = served.app();
    let reader = std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        let mut total = 0usize;
        loop {
            match app.recv_bytes(&mut buf) {
                Ok(n) => total += n,
                Err(TransportError::Closed) => return total,
                Err(e) => {
                    eprintln!("the mount's reader stopped after {total} bytes: {e:?}");
                    return total;
                }
            }
        }
    });

    let before: Vec<String> = threads_named("tst-rtsp-client");
    let mut push = RtspPush::connect(builder, &PushDescription::Mp2t).unwrap();
    let io: Vec<String> = threads_named("tst-rtsp-client")
        .into_iter()
        .filter(|t| !before.contains(t))
        .collect();
    let io_stat = match io.as_slice() {
        [tid] => Some(format!("task/{tid}/")),
        _ => None,
    };
    let io_cpu = || io_stat.as_deref().map(cpu_ticks);

    let interval =
        Duration::from_secs_f64((PUSH_MP2T_MAX_PAYLOAD * 8) as f64 / MEASURE_BITS_PER_SECOND);
    let sends = (MEASURE_FOR.as_secs_f64() / interval.as_secs_f64()) as u32;
    let (io0, proc0) = (io_cpu(), cpu_ticks(""));
    let start = Instant::now();
    let mut last_return = start;
    let (mut parked, mut longest_send, mut widest_gap) = (0u32, Duration::ZERO, Duration::ZERO);
    for i in 0..sends {
        let due = start + interval * i;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        let payload = ts_packets(PUSH_MP2T_MAX_PAYLOAD / 188, i as u8);
        let t0 = Instant::now();
        push.send_ts(&payload).unwrap();
        let t1 = Instant::now();
        let took = t1 - t0;
        parked += u32::from(took > PARKED);
        longest_send = longest_send.max(took);
        widest_gap = widest_gap.max(t1 - last_return);
        last_return = t1;
    }
    let pushed_for = last_return - start;
    let behind = last_return.saturating_duration_since(start + interval * (sends - 1));
    served.stats_when("every packet at the mount", |st| {
        st.rtp_packets_received >= u64::from(sends)
    });
    let wall = start.elapsed();
    let (io1, proc1) = (io_cpu(), cpu_ticks(""));
    push.close().unwrap();
    let received = reader.join().unwrap();

    // SAFETY: sysconf reads a constant of the system; no memory is shared.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let cores = |ticks: u64| ticks as f64 / hz / wall.as_secs_f64();
    let io_line = match (io0, io1) {
        (Some(a), Some(b)) => format!(
            "client thread {:.2} s CPU = {:.3} of a core",
            (b - a) as f64 / hz,
            cores(b - a)
        ),
        _ => format!(
            "client thread not identified ({} new threads named tst-rtsp-client)",
            io.len()
        ),
    };
    format!(
        "{sends} sends over {:.2} s (last return {:.1} ms behind schedule), \
         {parked} parked (> {PARKED:?}), longest send {longest_send:?}, \
         widest gap between returns {widest_gap:?}; {io_line}; \
         whole process {:.2} s CPU = {:.3} of a core over {:.2} s; \
         the mount's reader got {received} of {} bytes",
        pushed_for.as_secs_f64(),
        behind.as_secs_f64() * 1e3,
        (proc1 - proc0) as f64 / hz,
        cores(proc1 - proc0),
        wall.as_secs_f64(),
        u64::from(sends) * PUSH_MP2T_MAX_PAYLOAD as u64,
    )
}

/// What a push costs the client's thread at 20 Mb/s over `rtsp://` and over
/// `rtsps://`: sends that waited for room in the media queue, the widest
/// gap between sends, and the thread's CPU time.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "a measurement: pushes for a minute and prints what it saw"]
fn tls_push_throughput() {
    let plain = Served::ending_on_exit();
    let over_tcp = measure_push(&plain, plain.builder());
    println!("rtsp://  {over_tcp}");
    let tls = TlsServed::with(
        |_| {},
        PublishMountConfig::new().end_on_publisher_exit(true),
    );
    let over_tls = measure_push(&tls.served, tls.builder_at(PATH, tls.roots.clone()));
    println!("rtsps:// {over_tls}");
}
