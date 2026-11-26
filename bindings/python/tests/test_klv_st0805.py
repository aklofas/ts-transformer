"""ST 0805.1 KLV -> Cursor-on-Target (CoT) conversion — Python wrap tests.

Fixture values are ported verbatim from the Rust `crates/tst-core/src/
klv/st0805.rs::tests::fixture()` hand-built fixture, so the expected
golden values asserted here are already proven correct at the Rust
layer — these tests exercise the Python <-> Rust marshaling (`UasDatalinkLs`
extraction, `CotConfig` translation, error mapping), not the CoT mapping
logic itself."""

import pytest

from tstrans.klv import (
    CotConfig,
    UasDatalinkLs,
    platform_position_xml,
    platform_uid,
    sensor_point_of_interest_xml,
    spi_uid,
)

GENERATED_US = 798_039_895_000_000


def _fixture(**overrides: object) -> UasDatalinkLs:
    """Mirrors the Rust `fixture()` helper in st0805.rs."""
    defaults: dict[str, object] = dict(
        timestamp_us=798_039_894_000_000,
        platform_designation="PRED01",
        mission_id="M05",
        image_source_sensor="EO",
        sensor_lat_deg=34.05,
        sensor_lon_deg=-118.25,
        sensor_ellipsoid_height_m=1524.0,  # HAE-native -> no geoid needed
        platform_heading_deg=90.0,
        sensor_rel_az_deg=300.0,  # 90+300 = 390 -> azimuth 30.0
        sensor_hfov_deg=2.5,
        sensor_vfov_deg=1.9,
        slant_range_m=12_000.0,
        target_location_lat_deg=34.10,  # SPI prefers 40/41 over 23/24
        target_location_lon_deg=-118.20,
        target_location_elev_m=250.0,  # MSL, no undulation set -> as-is
        target_error_ce90_m=425.215152,
        target_error_le90_m=608.9231,
    )
    defaults.update(overrides)
    return UasDatalinkLs(**defaults)


# ---------------------------------------------------------------------------
# uid determinism
# ---------------------------------------------------------------------------


def test_platform_uid_is_deterministic_concatenation():
    assert platform_uid(_fixture()) == "PRED01_M05"


def test_spi_uid_is_deterministic_concatenation():
    assert spi_uid(_fixture()) == "PRED01_M05_EO"


def test_platform_uid_missing_mission_id_raises_value_error():
    record = _fixture(mission_id=None)
    with pytest.raises(ValueError, match="tag 3"):
        platform_uid(record)


def test_spi_uid_missing_image_source_sensor_raises_value_error():
    record = _fixture(image_source_sensor=None)
    with pytest.raises(ValueError, match="tag 11"):
        spi_uid(record)


# ---------------------------------------------------------------------------
# golden XML (defaults)
# ---------------------------------------------------------------------------


def test_platform_position_golden():
    xml = platform_position_xml(_fixture(), generated_us=GENERATED_US)
    assert 'uid="PRED01_M05"' in xml
    assert 'type="a-f-A-M-F"' in xml
    assert 'stale="1995-04-16T13:44:59.000000Z"' in xml
    assert 'hae="1524"' in xml
    assert 'ce="9999999"' in xml
    assert 'le="9999999"' in xml
    assert '<sensor azimuth="30" fov="2.5" vfov="1.9" model="EO" range="12000"/>' in xml


def test_spi_golden_with_ce_le_divisors_and_link():
    xml = sensor_point_of_interest_xml(_fixture(), generated_us=GENERATED_US)
    assert 'type="b-m-p-s-p-i"' in xml
    assert 'uid="PRED01_M05_EO"' in xml
    assert 'ce="198.14312' in xml
    assert 'le="370.16601' in xml
    assert '<link relation="p-p" type="a-f-A-M-F" uid="PRED01_M05"/>' in xml


def test_platform_position_missing_timestamp_raises_value_error():
    record = _fixture(timestamp_us=None)
    with pytest.raises(ValueError, match="tag 2"):
        platform_position_xml(record, generated_us=GENERATED_US)


# ---------------------------------------------------------------------------
# CotConfig marshaling (Python -> Rust field extraction)
# ---------------------------------------------------------------------------


def test_custom_config_overrides_platform_type_and_producer():
    cfg = CotConfig(platform_type="a-f-G-U-C", producer="MyOrg")
    xml = platform_position_xml(_fixture(), config=cfg, generated_us=GENERATED_US)
    assert 'type="a-f-G-U-C"' in xml
    assert "<_flow-tags_ MyOrg=" in xml


def test_custom_config_geoid_undulation_marshals_to_msl_altitude():
    # MSL-only record (no HAE-native tag 75/104) + a configured geoid
    # undulation: hae = msl + undulation, marshaled through CotConfig.
    record = _fixture(sensor_ellipsoid_height_m=None, sensor_alt_m=1500.0)
    cfg = CotConfig(geoid_undulation_m=24.5)
    xml = platform_position_xml(record, config=cfg, generated_us=GENERATED_US)
    assert 'hae="1524.5"' in xml


