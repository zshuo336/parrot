use std::process::Command;

#[test]
fn invalid_twin_workloads_exit_nonzero_without_success_or_panic() {
    for args in [
        vec!["--sample", "0"],
        vec!["--nodes", "0"],
        vec!["--clusters"],
        vec!["--sample", "garbage"],
        vec!["--nodes", "1", "--nodes", "2"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_federation-lab"))
            .arg("twin")
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("通过"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    }
}

#[test]
fn sampled_cli_reports_the_actual_scope() {
    let output = Command::new(env!("CARGO_BIN_EXE_federation-lab"))
        .args([
            "twin",
            "--clusters",
            "2",
            "--nodes",
            "3",
            "--shards",
            "2",
            "--sample",
            "4",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("visited=4 correct=4 unreachable=0 mismatch=0"),
        "{stdout}"
    );
    assert!(stdout.contains("4/6 地址"), "{stdout}");
    assert!(
        !stdout.contains("G5"),
        "a local sample cannot certify the full deployment"
    );
}
