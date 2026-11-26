package org.tstrans.klv;

/**
 * The vertical datum of a composite view's altitude: which kind of item the
 * view took the height from. Returned by {@link UasDatalinkLs#sensorPositionDatum()}
 * and {@link UasDatalinkLs#frameCenterDatum()}. Mirrors tst-py's
 * {@code tstrans.klv.AltitudeDatum}.
 */
public enum AltitudeDatum {
    /** Height above the WGS 84 ellipsoid (Items 75, 78, 104). */
    HAE,
    /** Height above mean sea level (Items 15, 25). */
    MSL
}
