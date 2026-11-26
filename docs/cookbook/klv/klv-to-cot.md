# Converting ST 0601 to Cursor-on-Target

> **When to use this:** Downstream consumers (ATAK, WinTAK, other TAK-family clients) speak Cursor-on-Target, not KLV — you have a decoded ST 0601 record and need both the platform's own position and its sensor's ground aimpoint as CoT XML.

> **Related:**
> - [guides/klv.md](/docs/guides/klv.md) — the `uid` linkage rule, the `generated_us`/determinism contract, and the altitude-source precedence table (Platform Position vs Sensor Point of Interest differ)
> - [Decode ST 0601 from a captured `.klv` blob](/docs/cookbook/klv/decode-st0601-blob.md) — get a `UasDatalinkLs` first

MISB ST 0805.1 defines a one-way KLV→CoT conversion: `klv::st0805`
turns a decoded `UasDatalinkLs` into two independent CoT XML events —
**Platform Position** (the platform's own position) and **Sensor
Point of Interest** (SPI, the sensor's ground aimpoint) — linked by
`uid`. Both come from the same source record; produce both if your
consumer wants the aimpoint plotted separately from the platform.

```rust,no_run
use tst_core::klv::st0601::UasDatalinkLs;
use tst_core::klv::st0805::{
    CotConfig, platform_position_xml, sensor_point_of_interest_xml,
};

fn to_cot(
    ls: &UasDatalinkLs,
    generated_us: u64,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    // Override platform_type per your platform (rotary-wing, manned
    // pod, ...) — the spec's "a-f-A-M-F" default is a fixed-wing example.
    let cfg = CotConfig {
        platform_type: "a-f-A-M-F".to_string(),
        producer: "MyGroundStation".to_string(), // an XML attribute *name*
        ..CotConfig::default()
    };

    // Platform Position: uid = "{tag10}_{tag3}" (platform_uid).
    let platform_xml = platform_position_xml(ls, &cfg, generated_us)?;

    // Sensor Point of Interest: uid = "{tag10}_{tag3}_{tag11}" (spi_uid),
    // and its <link> element embeds the Platform Position uid so a
    // consumer can join the two events.
    let spi_xml = sensor_point_of_interest_xml(ls, &cfg, generated_us)?;

    Ok((platform_xml, spi_xml))
}
```

```python
import time
from tstrans.klv import CotConfig, platform_position_xml, sensor_point_of_interest_xml

cfg = CotConfig(
    platform_type="a-f-A-M-F",
    producer="MyGroundStation",  # an XML attribute *name*
)
generated_us = int(time.time() * 1_000_000)

# record: a decoded UasDatalinkLs — see the Related recipe. Config and
# generated_us are keyword-only.
platform_xml = platform_position_xml(record, config=cfg, generated_us=generated_us)
spi_xml = sensor_point_of_interest_xml(record, config=cfg, generated_us=generated_us)
```

## Notes

**`uid` linkage, not a shared timestamp:** the two events are separate
CoT messages, each with its own `uid`. The SPI event's `<link
relation="p-p" .../>` element carries the Platform Position `uid` —
that's the join key a consumer uses to associate the sensor aimpoint
with the platform that reported it. Call `platform_uid(ls)` /
`spi_uid(ls)` directly if you only need the id strings (e.g. to key a
lookup table) without generating XML.

**`CotConfig.producer` is an XML attribute name, not a value:** it must
be an ASCII letter or `_` followed by letters, digits, `_`, `.` or `-`,
and must not start with `xml` in any case. Any other value is refused
with `CotError::InvalidProducerName` (Python `ValueError`, JVM
`IllegalArgumentException`); the default `ST0601CoT` passes.

**Every document parses:** a string from the record or the
configuration that holds a character XML 1.0 does not allow (U+0000 to
U+001F except TAB, LF and CR; U+FFFE; U+FFFF) is refused with
`CotError::InvalidXmlChar { field }`, which names the item ("Mission
ID", …) or the configuration field. TAB, LF and CR are written as
character references and survive an XML parser. The KLV decoders keep
control characters in strings, so clean them first if your producer
sends them.

**Determinism / replay:** `generated_us` is a caller-supplied argument,
never sampled internally, and attribute order in the emitted XML is
fixed. Converting the same `UasDatalinkLs` + `CotConfig` +
`generated_us` twice — whether live or replayed from a captured file —
produces byte-identical XML. This is a deliberate ST 0805.1 §1
requirement, not an implementation accident; don't route the output
through an XML formatter/serializer that could reorder attributes
before it reaches the wire.

**Altitude source differs by event** (Platform Position prefers the
HAE-native extended tag, SPI prefers Target Location over Frame
Center) — see the precedence table in
[guides/klv.md](/docs/guides/klv.md#cursor-on-target-st-0805) rather
than duplicating it here. An item sent as "no value" (Item 104 as an
IMAPB special) ends the chain: the Platform Position event is refused
with `MissingField { tag: 104 }`, not taken from Item 75.

**An MSL height without an undulation:** with no
`CotConfig::geoid_undulation_m`, an MSL height (Item 15; Item 42 or 25
for the aimpoint) is written as the HAE as it is. To refuse instead,
call `platform_position_xml_with` / `sensor_point_of_interest_xml_with`
with `MslFallback::Refuse` (Python `require_hae=True`, JVM the
`requireHae` overloads): such an event fails with
`CotError::MslWithoutUndulation { tag }`.

**Missing fields fail closed:** both functions return
`CotError::MissingField { tag, name }` (Rust) / raise the mapped
binding exception when a KLV tag the mapping requires — `uid`
components, timestamp, position, altitude — is absent from the record.
There is no partial-XML output; catch the error and skip or log the
frame.
