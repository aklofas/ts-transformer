"""Unit tests for scripts/dev/deps_advisory.py (the pure parts: no network).

Run:  python3 -m unittest scripts/dev/test_deps_advisory.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import deps_advisory as da  # noqa: E402


class ParseVersion(unittest.TestCase):
    """One parser over the four upstream tag grammars."""

    def test_libsrt_v_prefix(self):
        self.assertEqual(da.parse_version("v1.5.7"), ((1, 5, 7), False))

    def test_mbedtls_name_prefix(self):
        self.assertEqual(da.parse_version("mbedtls-3.6.7"), ((3, 6, 7), False))

    def test_freertos_capital_v(self):
        self.assertEqual(da.parse_version("V11.3.1"), ((11, 3, 1), False))

    def test_lwip_stable_underscores(self):
        self.assertEqual(da.parse_version("STABLE-2_2_1_RELEASE"), ((2, 2, 1), False))

    def test_release_candidate_is_prerelease(self):
        self.assertEqual(da.parse_version("v0.2.19-rc4"), ((0, 2, 19), True))
        self.assertEqual(da.parse_version("STABLE-2_2_0_RC1"), ((2, 2, 0), True))

    def test_two_component_version(self):
        self.assertEqual(da.parse_version("v1.0"), ((1, 0), False))

    def test_junk_tags_are_none(self):
        for tag in ("start", "merged_from_main_to_STABLE", "yotta", ""):
            self.assertIsNone(da.parse_version(tag), tag)


class LineKey(unittest.TestCase):
    def test_major_line(self):
        self.assertEqual(da.line_key((3, 6, 7)), (3,))

    def test_zero_major_uses_minor(self):
        self.assertEqual(da.line_key((0, 2, 20)), (0, 2))


class ReleaseComparison(unittest.TestCase):
    TAGS = ["v4.2.0", "mbedtls-4.2.0", "mbedtls-4.1.1", "mbedtls-3.6.8",
            "mbedtls-3.6.7", "mbedtls-3.6.8-rc1", "yotta-2.3.2"]

    def test_newest_in_pinned_line_ignores_prereleases_and_other_lines(self):
        self.assertEqual(da.newest_in_line(self.TAGS, (3, 6, 7)), ((3, 6, 8), "mbedtls-3.6.8"))

    def test_newest_in_line_is_none_when_pin_is_current(self):
        self.assertIsNone(da.newest_in_line(["mbedtls-3.6.7", "mbedtls-4.2.0"], (3, 6, 7)))

    def test_newer_lines_deduplicates_tag_spellings(self):
        self.assertEqual(da.newer_lines(self.TAGS, (3, 6, 7)), [((4,), "4.2.0")])

    def test_newer_lines_for_zero_major(self):
        self.assertEqual(da.newer_lines(["v0.2.20", "v0.3.0", "v0.3.1"], (0, 2, 20)),
                         [((0, 3), "0.3.1")])


class VersionSource(unittest.TestCase):
    """Upstream's curated releases beat the raw tag list, which carries junk
    like mbedtls `mbedos-16.03-release` and FreeRTOS `V202110.00-SMP`."""

    def test_releases_win_when_present(self):
        self.assertEqual(da.choose_versions(["mbedtls-3.6.7"], ["mbedos-16.03-release", "v3.6.7"]),
                         ["mbedtls-3.6.7"])

    def test_tags_when_no_releases(self):
        self.assertEqual(da.choose_versions([], ["STABLE-2_2_1_RELEASE"]), ["STABLE-2_2_1_RELEASE"])


class ResolvePin(unittest.TestCase):
    def test_pin_sha_maps_to_its_tag(self):
        tags = [("v1.5.7", "899348d8318eb9a3c5a5b6ec43c4a1114288773a"), ("v1.5.6", "c63c311e")]
        self.assertEqual(da.resolve_pin_tag(tags, "899348d8318eb9a3c5a5b6ec43c4a1114288773a"), "v1.5.7")

    def test_release_spelling_preferred_when_two_tags_share_the_commit(self):
        tags = [("v3.6.7", "068ff080"), ("mbedtls-3.6.7", "068ff080")]
        self.assertEqual(da.resolve_pin_tag(tags, "068ff080", prefer={"mbedtls-3.6.7"}), "mbedtls-3.6.7")

    def test_untagged_pin_is_none(self):
        self.assertIsNone(da.resolve_pin_tag([("v1.0", "abc")], "f8cc8e38"))


class GhsaRange(unittest.TestCase):
    def test_and_joined_bounds(self):
        rng = ">=V7.4.0 AND <=V11.3.0"
        self.assertTrue(da.range_affects(rng, (11, 3, 0)))
        self.assertFalse(da.range_affects(rng, (11, 3, 1)))
        self.assertFalse(da.range_affects(rng, (7, 3, 9)))

    def test_single_upper_bound(self):
        self.assertTrue(da.range_affects("< 1.5.6", (1, 5, 5)))
        self.assertFalse(da.range_affects("< 1.5.6", (1, 5, 7)))

    def test_unparsable_range_is_conservative(self):
        self.assertTrue(da.range_affects("all versions", (1, 0, 0)))
        self.assertTrue(da.range_affects("", (1, 0, 0)))

    def test_advisory_affects_pin_uses_any_vulnerability_entry(self):
        adv = {"vulnerabilities": [
            {"vulnerable_version_range": "< 1.0.0"},
            {"vulnerable_version_range": ">= 1.5.0 AND < 1.5.8"},
        ]}
        self.assertTrue(da.ghsa_affects(adv, (1, 5, 7)))
        self.assertFalse(da.ghsa_affects(adv, (1, 5, 8)))

    def test_advisory_without_ranges_falls_back_to_publish_date(self):
        adv = {"vulnerabilities": [], "published_at": "2026-09-01T00:00:00Z"}
        self.assertTrue(da.ghsa_affects(adv, (1, 5, 7), pin_date="2026-08-28T07:12:34Z"))
        self.assertFalse(da.ghsa_affects(adv, (1, 5, 7), pin_date="2026-09-02T00:00:00Z"))


class NvdMatch(unittest.TestCase):
    CPE = "cpe:2.3:a:arm:mbed_tls"

    def cve(self, matches, published="2026-01-01T00:00:00.000"):
        return {"cve": {"id": "CVE-1", "published": published, "configurations": [
            {"nodes": [{"cpeMatch": matches}]}]}}

    def test_end_excluding_bound(self):
        m = [{"vulnerable": True, "criteria": self.CPE + ":*:*:*:*:*:*:*:*",
              "versionEndExcluding": "3.6.8"}]
        self.assertTrue(da.nvd_affects(self.cve(m), self.CPE, (3, 6, 7), pin_date="2020-01-01T00:00:00Z"))
        self.assertFalse(da.nvd_affects(self.cve(m), self.CPE, (3, 6, 8), pin_date="2020-01-01T00:00:00Z"))

    def test_start_and_end_including(self):
        m = [{"vulnerable": True, "criteria": self.CPE + ":*:*:*:*:*:*:*:*",
              "versionStartIncluding": "3.0.0", "versionEndIncluding": "3.6.7"}]
        self.assertTrue(da.nvd_affects(self.cve(m), self.CPE, (3, 6, 7), pin_date="2020-01-01T00:00:00Z"))
        self.assertFalse(da.nvd_affects(self.cve(m), self.CPE, (2, 28, 0), pin_date="2020-01-01T00:00:00Z"))

    def test_exact_version_in_criteria(self):
        m = [{"vulnerable": True, "criteria": self.CPE + ":3.6.7:*:*:*:*:*:*:*"}]
        self.assertTrue(da.nvd_affects(self.cve(m), self.CPE, (3, 6, 7), pin_date="2020-01-01T00:00:00Z"))
        self.assertFalse(da.nvd_affects(self.cve(m), self.CPE, (3, 6, 6), pin_date="2020-01-01T00:00:00Z"))

    def test_unbounded_match_falls_back_to_publish_date(self):
        m = [{"vulnerable": True, "criteria": self.CPE + ":*:*:*:*:*:*:*:*"}]
        old = self.cve(m, published="2017-01-01T00:00:00.000")
        new = self.cve(m, published="2026-09-01T00:00:00.000")
        self.assertFalse(da.nvd_affects(old, self.CPE, (3, 6, 7), pin_date="2026-08-28T07:12:34Z"))
        self.assertTrue(da.nvd_affects(new, self.CPE, (3, 6, 7), pin_date="2026-08-28T07:12:34Z"))

    def test_other_products_in_the_same_cve_are_ignored(self):
        m = [{"vulnerable": True, "criteria": "cpe:2.3:a:other:thing:*:*:*:*:*:*:*:*",
              "versionEndExcluding": "9.9.9"}]
        self.assertFalse(da.nvd_affects(self.cve(m), self.CPE, (3, 6, 7), pin_date="2020-01-01T00:00:00Z"))


class Findings(unittest.TestCase):
    """Finding titles are the dedupe key, so they are asserted exactly."""

    def dep(self, **kw):
        d = {"name": "mbedtls", "path": "crates/mbedtls-src/vendor/mbedtls",
             "url": "https://github.com/Mbed-TLS/mbedtls"}
        d.update(kw)
        return d

    def test_pin_behind_in_line(self):
        f = da.build_findings(self.dep(), pin_tag="mbedtls-3.6.7", pin_version=(3, 6, 7),
                              tags=["mbedtls-3.6.8", "mbedtls-3.6.7"], advisories=[], cves=[])
        self.assertEqual([x.title for x in f], ["deps: mbedtls 3.6.8 released (pinned mbedtls-3.6.7)"])
        self.assertEqual(f[0].kind, "release")

    def test_newer_major_line_is_one_finding_per_line(self):
        f = da.build_findings(self.dep(), pin_tag="mbedtls-3.6.7", pin_version=(3, 6, 7),
                              tags=["mbedtls-4.2.0", "mbedtls-4.1.1", "mbedtls-3.6.7"], advisories=[], cves=[])
        self.assertEqual([x.title for x in f], ["deps: mbedtls 4.x line available (pinned mbedtls-3.6.7)"])
        self.assertEqual(f[0].kind, "major-line")

    def test_ghsa_affecting_pin(self):
        adv = {"ghsa_id": "GHSA-6xg9-784j-24rm", "cve_id": "CVE-2026-1", "severity": "critical",
               "summary": "KMREQ/KMRSP Stack-Based Buffer Overflow", "published_at": "2026-07-20T14:06:56Z",
               "html_url": "https://github.com/Haivision/srt/security/advisories/GHSA-6xg9-784j-24rm",
               "vulnerabilities": [{"vulnerable_version_range": "< 1.5.6", "patched_versions": "1.5.6"}]}
        f = da.build_findings(self.dep(name="libsrt"), pin_tag="v1.5.5", pin_version=(1, 5, 5),
                              tags=["v1.5.5"], advisories=[adv], cves=[])
        self.assertEqual([x.title for x in f],
                         ["deps: GHSA-6xg9-784j-24rm affects pinned libsrt v1.5.5 (critical)"])
        self.assertIn("CVE-2026-1", f[0].body)
        self.assertIn("1.5.6", f[0].body)

    def test_ghsa_not_affecting_pin_is_silent(self):
        adv = {"ghsa_id": "GHSA-x", "severity": "high", "summary": "s", "published_at": "2026-07-20T14:06:56Z",
               "vulnerabilities": [{"vulnerable_version_range": "< 1.5.6"}]}
        f = da.build_findings(self.dep(name="libsrt"), pin_tag="v1.5.7", pin_version=(1, 5, 7),
                              tags=["v1.5.7"], advisories=[adv], cves=[])
        self.assertEqual(f, [])

    def test_nvd_cve_affecting_pin(self):
        cve = {"cve": {"id": "CVE-2026-9", "published": "2026-09-01T00:00:00.000",
                       "descriptions": [{"lang": "en", "value": "An overflow."}],
                       "metrics": {"cvssMetricV31": [{"cvssData": {"baseSeverity": "HIGH"}}]},
                       "configurations": [{"nodes": [{"cpeMatch": [
                           {"vulnerable": True, "criteria": "cpe:2.3:a:arm:mbed_tls:*:*:*:*:*:*:*:*",
                            "versionEndExcluding": "3.6.8"}]}]}]}}
        f = da.build_findings(self.dep(cpe="cpe:2.3:a:arm:mbed_tls"), pin_tag="mbedtls-3.6.7",
                              pin_version=(3, 6, 7), tags=["mbedtls-3.6.7"], advisories=[], cves=[cve],
                              pin_date="2026-07-07T14:43:13Z")
        self.assertEqual([x.title for x in f], ["deps: CVE-2026-9 affects pinned mbedtls mbedtls-3.6.7 (HIGH)"])
        self.assertIn("An overflow.", f[0].body)

    def test_untagged_pin_reports_nothing_about_releases(self):
        f = da.build_findings(self.dep(name="freertos-posix"), pin_tag=None, pin_version=None,
                              tags=["v1.0"], advisories=[], cves=[])
        self.assertEqual(f, [])


class Dedupe(unittest.TestCase):
    def test_existing_titles_are_skipped_open_or_closed(self):
        f = [da.Finding("release", "deps: a", "b"), da.Finding("release", "deps: c", "d")]
        self.assertEqual([x.title for x in da.new_findings(f, existing_titles={"deps: a"})], ["deps: c"])


if __name__ == "__main__":
    unittest.main()
