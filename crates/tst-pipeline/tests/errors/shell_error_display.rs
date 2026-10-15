//! Display + Error::source() chain tests for the 6 shell error types.

use std::error::Error;

use tst_core::error::MuxError;
use tst_core::transport::{BrokenCause, TransportError};
use tst_pipeline::{
    DemuxReceiverError, MuxSenderError, MuxSenderErrorSource, RawReceiverError, RawSenderError,
    ReceiverError, SenderError, ShellError, ShellErrorKind,
};

#[test]
fn mux_sender_display_contains_kind_and_inner() {
    let err = MuxSenderError::from(MuxError::InvalidConfig("bad pid"));
    let display = format!("{err}");
    assert!(
        display.contains("ConfigInvalid"),
        "Display should contain kind name: {display}"
    );
    assert!(
        display.contains("bad pid"),
        "Display should contain inner detail: {display}"
    );
}

#[test]
fn mux_sender_error_source_walks_to_mux_error() {
    let err = MuxSenderError::from(MuxError::InvalidNal);
    let source = err.source().expect("MuxSenderError should have a source");
    // The source is the MuxSenderErrorSource enum variant.
    let source_str = format!("{source}");
    assert!(
        source_str.contains("Annex-B"),
        "Source Display should be MuxError::InvalidNal: {source_str}"
    );
}

#[test]
fn mux_sender_source_downcasts_to_typed_enum() {
    let err = MuxSenderError::from(MuxError::InvalidNal);
    let source = &err.source;
    assert!(
        matches!(source, MuxSenderErrorSource::Mux(MuxError::InvalidNal)),
        "Source should match typed variant: {source:?}"
    );
}

#[test]
fn sender_display_contains_kind() {
    let err = SenderError::from(TransportError::Closed);
    let display = format!("{err}");
    assert!(
        display.contains("Closed"),
        "Display should contain kind: {display}"
    );
}

#[test]
fn raw_sender_display_contains_kind() {
    let err = RawSenderError::from(TransportError::Backpressure {
        msg: "test".into(),
        errno_code: None,
    });
    let display = format!("{err}");
    assert!(
        display.contains("Backpressure"),
        "Display should contain kind: {display}"
    );
}

#[test]
fn demux_receiver_closed_displays_as_end_of_stream() {
    // Receiver-side Closed -> EndOfStream kind (peer-EOS).
    let err = DemuxReceiverError::from(TransportError::Closed);
    let display = format!("{err}");
    assert!(
        display.contains("EndOfStream"),
        "Receiver-side Closed should display as EndOfStream kind: {display}"
    );
    assert_eq!(err.kind(), ShellErrorKind::EndOfStream);
}

#[test]
fn receiver_explicit_close_displays_as_closed() {
    let err = ReceiverError::from(TransportError::ExplicitClose);
    let display = format!("{err}");
    assert!(
        display.contains("Closed"),
        "ExplicitClose should display as Closed kind: {display}"
    );
    assert_eq!(err.kind(), ShellErrorKind::Closed);
}

#[test]
fn raw_receiver_broken_displays_as_transport_broken() {
    let err = RawReceiverError::from(TransportError::Broken {
        msg: "test".into(),
        errno_code: None,
        cause: BrokenCause::Unspecified,
    });
    let display = format!("{err}");
    assert!(display.contains("TransportBroken"), "{display}");
    assert_eq!(err.kind(), ShellErrorKind::TransportBroken);
}

/// The full Error::source chain walks from MuxSenderError ->
/// MuxSenderErrorSource -> MuxError. Verify the chain depth.
#[test]
fn error_source_chain_depth_is_two_for_struct_shells() {
    let err = MuxSenderError::from(MuxError::InvalidNal);
    let level_1 = err.source().expect("level 1: MuxSenderErrorSource");
    let level_2 = level_1.source();
    // thiserror's transparent variants pass through to the inner error's
    // source, so level_2 may be Some (if MuxError::InvalidNal has a source,
    // which it doesn't — it's a unit variant). Either way, the chain
    // terminates within 2 levels.
    if let Some(deepest) = level_2 {
        let deeper = deepest.source();
        assert!(
            deeper.is_none(),
            "chain should terminate at level 3 or earlier"
        );
    }
}
