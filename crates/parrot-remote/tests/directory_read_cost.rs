use parrot_remote::{
    raft::ManualClock,
    roles::directory::{DirCmd, DirQuery, DurableDirectory},
};
use std::sync::Arc;

#[test]
fn sustained_reads_preserve_persisted_bytes_and_do_not_exhaust_log_capacity() {
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new());
    let mut owner =
        DurableDirectory::open(directory.path(), "read-cost", "a", vec![], clock.clone()).unwrap();
    clock.advance(1400);
    owner.tick().unwrap();
    let command = DirCmd::Upsert {
        node: "worker".into(),
        endpoints: vec!["mem://worker".into()],
    };
    owner.propose(&command).unwrap();
    let query = DirQuery::Resolve {
        prefix_or_node: "worker".into(),
    };
    let expected = owner.query_local(&query).unwrap();
    let state = directory.path().join("raft.state");
    let before = std::fs::read(&state).unwrap();
    for i in 0..65_537 {
        let mut read = owner.begin_linear_read(&query).unwrap();
        let result = owner.poll_linear_read(&mut read).unwrap().unwrap();
        if i == 0 {
            assert_eq!(
                std::fs::read(&state).unwrap(),
                before,
                "a current-term committed read must not append a persistent barrier"
            );
        }
        assert_eq!(result.entry, expected);
        assert_eq!(result.barrier_index, 1);
        assert_eq!(result.applied_index, 1);
        assert!(owner.poll_linear_read(&mut read).is_err());
    }
    assert_eq!(std::fs::read(&state).unwrap(), before);
    drop(owner);
    let owner = DurableDirectory::open(
        directory.path(),
        "read-cost",
        "a",
        vec![],
        Arc::new(ManualClock::new()),
    )
    .unwrap();
    assert_eq!(owner.applied_index(), 1);
    assert_eq!(owner.query_local(&query).unwrap(), expected);
}
