//! MuxSender-from-URL example.
//!
//! Demonstrates the full `srt://host:port?key=value` flow: parse the URL,
//! apply its overlay to a fresh `SocketConfig`, connect. Then opens an
//! intentionally-malformed URL to show how each `UrlError` variant
//! surfaces — the error vocabulary is part of the API and worth seeing
//! end-to-end.
//!
//! Why this style: the URL is the deployment-time override; in production
//! pipelines, URL parameters typically arrive from config files or
//! orchestration metadata. Treating them as an overlay (rather than the
//! sole source of truth) lets your code set sensible defaults via the
//! builder while still honoring per-deployment tuning.
//!
//! Run with:
//!   cargo run -p tst-examples --example sender_from_url
//!
//! There's no listener side here; the connect call will fail with
//! `connection refused` or similar — the point is the URL parse + apply.

use tst_srt::{SocketBuilder, SrtUrl, UrlError};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Happy path: parse a URL with a representative subset of options.
    //
    // Why we picked these: streamid is the universal application-routing
    // key, latency is the most-tuned live-streaming knob, passphrase
    // covers the encryption common case. pbkeylen=24 selects AES-192 to
    // demonstrate that pbkeylen pairs naturally with passphrase.
    let url = "srt://example.invalid:9000\
               ?streamid=front-camera\
               &latency=200\
               &passphrase=hunter-too-long-thanks\
               &pbkeylen=24";
    let parsed = SrtUrl::parse(url)?;
    println!("host={}, port={}", parsed.host, parsed.port);

    // 2. Build a SocketConfig — start from a builder so any in-code
    //    defaults you want layered "underneath" the URL are easy to set
    //    (the URL will overwrite them, which is exactly the intended
    //    semantic for deployment overrides).
    //
    // Bind-then-step shape (`SocketBuilder` is `&mut self -> &mut Self`):
    // construct, mutate, then call the terminal `config()`. This shape
    // mirrors how Kotlin/Swift/Python bindings will spell the same idiom
    // — see `docs/reference/binding-authors.md`.
    let mut sb = SocketBuilder::new();
    // Hypothetical baked-in default: 100ms latency. URL says 200ms,
    // so this gets overwritten — that's the intended behavior.
    sb.latency_ms(100);
    let mut config = sb.config();
    parsed.overlay.apply_to_socket(&mut config);
    println!(
        "applied overlay; config has latency = {:?}, streamid = {:?}",
        config.latency,
        config.stream_id.as_ref().map(|s| s.as_str()),
    );
    // The latency is now 200ms (URL won), even though the builder said
    // 100ms. The streamid came purely from the URL.

    // 3. Connect attempt (will fail since example.invalid:9000 isn't a
    //    real host — this section is for showing the connect call
    //    shape).
    //
    // Uncomment if running against a real listener:
    // let socket = tst_srt::Socket::connect_with(
    //     &config,
    //     format!("{}:{}", parsed.host, parsed.port).as_str(),
    // )?;

    // 4. Show how each UrlError variant surfaces — useful for operators
    //    who need to know which malformed URL produces which message.
    //
    // We exercise the most-common variants here. The full set lives in
    // the parser's tests and in docs/guides/srt.md.

    let cases: &[(&str, &str)] = &[
        ("syntax", "not-a-url"),
        ("wrong scheme", "https://1.2.3.4:9000"),
        ("missing port", "srt://1.2.3.4"),
        ("userinfo", "srt://op:hunter2@1.2.3.4:9000"),
        ("unsupported mode", "srt://1.2.3.4:9000?mode=rendezvous"),
        ("unsupported key", "srt://1.2.3.4:9000?transtype=file"),
        ("unknown key", "srt://1.2.3.4:9000?lattency=100"),
        ("invalid value", "srt://1.2.3.4:9000?latency=200ms"),
        ("option validation", "srt://1.2.3.4:9000?pbkeylen=15"),
    ];
    for (label, bad_url) in cases {
        match SrtUrl::parse(bad_url) {
            Ok(_) => println!("[{label}] unexpectedly parsed OK"),
            Err(e) => print_error_classification(label, &e),
        }
    }

    Ok(())
}

fn print_error_classification(label: &str, e: &UrlError) {
    // Why a separate helper: distinguishing variant family in operator
    // logs is the point of this example. The Display impl on UrlError
    // already produces a readable message; here we add the variant tag
    // so operators can see "ah, that's a parser-level type error
    // (InvalidValue) vs. a typed-constructor rejection (OptionValidation)".
    let variant = match e {
        UrlError::Syntax(_) => "Syntax",
        UrlError::WrongScheme { .. } => "WrongScheme",
        UrlError::MissingPort => "MissingPort",
        UrlError::MissingHost => "MissingHost",
        UrlError::UserinfoNotSupported => "UserinfoNotSupported",
        UrlError::UnsupportedMode { .. } => "UnsupportedMode",
        UrlError::UnsupportedKey { .. } => "UnsupportedKey",
        UrlError::UnknownKey { .. } => "UnknownKey",
        UrlError::InvalidValue { .. } => "InvalidValue",
        UrlError::OptionValidation { .. } => "OptionValidation",
        _ => "Other",
    };
    println!("[{label}] {variant}: {e}");
}
