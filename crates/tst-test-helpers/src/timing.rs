//! Test timing margins.

use std::time::Duration;

/// `base` stretched by the `TST_TEST_TIMEOUT_SCALE` environment variable
/// (a whole number, default 1). The sanitizer jobs in `sanitizers.yml` set it
/// because TSan runs the server thread several times slower, which turns a
/// 2 s read margin into a flake. Unset, the margin is unchanged.
pub fn scaled(base: Duration) -> Duration {
    let scale = std::env::var("TST_TEST_TIMEOUT_SCALE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(1);
    base * scale
}
