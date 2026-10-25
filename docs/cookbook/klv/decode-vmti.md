# Decode VMTI per-target detections from an ST 0601 stream

> **When to use this:** ISR capture analysis — surface detected/tracked targets per frame via Tag 74 (VMTI Local Set).

> **Related:**
> - [guides/klv.md](/docs/guides/klv.md) — sibling-layer composition (ST 0601 → ST 0903 VMTI)
> - [guides/mpegts-demux.md](/docs/guides/mpegts-demux.md) — `MetadataKind::KlvSyncAuCell` vs `KlvAsync` carriage
> - [Example: `decode_vmti_metadata`](/examples/klv-metadata/decode_vmti_metadata.rs)

When you're working with an ISR capture and want to surface the
detected/tracked targets per frame, sibling-layer composition again:
decode the parent ST 0601 LS, then if Tag 74 is non-empty run
`klv::st0903::decode` on the inner bytes. Sync (`KlvSyncAuCell`) and
async (`KlvAsync`) carriage paths both surface KLV LS bytes — accept
either so VMTI from a KLVA-async producer also flows through.

```rust,no_run
use tst_core::klv::{st0601, st0903};
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, MetadataKind};

# fn process_capture(bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
let mut demuxer = Demuxer::new();
// Feed bounded chunks (one feed() over the 4 MiB ingress ceiling fails)
// and drain after each; the trailing `None` flushes the last PES.
for chunk in bytes.chunks(188 * 1024).map(Some).chain([None]) {
    match chunk {
        Some(chunk) => demuxer.feed(chunk)?,
        None => demuxer.flush(),
    }
    while let Some(event) = demuxer.next_event() {
        let DemuxEvent::Metadata { kind, payload, .. } = event else {
            continue;
        };
        match kind {
            MetadataKind::KlvSyncAuCell { .. } | MetadataKind::KlvAsync => {}
            MetadataKind::Unknown(_) => continue,
        }

        let uas = st0601::decode(&payload)?;
        let Some(vmti_bytes) = uas.vmti.as_deref() else {
            continue;
        };
        let vmti = st0903::decode(vmti_bytes)?;

        for target in &vmti.targets {
            // Analyst-actionable subset — pair with VObjectSeries
            // (Tag 107, deferred typed layer) for classification.
            println!(
                "target {}: centroid_px={:?} bbox=({:?}..{:?}) confidence={:?}",
                target.target_id,
                target.centroid_pixel,
                target.bbox_top_left_pixel,
                target.bbox_bottom_right_pixel,
                target.confidence_level,
            );
        }
    }
}
# Ok(())
# }
```

Runnable form: `cargo run -p tst-examples --example decode_vmti_metadata -- capture.ts`.

For per-target classification labels (VObjectSeries), per-target
track state (VTracker), pixel masks (VMask), or image cutouts (VChip
/ VChipSeries), see the deferred-features note on typed nested VMTI
Local Sets.
