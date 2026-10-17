//! The old names this crate still accepts compile and name the new types,
//! at the root re-export and at the defining module path.

#![allow(deprecated)]

#[test]
fn raw_stats_names_resolve_at_both_paths() {
    fn send(_: Option<&tst_pipeline::RawSendStats>) {}
    fn send_mod(_: Option<&tst_pipeline::raw_sender::RawSendStats>) {}
    fn recv(_: Option<&tst_pipeline::RawRecvStats>) {}
    fn recv_mod(_: Option<&tst_pipeline::raw_receiver::RawRecvStats>) {}
    // Each old name is the new type: the new spelling is accepted where the old is named.
    let s: Option<&tst_pipeline::RawSenderStats> = None;
    send(s);
    send_mod(s);
    let r: Option<&tst_pipeline::RawReceiverStats> = None;
    recv(r);
    recv_mod(r);
}
