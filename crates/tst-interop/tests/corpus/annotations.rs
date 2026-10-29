use tst_interop::corpus::annotations::{Annotations, load};

use crate::{sha_of, temp_dir_named};

#[test]
fn absent_file_is_default() {
    let dir = temp_dir_named("ann-absent");
    let got = load(&dir, &sha_of(b"nothing")).unwrap();
    assert_eq!(got, Annotations::default());
}

#[test]
fn parses_every_key_and_rejects_an_unknown_one() {
    let dir = temp_dir_named("ann-keys");
    let sha = sha_of(b"annotated");
    let file = dir.join(format!("{sha}.toml"));
    std::fs::write(
        &file,
        "# reviewed by hand\nnon_misb_private_pids = [4097, 4101]\nmax_private_records = 40\n\nknown_discontinuity = true\nmax_pts_backward_breaks = 2\ndiscontinuity_pids = [256, 257]\nknown_cc_mismatch = true\nmax_cc_mismatch = 1\nknown_feed_errors = true\nmax_feed_errors = 3\nknown_klv_truncation = true\nmax_klv_truncated = 6\nklv_truncation_pids = [65]\nreason = \"splice\"\n",
    )
    .unwrap();
    assert_eq!(
        load(&dir, &sha).unwrap(),
        Annotations {
            non_misb_private_pids: vec![4097, 4101],
            max_private_records: Some(40),
            known_discontinuity: true,
            max_pts_backward_breaks: Some(2),
            discontinuity_pids: vec![256, 257],
            known_cc_mismatch: true,
            max_cc_mismatch: Some(1),
            known_feed_errors: true,
            max_feed_errors: Some(3),
            known_klv_truncation: true,
            max_klv_truncated: Some(6),
            klv_truncation_pids: vec![65],
            reason: Some("splice".to_string()),
        }
    );
    std::fs::write(&file, "known_discontinuity = false\nfoo = 1\n").unwrap();
    let err = load(&dir, &sha).unwrap_err();
    assert!(err.contains("line 2"), "{err}");
    std::fs::write(&file, "foo = 1\n").unwrap();
    assert!(load(&dir, &sha).unwrap_err().contains("line 1"));
    std::fs::write(&file, "reason = splice\n").unwrap();
    assert!(load(&dir, &sha).unwrap_err().contains("line 1"));
    std::fs::write(&file, "known_cc_mismatch = yes\n").unwrap();
    assert!(load(&dir, &sha).unwrap_err().contains("line 1"));
    std::fs::write(&file, "max_feed_errors = -1\n").unwrap();
    assert!(load(&dir, &sha).unwrap_err().contains("line 1"));
}

#[test]
fn a_key_that_excuses_a_rule_needs_a_reason() {
    let dir = temp_dir_named("ann-noreason");
    let sha = sha_of(b"annotated without a reason");
    let file = dir.join(format!("{sha}.toml"));
    for (body, line, key) in [
        ("# note\nknown_feed_errors = true\n", 2, "known_feed_errors"),
        ("known_cc_mismatch = true\n", 1, "known_cc_mismatch"),
        (
            "known_discontinuity = true\nreason = \"\"\n",
            1,
            "known_discontinuity",
        ),
        (
            "non_misb_private_pids = [4097]\n",
            1,
            "non_misb_private_pids",
        ),
        (
            "max_klv_truncated = 3\nknown_klv_truncation = true\n",
            2,
            "known_klv_truncation",
        ),
    ] {
        std::fs::write(&file, body).unwrap();
        let err = load(&dir, &sha).unwrap_err();
        assert!(
            err.contains(&format!("line {line}: {key}")),
            "{body:?}: {err}"
        );
        assert!(err.contains("needs a reason"), "{err}");
    }
    // A key that excuses nothing needs none.
    std::fs::write(
        &file,
        "known_feed_errors = false\nnon_misb_private_pids = []\n",
    )
    .unwrap();
    assert_eq!(load(&dir, &sha).unwrap(), Annotations::default());
}

