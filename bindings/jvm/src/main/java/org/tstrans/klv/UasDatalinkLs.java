package org.tstrans.klv;

import java.nio.ByteBuffer;
import java.util.Collections;
import java.util.List;
import java.util.Optional;

/**
 * MISB ST 0601 UAS Datalink Local Set typed view.
 *
 * <p>Mirror of the 147-field Rust {@code tst_core::klv::st0601::UasDatalinkLs}
 * flat struct. Composite views (sensor position, attitude, FOV, corners) are
 * accessor methods that return {@link Optional#empty()} when any of the
 * underlying primitive fields is absent.
 *
 * <p>{@link #securityLocalSet()} carries the Tag 48 ST 0102 LS body bytes
 * (no UL prefix); call {@link Klv#decodeSecurity(byte[])} for typed access.
 *
 * <p>{@link #vmti()} carries the Tag 74 ST 0903 LS body bytes (no UL prefix);
 * call {@link Klv#decodeVmti(byte[])} for typed access.
 *
 * <p>{@code unknown} preserves any tag not in the typed-modeled set per
 * ST 0107.5 §6 future-proof skip rule. {@code fieldErrors} collects per-field
 * decode failures from lenient mode; strict mode raises
 * {@link org.tstrans.KlvDecodeException} instead.
 *
 * <p>Decode via {@link Klv#decodeUasDatalink(byte[])}; encode via
 * {@link Klv#encodeUasDatalink(UasDatalinkLs)}.
 *
 * <h2>Encode-enforced ranges (MISB ST 0601.19)</h2>
 *
 * <p>Values outside the listed range cause {@code KlvEncodeException} on
 * {@link Klv#encodeUasDatalink}. Narrow/full twins: pitch 6&harr;90,
 * roll 7&harr;91, corner offsets 26&ndash;33&harr;82&ndash;89 (absolute).
 *
 * <pre>
 * Platform state
 *   platformHeadingDeg          Tag  5  [0, 360] deg
 *   platformPitchDeg            Tag  6  [-20, 20] deg      narrow; full twin: platformPitchFullDeg (Tag 90)
 *   platformRollDeg             Tag  7  [-50, 50] deg      narrow; full twin: platformRollFullDeg (Tag 91)
 *   platformTrueAirspeed        Tag  8  [0, 255] m/s
 *   platformIndicatedAirspeed   Tag  9  [0, 255] m/s
 *   platformPitchFullDeg        Tag 90  [-90, 90] deg      full twin of platformPitchDeg (Tag 6)
 *   platformRollFullDeg         Tag 91  [-90, 90] deg      full twin of platformRollDeg (Tag 7)
 *   platformAngleOfAttackDeg    Tag 50  [-20, 20] deg
 *
 * Sensor pose &amp; position
 *   sensorLatDeg                Tag 13  [-90, 90] deg
 *   sensorLonDeg                Tag 14  [-180, 180] deg
 *   sensorAltM                  Tag 15  [-900, 19000] m
 *   sensorEllipsoidHeightM      Tag 75  [-900, 19000] m  extended twin: sensorEllipsoidHeightExtendedM (Tag 104, IMAPB)
 *   sensorHfovDeg               Tag 16  [0, 180] deg
 *   sensorVfovDeg               Tag 17  [0, 180] deg
 *   sensorRelAzDeg              Tag 18  [0, 360] deg
 *   sensorRelElDeg              Tag 19  [-180, 180] deg
 *   sensorRelRollDeg            Tag 20  [0, 360] deg
 *
 * Ranging &amp; frame center
 *   slantRangeM                 Tag 21  [0, 5000000] m
 *   targetWidthM                Tag 22  [0, 10000] m  extended twin: targetWidthExtendedM (Tag 96, IMAPB)
 *   frameCenterLatDeg           Tag 23  [-90, 90] deg
 *   frameCenterLonDeg           Tag 24  [-180, 180] deg
 *   frameCenterElevM            Tag 25  [-900, 19000] m
 *   frameCenterEllipsoidHeightM Tag 78  [-900, 19000] m
 *
 * Corner offsets (narrow +-0.075 deg)
 *   cornerLatOffsetP1Deg        Tag 26  [-0.075, 0.075] deg  full twin: cornerLatP1Deg (Tag 82)
 *   cornerLonOffsetP1Deg        Tag 27  [-0.075, 0.075] deg  full twin: cornerLonP1Deg (Tag 83)
 *   cornerLatOffsetP2Deg        Tag 28  [-0.075, 0.075] deg  full twin: cornerLatP2Deg (Tag 84)
 *   cornerLonOffsetP2Deg        Tag 29  [-0.075, 0.075] deg  full twin: cornerLonP2Deg (Tag 85)
 *   cornerLatOffsetP3Deg        Tag 30  [-0.075, 0.075] deg  full twin: cornerLatP3Deg (Tag 86)
 *   cornerLonOffsetP3Deg        Tag 31  [-0.075, 0.075] deg  full twin: cornerLonP3Deg (Tag 87)
 *   cornerLatOffsetP4Deg        Tag 32  [-0.075, 0.075] deg  full twin: cornerLatP4Deg (Tag 88)
 *   cornerLonOffsetP4Deg        Tag 33  [-0.075, 0.075] deg  full twin: cornerLonP4Deg (Tag 89)
 *
 * Corner full lat/lon (ST 0601.13+)
 *   cornerLatP1Deg              Tag 82  [-90, 90] deg      full twin of cornerLatOffsetP1Deg (Tag 26)
 *   cornerLonP1Deg              Tag 83  [-180, 180] deg    full twin of cornerLonOffsetP1Deg (Tag 27)
 *   cornerLatP2Deg              Tag 84  [-90, 90] deg      full twin of cornerLatOffsetP2Deg (Tag 28)
 *   cornerLonP2Deg              Tag 85  [-180, 180] deg    full twin of cornerLonOffsetP2Deg (Tag 29)
 *   cornerLatP3Deg              Tag 86  [-90, 90] deg      full twin of cornerLatOffsetP3Deg (Tag 30)
 *   cornerLonP3Deg              Tag 87  [-180, 180] deg    full twin of cornerLonOffsetP3Deg (Tag 31)
 *   cornerLatP4Deg              Tag 88  [-90, 90] deg      full twin of cornerLatOffsetP4Deg (Tag 32)
 *   cornerLonP4Deg              Tag 89  [-180, 180] deg    full twin of cornerLonOffsetP4Deg (Tag 33)
 *
 * Target location &amp; tracking
 *   targetLocationLatDeg        Tag 40  [-90, 90] deg
 *   targetLocationLonDeg        Tag 41  [-180, 180] deg
 *   targetLocationElevM         Tag 42  [-900, 19000] m
 *   targetTrackGateWidthPx      Tag 43  [0, 510] px
 *   targetTrackGateHeightPx     Tag 44  [0, 510] px
 *   targetErrorCe90M            Tag 45  [0, 4095] m
 *   targetErrorLe90M            Tag 46  [0, 4095] m
 *
 * Weather / atmospheric
 *   windDirectionDeg            Tag 35  [0, 360] deg
 *   windSpeed                   Tag 36  [0, 100] m/s
 *   staticPressureMbar          Tag 37  [0, 5000] mbar
 *   densityAltitudeM            Tag 38  [-900, 19000] m  extended twin: densityAltitudeExtendedM (Tag 103, IMAPB)
 *   differentialPressureMbar    Tag 49  [0, 5000] mbar
 *   airfieldBarometricPressureMbar Tag 53  [0, 5000] mbar
 *   airfieldElevationM          Tag 54  [-900, 19000] m
 *   relativeHumidityPct         Tag 55  [0, 100] %
 *
 * Extended platform state
 *   platformVerticalSpeed       Tag 51  [-180, 180] m/s
 *   platformSideslipDeg         Tag 52  [-20, 20] deg    narrow; full twin: platformSideslipFullDeg (Tag 93)
 *   platformGroundSpeed         Tag 56  [0, 255] m/s
 *   groundRangeM                Tag 57  [0, 5000000] m
 *   platformFuelRemainingKg     Tag 58  [0, 10000] kg
 *   platformMagneticHeadingDeg  Tag 64  [0, 360] deg
 *   platformAngleOfAttackFullDeg Tag 92  [-90, 90] deg
 *   platformSideslipFullDeg     Tag 93  [-180, 180] deg  full twin of platformSideslipDeg (Tag 52)
 *
 * Alternate platform
 *   alternatePlatformLatDeg     Tag 67  [-90, 90] deg
 *   alternatePlatformLonDeg     Tag 68  [-180, 180] deg
 *   alternatePlatformAltM       Tag 69  [-900, 19000] m  not to be confused with Item 105 (extends Tag 76); no IMAPB twin of its own
 *   alternatePlatformHeadingDeg Tag 71  [0, 360] deg
 *   alternatePlatformEllipsoidHeightM Tag 76  [-900, 19000] m  extended twin: alternatePlatformEllipsoidHeightExtendedM (Tag 105, IMAPB)
 *
 * Sensor velocity
 *   sensorNorthVelocity         Tag 79  [-327, 327] m/s
 *   sensorEastVelocity          Tag 80  [-327, 327] m/s
 *
 * Extended-range items (ST 1201.5 IMAPB)
 *   targetWidthExtendedM        Tag 96  [0, 1500000] m        IMAPB, &le;8 bytes; extended twin of targetWidthM (Tag 22)
 *   densityAltitudeExtendedM    Tag 103  [-900, 40000] m      IMAPB, &le;8 bytes; extended twin of densityAltitudeM (Tag 38)
 *   sensorEllipsoidHeightExtendedM Tag 104  [-900, 40000] m      IMAPB, &le;8 bytes; extended twin of sensorEllipsoidHeightM (Tag 75)
 *   alternatePlatformEllipsoidHeightExtendedM Tag 105  [-900, 40000] m      IMAPB, &le;8 bytes; extended twin of alternatePlatformEllipsoidHeightM (Tag 76)
 *   rangeToRecoveryKm           Tag 109  [0, 21000] km        IMAPB, &le;4 bytes
 *   platformCourseAngleDeg      Tag 112  [0, 360] deg         IMAPB, &le;8 bytes
 *   altitudeAglM                Tag 113  [-900, 40000] m      IMAPB, &le;4 bytes
 *   radarAltimeterM             Tag 114  [-900, 40000] m      IMAPB, &le;4 bytes
 *   sensorAzimuthRateDps        Tag 117  [-1000, 1000] deg/s  IMAPB, &le;4 bytes
 *   sensorElevationRateDps      Tag 118  [-1000, 1000] deg/s  IMAPB, &le;4 bytes
 *   sensorRollRateDps           Tag 119  [-1000, 1000] deg/s  IMAPB, &le;4 bytes
 *   miStoragePercentFull        Tag 120  [0, 100] %           IMAPB, &le;3 bytes
 *   transmissionFrequencyMhz    Tag 132  [1, 99999] MHz       IMAPB, &le;4 bytes
 *   zoomPercentage              Tag 134  [0, 100] %           IMAPB, &le;4 bytes
 * </pre>
 */
