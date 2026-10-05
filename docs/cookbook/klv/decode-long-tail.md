# Reading waypoint lists, weapons stores, and SDCC covariance

> **When to use this:** The ST 0601 long tail (repeat-pack and covariance items) is populated and you need typed access — not just the field families the [guides/klv.md](/docs/guides/klv.md) long-tail overview covers.

> **Related:**
> - [guides/klv.md](/docs/guides/klv.md) — the extended-range precedence rule, the field-family list, and a scalar-only long-tail snippet
> - [Decode ST 0601 from a captured `.klv` blob](/docs/cookbook/klv/decode-st0601-blob.md) — the decode-ladder starting point this recipe builds on

`UasDatalinkLs` types 142 of the 143 active ST 0601.19 items. Most
of the long tail is plain `Option<f64>`/`Option<u32>` scalars — see the
guide above for those. This recipe covers the three long-tail shapes
that need more than a field read: two **repeated-record lists**
(Item 140 Weapons Stores, Item 141 Waypoint List) and one
**cross-referenced covariance pack** (Item 102 SDCC-FLP, decoded via
the sibling `klv::st1010` module).

```rust,no_run
use tst_core::klv::st0601::decode;
use tst_core::klv::st1010::decode_sdcc_flp;

fn walk_long_tail(buf: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let ls = decode(buf)?;

    // Item 141: Waypoint List — repeated records, each with an id,
    // a prosecution-order signal (0=current, >0=planned, <0=historical,
    // 0x7FFF=cancelled), and an optional WGS84 Location (lat/lon
    // mandatory-pair, hae optional).
    if let Some(waypoints) = &ls.waypoint_list {
        for wp in waypoints {
            let pos = wp
                .location
                .and_then(|loc| Some((loc.lat_deg?, loc.lon_deg?)));
            println!(
                "waypoint {}: order={} pos={:?}",
                wp.id, wp.prosecution_order, pos
            );
        }
    }

    // Item 140: Weapons Stores — station/hardpoint/carriage/store
    // addressing plus a packed status word; use the accessor methods
    // instead of hand-masking status_raw.
    if let Some(stores) = &ls.weapons_stores {
        for ws in stores {
            println!(
                "store {}: {} armed={} laser={}",
                ws.store_id,
                ws.weapon_type,
                ws.weapon_armed(),
                ws.laser_enabled(),
            );
        }
    }

    // A couple of the long-tail plain scalars, for context:
    if let Some(agl) = ls.altitude_agl_m {
        println!("altitude AGL: {agl:.1} m (Item 113, IMAPB)");
    }
    if let Some(freq) = ls.transmission_frequency_mhz {
        println!("tx frequency: {freq:.3} MHz (Item 132, IMAPB)");
    }

    // Item 102: SDCC-FLP — a general-purpose covariance pack (MISB
    // ST 1010.3) with no ST 0601 knowledge of its own. UasDatalinkLs
    // keeps the raw pack bytes plus which preceding wire tags each
    // occurrence refines; decode the pack separately with st1010.
    for field in &ls.sdcc_flps {
        let sdcc = decode_sdcc_flp(&field.bytes)?;
        println!(
            "SDCC-FLP refining tags {:?}: matrix_size={} sigma_0={:?}",
            field.preceding_tags,
            sdcc.matrix_size,
            sdcc.std_devs.first(),
        );
        // sdcc.correlation(i, j) indexes either the diagonal (std dev)
        // or the upper-triangle correlation coefficient, symmetrized.
    }

    Ok(())
}
```

```python
from tstrans.klv import decode_uas_datalink, decode_sdcc_flp

# buf: raw ST 0601 Local Set bytes — see the decode-ladder recipe linked above.
ls = decode_uas_datalink(buf)

# Item 141: Waypoint List.
if ls.waypoint_list is not None:
    for wp in ls.waypoint_list:
        pos = None
        if wp.location is not None and wp.location.lat_deg is not None:
            pos = (wp.location.lat_deg, wp.location.lon_deg)
        print(f"waypoint {wp.id}: order={wp.prosecution_order} pos={pos}")

# Item 140: Weapons Stores.
if ls.weapons_stores is not None:
    for ws in ls.weapons_stores:
        print(
            f"store {ws.store_id}: {ws.weapon_type} "
            f"armed={ws.weapon_armed} laser={ws.laser_enabled}"
        )

# A couple of the long-tail plain scalars, for context.
if ls.altitude_agl_m is not None:
    print(f"altitude AGL: {ls.altitude_agl_m:.1f} m (Item 113, IMAPB)")
if ls.transmission_frequency_mhz is not None:
    print(f"tx frequency: {ls.transmission_frequency_mhz:.3f} MHz (Item 132, IMAPB)")

# Item 102: SDCC-FLP, decoded via the sibling st1010 module.
for field in ls.sdcc_flps:
    sdcc = decode_sdcc_flp(field.bytes)
    sigma_0 = sdcc.std_devs[0] if sdcc.std_devs else None
    print(
        f"SDCC-FLP refining tags {field.preceding_tags}: "
        f"matrix_size={sdcc.matrix_size} sigma_0={sigma_0}"
    )
```

## Notes

**Other long-tail families**, one line each — grep
`crates/tst-core/src/klv/st0601/packs.rs` (Rust) or
`bindings/python/python/tstrans/klv.py` (Python) for the type name:

- `ViewDomain` (Item 142) — up to three `(start, range)` azimuth/elevation/roll pairs.
- `CountryCodes` (Item 122) and `AirbaseLocations` (Item 130) — flight-plan context packs.
- `PayloadList` (Item 138) / `active_payloads` (Item 139 bitmask) — onboard payload inventory + which are active.
- `WavelengthRecord` list (Item 128) / `active_wavelengths` (Item 121 BER-OID list) — sensor wavelength catalog + which are active.
- `SensorFrameRate` (Item 127) and `MetadataSubstreamId` (Item 143) — stream-shape packs.
- `ControlCommand` (Item 115, multi-instance) plus `control_command_verification` (Item 116) — command/ack pairing.
- `ImageHorizonPixels` (Item 81) — horizon-line pixel pack for image-processing consumers.

**Weapons Stores status accessors:** `WeaponsStore::status_raw` packs
ST 0601.19 §Table 21 (General Status, low 8 bits) and §Table 22
(Engagement Status flags, next 4 bits) into one BER-OID field. Use
`general_status()`/`fuze_enabled()`/`laser_enabled()`/`target_enabled()`/
`weapon_armed()` (Python: the equivalent `@property`s) rather than
hand-masking — the bit layout is an implementation detail these
accessors already encode.

**SDCC-FLP is not ST-0601-specific:** `klv::st1010` is a standalone
MISB ST 1010.3 pack decoder usable by any parent document. See
[guides/klv.md](/docs/guides/klv.md#sdcc-error-covariance-st-1010) for
the wire-mode coverage (Mode 1 vs Mode 2) and the encode side
(`encode_sdcc_flp_mode2`).