/// Every key that excuses a rule needs the reviewed bound beside it, and
/// its PID list where it has one; the error names the key's line and the
/// missing key.
#[test]
fn a_key_that_excuses_a_rule_needs_its_bound() {
    let dir = temp_dir_named("ann-nobound");
    let sha = sha_of(b"annotated without a bound");
    let file = dir.join(format!("{sha}.toml"));
    let why = "reason = \"reviewed\"\n";
    for (body, line, key, needs) in [
        (
            "known_feed_errors = true\n",
            1,
            "known_feed_errors",
            "max_feed_errors",
        ),
        (
            "known_cc_mismatch = true\n",
            1,
            "known_cc_mismatch",
            "max_cc_mismatch",
        ),
        (
            "known_discontinuity = true\ndiscontinuity_pids = [256]\n",
            1,
            "known_discontinuity",
            "max_pts_backward_breaks",
        ),
        (
            "known_discontinuity = true\nmax_pts_backward_breaks = 2\n",
            1,
            "known_discontinuity",
            "discontinuity_pids",
        ),
        (
            "# note\nknown_klv_truncation = true\nklv_truncation_pids = [65]\n",
            2,
            "known_klv_truncation",
            "max_klv_truncated",
        ),
        (
            "known_klv_truncation = true\nmax_klv_truncated = 6\n",
            1,
            "known_klv_truncation",
            "klv_truncation_pids",
        ),
        (
            "non_misb_private_pids = [4097]\n",
            1,
            "non_misb_private_pids",
            "max_private_records",
        ),
    ] {
        std::fs::write(&file, format!("{body}{why}")).unwrap();
        let err = load(&dir, &sha).unwrap_err();
        assert!(
            err.contains(&format!(
                "line {line}: {key} excuses a rule and needs {needs} = "
            )),
            "{body:?}: {err}"
        );
    }
}

/// A bound or PID list without the key it bounds is a typo, not a no-op.
#[test]
fn a_bound_without_its_excusing_key_is_refused() {
    let dir = temp_dir_named("ann-orphan");
    let sha = sha_of(b"annotated with a stray bound");
    let file = dir.join(format!("{sha}.toml"));
    for (body, line, key, bounds) in [
        (
            "max_feed_errors = 3\n",
            1,
            "max_feed_errors",
            "known_feed_errors",
        ),
        (
            "known_cc_mismatch = false\nmax_cc_mismatch = 0\n",
            2,
            "max_cc_mismatch",
            "known_cc_mismatch",
        ),
        (
            "discontinuity_pids = [256]\n",
            1,
            "discontinuity_pids",
            "known_discontinuity",
        ),
        (
            "max_pts_backward_breaks = 1\n",
            1,
            "max_pts_backward_breaks",
            "known_discontinuity",
        ),
        (
            "klv_truncation_pids = [65]\n",
            1,
            "klv_truncation_pids",
            "known_klv_truncation",
        ),
        (
            "max_klv_truncated = 2\n",
            1,
            "max_klv_truncated",
            "known_klv_truncation",
        ),
        (
            "max_private_records = 4\n",
            1,
            "max_private_records",
            "non_misb_private_pids",
        ),
    ] {
        std::fs::write(&file, body).unwrap();
        let err = load(&dir, &sha).unwrap_err();
        assert!(
            err.contains(&format!(
                "line {line}: {key} bounds {bounds}, which excuses nothing"
            )),
            "{body:?}: {err}"
        );
    }
    // A bound of 0 beside its key is legal: nothing of it is excused.
    std::fs::write(
        &file,
        "known_feed_errors = true\nmax_feed_errors = 0\nreason = \"reviewed\"\n",
    )
    .unwrap();
    assert_eq!(load(&dir, &sha).unwrap().max_feed_errors, Some(0));
}