# ---------------------------------------------------------------------------
# Well-formed XML: refused characters, the producer name, TAB / LF / CR
# ---------------------------------------------------------------------------

_CONVERTERS = (platform_position_xml, sensor_point_of_interest_xml)


def _xml_forbidden(c: str) -> bool:
    cp = ord(c)
    return cp <= 0x8 or cp in (0xB, 0xC) or 0xE <= cp <= 0x1F or cp in (0xFFFE, 0xFFFF)


def _parse(xml: str) -> dict[str, dict[str, str]]:
    """Parses `xml` plainly and namespace-aware; returns each element's attributes."""
    import xml.parsers.expat

    seen: dict[str, dict[str, str]] = {}

    def start(name: str, attrs: dict[str, str]) -> None:
        seen[name] = attrs

    plain = xml.parsers.expat.ParserCreate()
    plain.StartElementHandler = start
    plain.Parse(xml.encode("utf-8"), True)
    names: list[str] = []
    ns = xml.parsers.expat.ParserCreate(namespace_separator="|")
    ns.StartElementHandler = lambda name, attrs: names.append(name)
    ns.Parse(xml.encode("utf-8"), True)
    assert "_flow-tags_" in names, names
    return seen


@pytest.mark.parametrize(
    ("overrides", "field"),
    [
        ({"mission_id": "M\0"}, "Mission ID"),
        ({"mission_id": "A\x01"}, "Mission ID"),
        ({"mission_id": "x￾"}, "Mission ID"),
        ({"platform_designation": "P￿"}, "Platform Designation"),
        ({"image_source_sensor": "E\x1bO"}, "Image Source Sensor"),
    ],
)
def test_cot_rejects_xml_illegal_chars(overrides: dict[str, object], field: str):
    record = _fixture(**overrides)
    for convert in _CONVERTERS:
        with pytest.raises(ValueError, match=f"^{field} holds a character that XML 1.0 does not allow$"):
            convert(record, generated_us=GENERATED_US)
    cfg = CotConfig(platform_type="a-f-\x02")
    for convert in _CONVERTERS:
        with pytest.raises(ValueError, match="^platform_type holds"):
            convert(_fixture(), config=cfg, generated_us=GENERATED_US)


@pytest.mark.parametrize(
    "producer",
    ["bad name", "1abc", "", "ns:tag", 'x="1"/><evil a', "xmlns", "XMLfoo", "a퟿b", "⁰", "a·b"],
)
def test_cot_rejects_a_producer_that_is_not_an_xml_name(producer: str):
    cfg = CotConfig(producer=producer)
    for convert in _CONVERTERS:
        with pytest.raises(ValueError, match="producer name is not an allowed XML name"):
            convert(_fixture(), config=cfg, generated_us=GENERATED_US)


def test_cot_output_parses():
    # Every event a converter returns parses, plainly and namespace-aware, and
    # gives back the value that went in, TAB / LF / CR included.
    probes = [chr(cp) for cp in range(0xA1)] + ["퟿", "", "�", "￾", "￿", "\U00010000", "\U0010ffff"]
    returned = 0
    for c in probes:
        value = f"a{c}b"
        for name, overrides, cfg in (
            ("mission_id", {"mission_id": value}, CotConfig()),
            ("platform_designation", {"platform_designation": value}, CotConfig()),
            ("image_source_sensor", {"image_source_sensor": value}, CotConfig()),
            ("how", {}, CotConfig(how=value)),
            ("platform_type", {}, CotConfig(platform_type=value)),
        ):
            for convert in _CONVERTERS:
                try:
                    xml = convert(_fixture(**overrides), config=cfg, generated_us=GENERATED_US)
                except ValueError:
                    assert _xml_forbidden(c), f"U+{ord(c):04X} in {name}"
                    continue
                assert not _xml_forbidden(c), f"U+{ord(c):04X} in {name} returned"
                attrs = _parse(xml)
                returned += 1
                if name == "mission_id":
                    assert attrs["event"]["uid"].startswith(f"PRED01_{value}")
                if name == "how":
                    assert attrs["event"]["how"] == value
    for producer in ("ST0601CoT", "_x-1.2"):
        for convert in _CONVERTERS:
            attrs = _parse(convert(_fixture(), config=CotConfig(producer=producer), generated_us=GENERATED_US))
            assert producer in attrs["_flow-tags_"]
    assert returned > 0


def test_cot_keeps_tab_lf_cr_through_a_parser():
    record = _fixture(mission_id="a\tb\nc\rd")
    attrs = _parse(platform_position_xml(record, generated_us=GENERATED_US))
    assert attrs["event"]["uid"] == "PRED01_a\tb\nc\rd"
    assert platform_uid(record) == "PRED01_a\tb\nc\rd"