public record UasDatalinkLs(
        // Non-optional identity fields (always present on the wire after decode)
        ByteBuffer universalLabel,
        int declaredVersion,

        // Identity
        String missionId,
        String platformTailNumber,
        String platformDesignation,
        String imageSourceSensor,
        String imageCoordinateSystem,
        String platformCallSign,
        Integer uasLsVersion,

        // Time
        Long timestampUs,

        // Platform state
        Double platformHeadingDeg,
        Double platformPitchDeg,
        Double platformRollDeg,
        Double platformTrueAirspeed,
        Double platformIndicatedAirspeed,
        Double platformPitchFullDeg,
        Double platformRollFullDeg,
        Double platformAngleOfAttackDeg,

        // Sensor pose & position
        Double sensorLatDeg,
        Double sensorLonDeg,
        Double sensorAltM,
        Double sensorEllipsoidHeightM,
        Double sensorHfovDeg,
        Double sensorVfovDeg,
        Double sensorRelAzDeg,
        Double sensorRelElDeg,
        Double sensorRelRollDeg,

        // Ranging & frame center
        Double slantRangeM,
        Double targetWidthM,
        Double frameCenterLatDeg,
        Double frameCenterLonDeg,
        Double frameCenterElevM,
        Double frameCenterEllipsoidHeightM,

        // Image corner offsets from frame center (tags 26–33)
        Double cornerLatOffsetP1Deg,
        Double cornerLonOffsetP1Deg,
        Double cornerLatOffsetP2Deg,
        Double cornerLonOffsetP2Deg,
        Double cornerLatOffsetP3Deg,
        Double cornerLonOffsetP3Deg,
        Double cornerLatOffsetP4Deg,
        Double cornerLonOffsetP4Deg,

        // Image corners — full lat/lon (tags 82–89, ST 0601.13+)
        Double cornerLatP1Deg,
        Double cornerLonP1Deg,
        Double cornerLatP2Deg,
        Double cornerLonP2Deg,
        Double cornerLatP3Deg,
        Double cornerLonP3Deg,
        Double cornerLatP4Deg,
        Double cornerLonP4Deg,

        // Misc
        Integer genericFlagData,
        ByteBuffer securityLocalSet,
        ByteBuffer vmti,

        /**
         * Tag 94 — MIIS Core Identifier raw bytes (ST 0601.19 §8.94 / ST 1204.3).
         * {@code null} when absent. Call {@link Klv#decodeCoreId(byte[])} to parse
         * these bytes into a typed {@link CoreId}.
         */
        byte[] miisCoreId,

        // ---------------------------------------------------------------
        // Ranged f64 fields (tags 35-93 subset, 30 fields)
        // ---------------------------------------------------------------
        Double targetLocationLatDeg,
        Double targetLocationLonDeg,
        Double targetLocationElevM,
        Double targetTrackGateWidthPx,
        Double targetTrackGateHeightPx,
        Double targetErrorCe90M,
        Double targetErrorLe90M,
        Double windDirectionDeg,
        Double windSpeed,
        Double staticPressureMbar,
        Double densityAltitudeM,
        Double differentialPressureMbar,
        Double airfieldBarometricPressureMbar,
        Double airfieldElevationM,
        Double relativeHumidityPct,
        Double platformVerticalSpeed,
        Double platformSideslipDeg,
        Double platformGroundSpeed,
        Double groundRangeM,
        Double platformFuelRemainingKg,
        Double platformMagneticHeadingDeg,
        Double platformAngleOfAttackFullDeg,
        Double platformSideslipFullDeg,
        Double alternatePlatformLatDeg,
        Double alternatePlatformLonDeg,
        Double alternatePlatformAltM,
        Double alternatePlatformHeadingDeg,
        Double alternatePlatformEllipsoidHeightM,
        Double sensorNorthVelocity,
        Double sensorEastVelocity,

        // ---------------------------------------------------------------
        // IMAPB (ST 1201.5) f64 fields, extended-range
        // twins of the narrower ranged-f64 items (tags 96, 103-105,
        // 109, 112-114, 117-120, 132, 134). Decode accepts any wire length
        // 1..=max_len; encode emits the tag's default_len.
        // ---------------------------------------------------------------
        Double targetWidthExtendedM,
        Double densityAltitudeExtendedM,
        Double sensorEllipsoidHeightExtendedM,
        Double alternatePlatformEllipsoidHeightExtendedM,
        Double rangeToRecoveryKm,
        Double platformCourseAngleDeg,
        Double altitudeAglM,
        Double radarAltimeterM,
        Double sensorAzimuthRateDps,
        Double sensorElevationRateDps,
        Double sensorRollRateDps,
        Double miStoragePercentFull,
        Double transmissionFrequencyMhz,
        Double zoomPercentage,

        // ---------------------------------------------------------------
        // MISB variable-length truncatable int/enum fields
        // (tags 110-139). u32 fields cross as {@code Long} (Java has no
        // unsigned int); u8/enum fields cross as {@code Integer}.
        // ---------------------------------------------------------------
        Long timeAirborneS,
        Long propulsionUnitSpeedRpm,
        Integer navsatsInView,
        /**
         * Tag 124 — Positioning Method Source, bitfield (bit 0 INS, bit 1
         * GPS, bit 2 Galileo, bit 3 QZSS, bit 4 NAVIC, bit 5 GLONASS,
         * bit 6 BeiDou-1, bit 7 BeiDou-2). Kept as a raw opaque codepoint,
         * not a sub-struct — callers needing individual bits shift and
         * mask this value themselves.
         */
        Integer positioningMethodSource,
        Integer platformStatusCode,
        Integer sensorControlModeCode,
        Long takeOffTimeUs,
        Long miStorageCapacityGb,
        Integer leapSeconds,
        Long correctionOffsetUs,
        /**
         * Tag 139 — Active Payloads, bitmask of active Payload IDs
         * (LSB-first within each byte; multi-byte extends upward). Hand-unpack
         * the bits; no typed accessor iterator on this binding (mirrors tst-py,
         * which also left it unmirrored).
         */
        ByteBuffer activePayloads,

        // ---------------------------------------------------------------
        // Named nested-set raw byte fields (tags 73/95/97-101)
        // ---------------------------------------------------------------
        ByteBuffer rvt,
        ByteBuffer sarMiLocalSet,
        ByteBuffer rangeImageLocalSet,
        ByteBuffer geoRegistrationLocalSet,
        ByteBuffer compositeImagingLocalSet,
        ByteBuffer segmentLocalSet,
        ByteBuffer amendLocalSet,

        // ---------------------------------------------------------------
        // Raw/simple scalar + string fields (11 fields)
        // ---------------------------------------------------------------
        Integer outsideAirTempC,
        /**
         * Tag 60 — Weapon Load, bit-packed nibbles (high&rarr;low): store
         * station, store substation, weapon type, weapon variant. Kept as a
         * raw opaque codepoint, not a sub-struct — callers needing individual
         * nibbles shift and mask this value themselves.
         */
        Integer weaponLoad,
        Integer weaponFired,
        Integer laserPrfCode,
        String alternatePlatformName,
        Long eventStartTimeUs,
        String streamDesignator,
        String operationalBase,
        String broadcastSource,
        String targetId,
        String communicationsMethod,

        // ---------------------------------------------------------------
        // Coded enums (tags 34/63/77), raw-codepoint boxed
        // Integer record components (authoritative representation); use the
        // typed accessors below ({@link #icingDetected()} etc.) for the
        // enum view — null for both "absent" and "wire-unknown".
        // ---------------------------------------------------------------
        Integer icingDetectedCode,
        Integer sensorFovNameCode,
        Integer operationalModeCode,

        // ---------------------------------------------------------------
        // Pack & list items (tags 81/102/115/116/121/122/
        // 127/128/130/138/140/141/142/143)
        // ---------------------------------------------------------------
        /** Item 81: Image Horizon Pixels. */
        ImageHorizonPixels imageHorizon,
        /**
         * Item 115: Control Command — MULTI-INSTANCE (ST 0601.19 Table 1
         * "Multiples Allowed" = Yes); every wire occurrence appends one
         * {@link ControlCommand}. Empty means none were present.
         */
        List<ControlCommand> controlCommands,
        /**
         * Item 116: Control Command Verification List — BER-OID command
         * ids the platform has acknowledged/verified, or {@code null} if
         * the tag was absent.
         */
        List<Long> controlCommandVerification,
        /**
         * Item 121: Active Wavelength List — BER-OID wavelength ids
         * currently active ({@code 0} = none active, exclusive per
         * ST 0601.19 §8.121), or {@code null} if the tag was absent.
         */
        List<Long> activeWavelengths,
        /** Item 127: Sensor Frame Rate Pack. */
        SensorFrameRate sensorFrameRate,
        /** Item 143: Metadata Substream Id. */
        MetadataSubstreamId metadataSubstreamId,
        /** Item 122: Country Codes. */
        CountryCodes countryCodes,
        /** Item 128: Wavelengths List, or {@code null} if the tag was absent. */
        List<WavelengthRecord> wavelengthsList,
        /** Item 130: Airbase Locations. */
        AirbaseLocations airbaseLocations,
        /** Item 138: Payload List. */
        PayloadList payloadList,
        /** Item 140: Weapons Stores, or {@code null} if the tag was absent. */
        List<WeaponsStore> weaponsStores,
        /** Item 141: Waypoint List, or {@code null} if the tag was absent. */
        List<Waypoint> waypointList,
        /** Item 142: View Domain. */
        ViewDomain viewDomain,
        /**
         * Item 102: SDCC-FLP (Standard Deviation and Correlation
         * Coefficient, Floating-Point). MULTI-INSTANCE per ST 0601.19
         * Table 1 ("Multiples Allowed" = Yes) — the "Refined Source List"
         * binding: every wire occurrence appends one {@link SdccFlpField},
         * capturing which preceding items it refines.
         */
        List<SdccFlpField> sdccFlps,

        // Pass-through
        List<KlvUnknownField> unknown,
        List<KlvFieldError> fieldErrors,

        /**
         * Tags whose wire value was the INT_MIN sentinel for their signed
         * linear mapping. INT_MIN is a spec-defined signal, not an error;
         * the corresponding typed field is {@code null} and the tag number
         * is recorded here. Consult the special-value assignments table in
         * ST 0601.19 to look up the spec-defined meaning for each tag number.
         */
        List<Long> sentinelTags,

        /**
         * Tags whose ST 1201.5 IMAPB wire value decoded to a spec-defined
         * special (§7.2.3 — infinities, NaN families, or the MISB
         * BelowMin/AboveMax/user-defined overflow signals) rather than a
         * normal-range float. The corresponding typed {@code Double} field
         * is {@code null} and the tag+special pair is recorded here
         * instead. Mirrors {@link #sentinelTags}: encoding a record
         * re-emits each entry's special bytes when its typed field is
         * still {@code null}; a non-null typed field always wins.
         */
        List<ImapbSpecialEntry> imapbSpecials
) implements KlvSet {

    /**
     * Compact constructor — validates {@code universalLabel} is exactly 16 bytes
     * and that the lists are non-null.
     */
    public UasDatalinkLs {
        if (universalLabel == null || universalLabel.capacity() != 16) {
            int cap = universalLabel == null ? 0 : universalLabel.capacity();
            throw new IllegalArgumentException(
                    "UasDatalinkLs.universalLabel must be exactly 16 bytes; got " + cap);
        }
        unknown = unknown == null ? Collections.emptyList() : Collections.unmodifiableList(unknown);
        fieldErrors = fieldErrors == null ? Collections.emptyList() : Collections.unmodifiableList(fieldErrors);
        sentinelTags = sentinelTags == null ? Collections.emptyList() : Collections.unmodifiableList(sentinelTags);
        imapbSpecials = imapbSpecials == null ? Collections.emptyList() : Collections.unmodifiableList(imapbSpecials);
        controlCommands = controlCommands == null
                ? Collections.emptyList() : Collections.unmodifiableList(controlCommands);
        sdccFlps = sdccFlps == null ? Collections.emptyList() : Collections.unmodifiableList(sdccFlps);
    }

    // -----------------------------------------------------------------------
    // Composite accessor methods
    // -----------------------------------------------------------------------

    /**
     * Return the sensor position when all three underlying fields are present.
     * Mirrors tst-py's {@code sensor_position()}.
     *
     * @return {@link GeoPoint} with lat/lon/alt, or {@link Optional#empty()}
     */
    public Optional<GeoPoint> sensorPosition() {
        if (sensorLatDeg != null && sensorLonDeg != null && sensorAltM != null) {
            return Optional.of(new GeoPoint(sensorLatDeg, sensorLonDeg, sensorAltM));
        }
        return Optional.empty();
    }

    /**
     * Return the sensor relative attitude when all three underlying fields are present.
     * Uses sensor relative azimuth/elevation/roll (tags 18/19/20). Mirrors tst-py's
     * {@code sensor_attitude()}.
     *
     * @return {@link Attitude} with heading/pitch/roll in degrees, or empty
     */
    public Optional<Attitude> sensorAttitude() {
        if (sensorRelAzDeg != null && sensorRelElDeg != null && sensorRelRollDeg != null) {
            return Optional.of(new Attitude(sensorRelAzDeg, sensorRelElDeg, sensorRelRollDeg));
        }
        return Optional.empty();
    }

    /**
     * Return the sensor field-of-view when both underlying fields are present.
     * Mirrors tst-py's {@code sensor_fov()}.
     *
     * @return {@link FieldOfView} with horizontal/vertical degrees, or empty
     */
    public Optional<FieldOfView> sensorFov() {
        if (sensorHfovDeg != null && sensorVfovDeg != null) {
            return Optional.of(new FieldOfView(sensorHfovDeg, sensorVfovDeg));
        }
        return Optional.empty();
    }

    /**
     * Return the platform attitude when all three underlying fields are present.
     * Uses platform heading/pitch/roll (tags 5/6/7). Mirrors tst-py's
     * {@code platform_attitude()}.
     *
     * @return {@link Attitude} with heading/pitch/roll in degrees, or empty
     */
    public Optional<Attitude> platformAttitude() {
        if (platformHeadingDeg != null && platformPitchDeg != null && platformRollDeg != null) {
            return Optional.of(new Attitude(platformHeadingDeg, platformPitchDeg, platformRollDeg));
        }
        return Optional.empty();
    }

    /**
     * Return the frame center when all three underlying fields are present.
     * Mirrors tst-py's {@code frame_center()}.
     *
     * @return {@link GeoPoint} with lat/lon/elev, or empty
     */
    public Optional<GeoPoint> frameCenter() {
        if (frameCenterLatDeg != null && frameCenterLonDeg != null && frameCenterElevM != null) {
            return Optional.of(new GeoPoint(frameCenterLatDeg, frameCenterLonDeg, frameCenterElevM));
        }
        return Optional.empty();
    }

    /**
     * Return the sensor footprint corners. Prefers the absolute corner tags
     * (82–89) when all eight are present; falls back to frame-center + offset
     * tags (26–33) when all eight offsets and both frame-center lat/lon are
     * present; otherwise returns empty. Mirrors tst-py's {@code corners()}.
     *
     * @return {@link Corners} with four {@link LatLon} points, or empty
     */
    public Optional<Corners> corners() {
        // Prefer absolute (tags 82-89) when fully populated.
        if (cornerLatP1Deg != null && cornerLonP1Deg != null
                && cornerLatP2Deg != null && cornerLonP2Deg != null
                && cornerLatP3Deg != null && cornerLonP3Deg != null
                && cornerLatP4Deg != null && cornerLonP4Deg != null) {
            return Optional.of(new Corners(
                    new LatLon(cornerLatP1Deg, cornerLonP1Deg),
                    new LatLon(cornerLatP2Deg, cornerLonP2Deg),
                    new LatLon(cornerLatP3Deg, cornerLonP3Deg),
                    new LatLon(cornerLatP4Deg, cornerLonP4Deg)
            ));
        }
        // Fall back to offsets + frame center.
        if (frameCenterLatDeg == null || frameCenterLonDeg == null) {
            return Optional.empty();
        }
        if (cornerLatOffsetP1Deg == null || cornerLonOffsetP1Deg == null
                || cornerLatOffsetP2Deg == null || cornerLonOffsetP2Deg == null
                || cornerLatOffsetP3Deg == null || cornerLonOffsetP3Deg == null
                || cornerLatOffsetP4Deg == null || cornerLonOffsetP4Deg == null) {
            return Optional.empty();
        }
        double lat0 = frameCenterLatDeg;
        double lon0 = frameCenterLonDeg;
        return Optional.of(new Corners(
                new LatLon(lat0 + cornerLatOffsetP1Deg, lon0 + cornerLonOffsetP1Deg),
                new LatLon(lat0 + cornerLatOffsetP2Deg, lon0 + cornerLonOffsetP2Deg),
                new LatLon(lat0 + cornerLatOffsetP3Deg, lon0 + cornerLonOffsetP3Deg),
                new LatLon(lat0 + cornerLatOffsetP4Deg, lon0 + cornerLonOffsetP4Deg)
        ));
    }

    // -----------------------------------------------------------------------
    // Coded-enum typed accessors — the raw *Code record components
    // above are authoritative; these are convenience views. Return null for
    // BOTH "absent" (code is null) and "wire-unknown" (code doesn't match a
    // named constant) — inspect the raw *Code() accessor to tell them apart.
    // -----------------------------------------------------------------------

    /** Tag 34 Icing Detected as a typed enum, or {@code null} (absent / wire-unknown). */
    public IcingDetected icingDetected() {
        return icingDetectedCode == null ? null : IcingDetected.fromCode(icingDetectedCode);
    }

    /** Tag 63 Sensor Field of View Name as a typed enum, or {@code null} (absent / wire-unknown). */
    public SensorFovName sensorFovName() {
        return sensorFovNameCode == null ? null : SensorFovName.fromCode(sensorFovNameCode);
    }

    /** Tag 77 Operational Mode as a typed enum, or {@code null} (absent / wire-unknown). */
    public OperationalMode operationalMode() {
        return operationalModeCode == null ? null : OperationalMode.fromCode(operationalModeCode);
    }

    // -----------------------------------------------------------------------
    // Var-length typed enum accessors — same raw-code-is-authoritative
    // convention as the coded-enum accessors above.
    // -----------------------------------------------------------------------

    /** Tag 125 Platform Status as a typed enum, or {@code null} (absent / wire-unknown). */
    public PlatformStatus platformStatus() {
        return platformStatusCode == null ? null : PlatformStatus.fromCode(platformStatusCode);
    }

    /** Tag 126 Sensor Control Mode as a typed enum, or {@code null} (absent / wire-unknown). */
    public SensorControlMode sensorControlMode() {
        return sensorControlModeCode == null ? null : SensorControlMode.fromCode(sensorControlModeCode);
    }

    // -----------------------------------------------------------------------
    // Builder
    // -----------------------------------------------------------------------

    /** Public mutable builder for {@link UasDatalinkLs}. */
    public static final class Builder {
        private ByteBuffer universalLabel;
        private int declaredVersion;
        private String missionId;
        private String platformTailNumber;
        private String platformDesignation;
        private String imageSourceSensor;
        private String imageCoordinateSystem;
        private String platformCallSign;
        private Integer uasLsVersion;
        private Long timestampUs;
        private Double platformHeadingDeg;
        private Double platformPitchDeg;
        private Double platformRollDeg;
        private Double platformTrueAirspeed;
        private Double platformIndicatedAirspeed;
        private Double platformPitchFullDeg;
        private Double platformRollFullDeg;
        private Double platformAngleOfAttackDeg;
        private Double sensorLatDeg;
        private Double sensorLonDeg;
        private Double sensorAltM;
        private Double sensorEllipsoidHeightM;
        private Double sensorHfovDeg;
        private Double sensorVfovDeg;
        private Double sensorRelAzDeg;
        private Double sensorRelElDeg;
        private Double sensorRelRollDeg;
        private Double slantRangeM;
        private Double targetWidthM;
        private Double frameCenterLatDeg;
        private Double frameCenterLonDeg;
        private Double frameCenterElevM;
        private Double frameCenterEllipsoidHeightM;
        private Double cornerLatOffsetP1Deg;
        private Double cornerLonOffsetP1Deg;
        private Double cornerLatOffsetP2Deg;
        private Double cornerLonOffsetP2Deg;
        private Double cornerLatOffsetP3Deg;
        private Double cornerLonOffsetP3Deg;
        private Double cornerLatOffsetP4Deg;
        private Double cornerLonOffsetP4Deg;
        private Double cornerLatP1Deg;
        private Double cornerLonP1Deg;
        private Double cornerLatP2Deg;
        private Double cornerLonP2Deg;
        private Double cornerLatP3Deg;
        private Double cornerLonP3Deg;
        private Double cornerLatP4Deg;
        private Double cornerLonP4Deg;
        private Integer genericFlagData;
        private ByteBuffer securityLocalSet;
        private ByteBuffer vmti;
        private byte[] miisCoreId;

        // Ranged f64 fields
        private Double targetLocationLatDeg;
        private Double targetLocationLonDeg;
        private Double targetLocationElevM;
        private Double targetTrackGateWidthPx;
        private Double targetTrackGateHeightPx;
        private Double targetErrorCe90M;
        private Double targetErrorLe90M;
        private Double windDirectionDeg;
        private Double windSpeed;
        private Double staticPressureMbar;
        private Double densityAltitudeM;
        private Double differentialPressureMbar;
        private Double airfieldBarometricPressureMbar;
        private Double airfieldElevationM;
        private Double relativeHumidityPct;
        private Double platformVerticalSpeed;
        private Double platformSideslipDeg;
        private Double platformGroundSpeed;
        private Double groundRangeM;
        private Double platformFuelRemainingKg;
        private Double platformMagneticHeadingDeg;
        private Double platformAngleOfAttackFullDeg;
        private Double platformSideslipFullDeg;
        private Double alternatePlatformLatDeg;
        private Double alternatePlatformLonDeg;
        private Double alternatePlatformAltM;
        private Double alternatePlatformHeadingDeg;
        private Double alternatePlatformEllipsoidHeightM;
        private Double sensorNorthVelocity;
        private Double sensorEastVelocity;

        // IMAPB f64 fields
        private Double targetWidthExtendedM;
        private Double densityAltitudeExtendedM;
        private Double sensorEllipsoidHeightExtendedM;
        private Double alternatePlatformEllipsoidHeightExtendedM;
        private Double rangeToRecoveryKm;
        private Double platformCourseAngleDeg;
        private Double altitudeAglM;
        private Double radarAltimeterM;
        private Double sensorAzimuthRateDps;
        private Double sensorElevationRateDps;
        private Double sensorRollRateDps;
        private Double miStoragePercentFull;
        private Double transmissionFrequencyMhz;
        private Double zoomPercentage;

        // Var-length int/enum fields
        private Long timeAirborneS;
        private Long propulsionUnitSpeedRpm;
        private Integer navsatsInView;
        private Integer positioningMethodSource;
        private Integer platformStatusCode;
        private Integer sensorControlModeCode;
        private Long takeOffTimeUs;
        private Long miStorageCapacityGb;
        private Integer leapSeconds;
        private Long correctionOffsetUs;
        private ByteBuffer activePayloads;

        // Named nested-set raw byte fields
        private ByteBuffer rvt;
        private ByteBuffer sarMiLocalSet;
        private ByteBuffer rangeImageLocalSet;
        private ByteBuffer geoRegistrationLocalSet;
        private ByteBuffer compositeImagingLocalSet;
        private ByteBuffer segmentLocalSet;
        private ByteBuffer amendLocalSet;

        // Raw/simple scalar + string fields
        private Integer outsideAirTempC;
        private Integer weaponLoad;
        private Integer weaponFired;
        private Integer laserPrfCode;
        private String alternatePlatformName;
        private Long eventStartTimeUs;
        private String streamDesignator;
        private String operationalBase;
        private String broadcastSource;
        private String targetId;
        private String communicationsMethod;

        // Coded enums (raw codepoint)
        private Integer icingDetectedCode;
        private Integer sensorFovNameCode;
        private Integer operationalModeCode;

        // Pack & list items
        private ImageHorizonPixels imageHorizon;
        private List<ControlCommand> controlCommands = Collections.emptyList();
        private List<Long> controlCommandVerification;
        private List<Long> activeWavelengths;
        private SensorFrameRate sensorFrameRate;
        private MetadataSubstreamId metadataSubstreamId;
        private CountryCodes countryCodes;
        private List<WavelengthRecord> wavelengthsList;
        private AirbaseLocations airbaseLocations;
        private PayloadList payloadList;
        private List<WeaponsStore> weaponsStores;
        private List<Waypoint> waypointList;
        private ViewDomain viewDomain;
        private List<SdccFlpField> sdccFlps = Collections.emptyList();

        private List<KlvUnknownField> unknown = Collections.emptyList();
        private List<KlvFieldError> fieldErrors = Collections.emptyList();
        private List<Long> sentinelTags = Collections.emptyList();
        private List<ImapbSpecialEntry> imapbSpecials = Collections.emptyList();

        public Builder() {}

        public Builder universalLabel(ByteBuffer v) { this.universalLabel = v; return this; }
        public Builder declaredVersion(int v) { this.declaredVersion = v; return this; }
        public Builder missionId(String v) { this.missionId = v; return this; }
        public Builder platformTailNumber(String v) { this.platformTailNumber = v; return this; }
        public Builder platformDesignation(String v) { this.platformDesignation = v; return this; }
        public Builder imageSourceSensor(String v) { this.imageSourceSensor = v; return this; }
        public Builder imageCoordinateSystem(String v) { this.imageCoordinateSystem = v; return this; }
        public Builder platformCallSign(String v) { this.platformCallSign = v; return this; }
        public Builder uasLsVersion(int v) { this.uasLsVersion = v; return this; }
        public Builder timestampUs(long v) { this.timestampUs = v; return this; }
        public Builder platformHeadingDeg(double v) { this.platformHeadingDeg = v; return this; }
        public Builder platformPitchDeg(double v) { this.platformPitchDeg = v; return this; }
        public Builder platformRollDeg(double v) { this.platformRollDeg = v; return this; }
        public Builder platformTrueAirspeed(double v) { this.platformTrueAirspeed = v; return this; }
        public Builder platformIndicatedAirspeed(double v) { this.platformIndicatedAirspeed = v; return this; }
        public Builder platformPitchFullDeg(double v) { this.platformPitchFullDeg = v; return this; }
        public Builder platformRollFullDeg(double v) { this.platformRollFullDeg = v; return this; }
        public Builder platformAngleOfAttackDeg(double v) { this.platformAngleOfAttackDeg = v; return this; }
        public Builder sensorLatDeg(double v) { this.sensorLatDeg = v; return this; }
        public Builder sensorLonDeg(double v) { this.sensorLonDeg = v; return this; }
        public Builder sensorAltM(double v) { this.sensorAltM = v; return this; }
        public Builder sensorEllipsoidHeightM(double v) { this.sensorEllipsoidHeightM = v; return this; }
        public Builder sensorHfovDeg(double v) { this.sensorHfovDeg = v; return this; }
        public Builder sensorVfovDeg(double v) { this.sensorVfovDeg = v; return this; }
        public Builder sensorRelAzDeg(double v) { this.sensorRelAzDeg = v; return this; }
        public Builder sensorRelElDeg(double v) { this.sensorRelElDeg = v; return this; }
        public Builder sensorRelRollDeg(double v) { this.sensorRelRollDeg = v; return this; }
        public Builder slantRangeM(double v) { this.slantRangeM = v; return this; }
        public Builder targetWidthM(double v) { this.targetWidthM = v; return this; }
        public Builder frameCenterLatDeg(double v) { this.frameCenterLatDeg = v; return this; }
        public Builder frameCenterLonDeg(double v) { this.frameCenterLonDeg = v; return this; }
        public Builder frameCenterElevM(double v) { this.frameCenterElevM = v; return this; }
        public Builder frameCenterEllipsoidHeightM(double v) { this.frameCenterEllipsoidHeightM = v; return this; }
        public Builder cornerLatOffsetP1Deg(double v) { this.cornerLatOffsetP1Deg = v; return this; }
        public Builder cornerLonOffsetP1Deg(double v) { this.cornerLonOffsetP1Deg = v; return this; }
        public Builder cornerLatOffsetP2Deg(double v) { this.cornerLatOffsetP2Deg = v; return this; }
        public Builder cornerLonOffsetP2Deg(double v) { this.cornerLonOffsetP2Deg = v; return this; }
        public Builder cornerLatOffsetP3Deg(double v) { this.cornerLatOffsetP3Deg = v; return this; }
        public Builder cornerLonOffsetP3Deg(double v) { this.cornerLonOffsetP3Deg = v; return this; }
        public Builder cornerLatOffsetP4Deg(double v) { this.cornerLatOffsetP4Deg = v; return this; }
        public Builder cornerLonOffsetP4Deg(double v) { this.cornerLonOffsetP4Deg = v; return this; }
        public Builder cornerLatP1Deg(double v) { this.cornerLatP1Deg = v; return this; }
        public Builder cornerLonP1Deg(double v) { this.cornerLonP1Deg = v; return this; }
        public Builder cornerLatP2Deg(double v) { this.cornerLatP2Deg = v; return this; }
        public Builder cornerLonP2Deg(double v) { this.cornerLonP2Deg = v; return this; }
        public Builder cornerLatP3Deg(double v) { this.cornerLatP3Deg = v; return this; }
        public Builder cornerLonP3Deg(double v) { this.cornerLonP3Deg = v; return this; }
        public Builder cornerLatP4Deg(double v) { this.cornerLatP4Deg = v; return this; }
        public Builder cornerLonP4Deg(double v) { this.cornerLonP4Deg = v; return this; }
        public Builder genericFlagData(int v) { this.genericFlagData = v; return this; }
        public Builder securityLocalSet(ByteBuffer v) { this.securityLocalSet = v; return this; }
        public Builder vmti(ByteBuffer v) { this.vmti = v; return this; }
        public Builder miisCoreId(byte[] v) { this.miisCoreId = v; return this; }

        // Ranged f64 fields
        public Builder targetLocationLatDeg(double v) { this.targetLocationLatDeg = v; return this; }
        public Builder targetLocationLonDeg(double v) { this.targetLocationLonDeg = v; return this; }
        public Builder targetLocationElevM(double v) { this.targetLocationElevM = v; return this; }
        public Builder targetTrackGateWidthPx(double v) { this.targetTrackGateWidthPx = v; return this; }
        public Builder targetTrackGateHeightPx(double v) { this.targetTrackGateHeightPx = v; return this; }
        public Builder targetErrorCe90M(double v) { this.targetErrorCe90M = v; return this; }
        public Builder targetErrorLe90M(double v) { this.targetErrorLe90M = v; return this; }
        public Builder windDirectionDeg(double v) { this.windDirectionDeg = v; return this; }
        public Builder windSpeed(double v) { this.windSpeed = v; return this; }
        public Builder staticPressureMbar(double v) { this.staticPressureMbar = v; return this; }
        public Builder densityAltitudeM(double v) { this.densityAltitudeM = v; return this; }
        public Builder differentialPressureMbar(double v) { this.differentialPressureMbar = v; return this; }
        public Builder airfieldBarometricPressureMbar(double v) { this.airfieldBarometricPressureMbar = v; return this; }
        public Builder airfieldElevationM(double v) { this.airfieldElevationM = v; return this; }
        public Builder relativeHumidityPct(double v) { this.relativeHumidityPct = v; return this; }
        public Builder platformVerticalSpeed(double v) { this.platformVerticalSpeed = v; return this; }
        public Builder platformSideslipDeg(double v) { this.platformSideslipDeg = v; return this; }
        public Builder platformGroundSpeed(double v) { this.platformGroundSpeed = v; return this; }
        public Builder groundRangeM(double v) { this.groundRangeM = v; return this; }
        public Builder platformFuelRemainingKg(double v) { this.platformFuelRemainingKg = v; return this; }
        public Builder platformMagneticHeadingDeg(double v) { this.platformMagneticHeadingDeg = v; return this; }
        public Builder platformAngleOfAttackFullDeg(double v) { this.platformAngleOfAttackFullDeg = v; return this; }
        public Builder platformSideslipFullDeg(double v) { this.platformSideslipFullDeg = v; return this; }
        public Builder alternatePlatformLatDeg(double v) { this.alternatePlatformLatDeg = v; return this; }
        public Builder alternatePlatformLonDeg(double v) { this.alternatePlatformLonDeg = v; return this; }
        public Builder alternatePlatformAltM(double v) { this.alternatePlatformAltM = v; return this; }
        public Builder alternatePlatformHeadingDeg(double v) { this.alternatePlatformHeadingDeg = v; return this; }
        public Builder alternatePlatformEllipsoidHeightM(double v) { this.alternatePlatformEllipsoidHeightM = v; return this; }
        public Builder sensorNorthVelocity(double v) { this.sensorNorthVelocity = v; return this; }
        public Builder sensorEastVelocity(double v) { this.sensorEastVelocity = v; return this; }

        // IMAPB f64 fields
        public Builder targetWidthExtendedM(double v) { this.targetWidthExtendedM = v; return this; }
        public Builder densityAltitudeExtendedM(double v) { this.densityAltitudeExtendedM = v; return this; }
        public Builder sensorEllipsoidHeightExtendedM(double v) { this.sensorEllipsoidHeightExtendedM = v; return this; }
        public Builder alternatePlatformEllipsoidHeightExtendedM(double v) { this.alternatePlatformEllipsoidHeightExtendedM = v; return this; }
        public Builder rangeToRecoveryKm(double v) { this.rangeToRecoveryKm = v; return this; }
        public Builder platformCourseAngleDeg(double v) { this.platformCourseAngleDeg = v; return this; }
        public Builder altitudeAglM(double v) { this.altitudeAglM = v; return this; }
        public Builder radarAltimeterM(double v) { this.radarAltimeterM = v; return this; }
        public Builder sensorAzimuthRateDps(double v) { this.sensorAzimuthRateDps = v; return this; }
        public Builder sensorElevationRateDps(double v) { this.sensorElevationRateDps = v; return this; }
        public Builder sensorRollRateDps(double v) { this.sensorRollRateDps = v; return this; }
        public Builder miStoragePercentFull(double v) { this.miStoragePercentFull = v; return this; }
        public Builder transmissionFrequencyMhz(double v) { this.transmissionFrequencyMhz = v; return this; }
        public Builder zoomPercentage(double v) { this.zoomPercentage = v; return this; }

        // Var-length int/enum fields
        public Builder timeAirborneS(long v) { this.timeAirborneS = v; return this; }
        public Builder propulsionUnitSpeedRpm(long v) { this.propulsionUnitSpeedRpm = v; return this; }
        public Builder navsatsInView(int v) { this.navsatsInView = v; return this; }
        public Builder positioningMethodSource(int v) { this.positioningMethodSource = v; return this; }
        public Builder platformStatusCode(int v) { this.platformStatusCode = v; return this; }
        public Builder sensorControlModeCode(int v) { this.sensorControlModeCode = v; return this; }
        public Builder takeOffTimeUs(long v) { this.takeOffTimeUs = v; return this; }
        public Builder miStorageCapacityGb(long v) { this.miStorageCapacityGb = v; return this; }
        public Builder leapSeconds(int v) { this.leapSeconds = v; return this; }
        public Builder correctionOffsetUs(long v) { this.correctionOffsetUs = v; return this; }
        public Builder activePayloads(ByteBuffer v) { this.activePayloads = v; return this; }

        // Named nested-set raw byte fields
        public Builder rvt(ByteBuffer v) { this.rvt = v; return this; }
        public Builder sarMiLocalSet(ByteBuffer v) { this.sarMiLocalSet = v; return this; }
        public Builder rangeImageLocalSet(ByteBuffer v) { this.rangeImageLocalSet = v; return this; }
        public Builder geoRegistrationLocalSet(ByteBuffer v) { this.geoRegistrationLocalSet = v; return this; }
        public Builder compositeImagingLocalSet(ByteBuffer v) { this.compositeImagingLocalSet = v; return this; }
        public Builder segmentLocalSet(ByteBuffer v) { this.segmentLocalSet = v; return this; }
        public Builder amendLocalSet(ByteBuffer v) { this.amendLocalSet = v; return this; }

        // Raw/simple scalar + string fields
        public Builder outsideAirTempC(int v) { this.outsideAirTempC = v; return this; }
        public Builder weaponLoad(int v) { this.weaponLoad = v; return this; }
        public Builder weaponFired(int v) { this.weaponFired = v; return this; }
        public Builder laserPrfCode(int v) { this.laserPrfCode = v; return this; }
        public Builder alternatePlatformName(String v) { this.alternatePlatformName = v; return this; }
        public Builder eventStartTimeUs(long v) { this.eventStartTimeUs = v; return this; }
        public Builder streamDesignator(String v) { this.streamDesignator = v; return this; }
        public Builder operationalBase(String v) { this.operationalBase = v; return this; }
        public Builder broadcastSource(String v) { this.broadcastSource = v; return this; }
        public Builder targetId(String v) { this.targetId = v; return this; }
        public Builder communicationsMethod(String v) { this.communicationsMethod = v; return this; }

        // Coded enums (raw codepoint; the record component is
        // authoritative, see UasDatalinkLs.icingDetected() etc. for the typed view)
        public Builder icingDetectedCode(int v) { this.icingDetectedCode = v; return this; }
        public Builder sensorFovNameCode(int v) { this.sensorFovNameCode = v; return this; }
        public Builder operationalModeCode(int v) { this.operationalModeCode = v; return this; }

        // Pack & list items
        public Builder imageHorizon(ImageHorizonPixels v) { this.imageHorizon = v; return this; }
        public Builder controlCommands(List<ControlCommand> v) { this.controlCommands = v; return this; }
        public Builder controlCommandVerification(List<Long> v) { this.controlCommandVerification = v; return this; }
        public Builder activeWavelengths(List<Long> v) { this.activeWavelengths = v; return this; }
        public Builder sensorFrameRate(SensorFrameRate v) { this.sensorFrameRate = v; return this; }
        public Builder metadataSubstreamId(MetadataSubstreamId v) { this.metadataSubstreamId = v; return this; }
        public Builder countryCodes(CountryCodes v) { this.countryCodes = v; return this; }
        public Builder wavelengthsList(List<WavelengthRecord> v) { this.wavelengthsList = v; return this; }
        public Builder airbaseLocations(AirbaseLocations v) { this.airbaseLocations = v; return this; }
        public Builder payloadList(PayloadList v) { this.payloadList = v; return this; }
        public Builder weaponsStores(List<WeaponsStore> v) { this.weaponsStores = v; return this; }
        public Builder waypointList(List<Waypoint> v) { this.waypointList = v; return this; }
        public Builder viewDomain(ViewDomain v) { this.viewDomain = v; return this; }
        public Builder sdccFlps(List<SdccFlpField> v) { this.sdccFlps = v; return this; }

        public Builder unknown(List<KlvUnknownField> v) { this.unknown = v; return this; }
        public Builder fieldErrors(List<KlvFieldError> v) { this.fieldErrors = v; return this; }
        public Builder sentinelTags(List<Long> v) { this.sentinelTags = v; return this; }
        public Builder imapbSpecials(List<ImapbSpecialEntry> v) { this.imapbSpecials = v; return this; }

        /** Build an immutable {@link UasDatalinkLs}. */
        public UasDatalinkLs build() {
            return new UasDatalinkLs(
                    universalLabel, declaredVersion,
                    missionId, platformTailNumber, platformDesignation,
                    imageSourceSensor, imageCoordinateSystem, platformCallSign,
                    uasLsVersion,
                    timestampUs,
                    platformHeadingDeg, platformPitchDeg, platformRollDeg,
                    platformTrueAirspeed, platformIndicatedAirspeed,
                    platformPitchFullDeg, platformRollFullDeg, platformAngleOfAttackDeg,
                    sensorLatDeg, sensorLonDeg, sensorAltM, sensorEllipsoidHeightM,
                    sensorHfovDeg, sensorVfovDeg,
                    sensorRelAzDeg, sensorRelElDeg, sensorRelRollDeg,
                    slantRangeM, targetWidthM,
                    frameCenterLatDeg, frameCenterLonDeg, frameCenterElevM,
                    frameCenterEllipsoidHeightM,
                    cornerLatOffsetP1Deg, cornerLonOffsetP1Deg,
                    cornerLatOffsetP2Deg, cornerLonOffsetP2Deg,
                    cornerLatOffsetP3Deg, cornerLonOffsetP3Deg,
                    cornerLatOffsetP4Deg, cornerLonOffsetP4Deg,
                    cornerLatP1Deg, cornerLonP1Deg,
                    cornerLatP2Deg, cornerLonP2Deg,
                    cornerLatP3Deg, cornerLonP3Deg,
                    cornerLatP4Deg, cornerLonP4Deg,
                    genericFlagData,
                    securityLocalSet, vmti,
                    miisCoreId,
                    targetLocationLatDeg, targetLocationLonDeg, targetLocationElevM,
                    targetTrackGateWidthPx, targetTrackGateHeightPx,
                    targetErrorCe90M, targetErrorLe90M,
                    windDirectionDeg, windSpeed,
                    staticPressureMbar, densityAltitudeM,
                    differentialPressureMbar,
                    airfieldBarometricPressureMbar, airfieldElevationM,
                    relativeHumidityPct,
                    platformVerticalSpeed, platformSideslipDeg,
                    platformGroundSpeed, groundRangeM, platformFuelRemainingKg,
                    platformMagneticHeadingDeg,
                    platformAngleOfAttackFullDeg, platformSideslipFullDeg,
                    alternatePlatformLatDeg, alternatePlatformLonDeg, alternatePlatformAltM,
                    alternatePlatformHeadingDeg, alternatePlatformEllipsoidHeightM,
                    sensorNorthVelocity, sensorEastVelocity,
                    targetWidthExtendedM, densityAltitudeExtendedM,
                    sensorEllipsoidHeightExtendedM,
                    alternatePlatformEllipsoidHeightExtendedM,
                    rangeToRecoveryKm, platformCourseAngleDeg,
                    altitudeAglM, radarAltimeterM,
                    sensorAzimuthRateDps, sensorElevationRateDps, sensorRollRateDps,
                    miStoragePercentFull, transmissionFrequencyMhz, zoomPercentage,
                    timeAirborneS, propulsionUnitSpeedRpm,
                    navsatsInView, positioningMethodSource,
                    platformStatusCode, sensorControlModeCode,
                    takeOffTimeUs, miStorageCapacityGb,
                    leapSeconds, correctionOffsetUs, activePayloads,
                    rvt, sarMiLocalSet, rangeImageLocalSet, geoRegistrationLocalSet,
                    compositeImagingLocalSet, segmentLocalSet, amendLocalSet,
                    outsideAirTempC, weaponLoad, weaponFired, laserPrfCode,
                    alternatePlatformName, eventStartTimeUs,
                    streamDesignator, operationalBase, broadcastSource,
                    targetId, communicationsMethod,
                    icingDetectedCode, sensorFovNameCode, operationalModeCode,
                    imageHorizon, controlCommands, controlCommandVerification, activeWavelengths,
                    sensorFrameRate, metadataSubstreamId, countryCodes, wavelengthsList,
                    airbaseLocations, payloadList, weaponsStores, waypointList, viewDomain, sdccFlps,
                    unknown, fieldErrors,
                    sentinelTags, imapbSpecials
            );
        }
    }
}
