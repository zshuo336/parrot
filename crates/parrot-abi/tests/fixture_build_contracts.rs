#[path = "common/build.rs"]
mod build;

fn flags(ordinary: Option<&str>, encoded: Option<&str>) -> Vec<String> {
    let command = build::with_flags(ordinary, encoded);
    let (_, value) = command
        .get_envs()
        .find(|(key, _)| *key == "CARGO_ENCODED_RUSTFLAGS")
        .unwrap();
    value
        .unwrap()
        .to_str()
        .unwrap()
        .split('\x1f')
        .map(str::to_owned)
        .collect()
}

#[test]
fn ordinary_flags_are_preserved_and_dynamic_std_is_required() {
    assert_eq!(flags(None, None), ["-C", "prefer-dynamic"]);
    assert_eq!(
        flags(Some("--cfg original -C link-arg=-Wl,--threads=1"), None),
        [
            "--cfg",
            "original",
            "-C",
            "link-arg=-Wl,--threads=1",
            "-C",
            "prefer-dynamic"
        ]
    );
    // Exercise the environment entry point without mutating process globals.
    assert!(build::cargo()
        .get_envs()
        .any(|(key, _)| key == "CARGO_ENCODED_RUSTFLAGS"));
}

#[test]
fn encoded_flags_preserve_argument_boundaries_and_override_ordinary_flags() {
    assert_eq!(
        flags(
            Some("--cfg ignored"),
            Some("-C\x1flink-arg=/path with spaces/lib")
        ),
        [
            "-C",
            "link-arg=/path with spaces/lib",
            "-C",
            "prefer-dynamic"
        ]
    );
    assert_eq!(
        flags(Some("--cfg ignored"), Some("")),
        ["-C", "prefer-dynamic"]
    );
}

#[test]
fn explicit_static_preference_is_overridden_without_dropping_other_options() {
    for supplied in ["-C\x1fprefer-dynamic=no", "-Cprefer-dynamic=no"] {
        let actual = flags(None, Some(supplied));
        assert!(actual.ends_with(&["-C".to_owned(), "prefer-dynamic".to_owned()]));
        assert_eq!(
            actual[..actual.len() - 2],
            supplied.split('\x1f').collect::<Vec<_>>()
        );
    }
    assert_eq!(
        flags(Some("-C prefer-dynamic"), None),
        ["-C", "prefer-dynamic"]
    );
}
