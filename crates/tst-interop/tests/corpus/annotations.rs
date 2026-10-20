use tst_interop::corpus::annotations::{Annotations, load};

use crate::{sha_of, temp_dir_named};

#[test]
fn absent_file_is_default() {
    let dir = temp_dir_named("ann-absent");
    let got = load(&dir, &sha_of(b"nothing")).unwrap();
    assert_eq!(got, Annotations::default());
}

#[test]
fn parses_the_three_keys_and_rejects_an_unknown_one() {
    let dir = temp_dir_named("ann-keys");
    let sha = sha_of(b"annotated");
    let file = dir.join(format!("{sha}.toml"));
    std::fs::write(
        &file,
        "# reviewed by hand\nnon_misb_private_pids = [4097, 4101]\n\nknown_discontinuity = true\nreason = \"splice\"\n",
    )
    .unwrap();
    assert_eq!(
        load(&dir, &sha).unwrap(),
        Annotations {
            non_misb_private_pids: vec![4097, 4101],
            known_discontinuity: true,
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
}
