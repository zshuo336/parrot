//! Run twice with the same private directory to observe recovered revisions.
use parrot_remote::{
    raft::{RealClock, Role},
    roles::directory::{DirCmd, DirQuery, DurableDirectory, DurableDirectoryStatus},
};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let directory = PathBuf::from(args.next().ok_or("usage: durable_directory DIRECTORY")?);
    if args.next().is_some() {
        return Err("usage: durable_directory DIRECTORY".into());
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(&directory) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    // A solo group demonstrates the disk/application boundary, not network HA.
    let mut node = DurableDirectory::open(
        &directory,
        "example",
        "directory",
        vec![],
        Arc::new(RealClock::default()),
    )?;
    println!("recovered revision={}", node.state_local()?.version);
    let deadline = Instant::now() + Duration::from_secs(5);
    while node.role() != Role::Leader {
        if Instant::now() >= deadline {
            return Err("local election timed out".into());
        }
        std::thread::sleep(Duration::from_millis(50));
        node.tick()?;
    }
    let receipt = node.propose(&DirCmd::Upsert {
        node: "worker".into(),
        endpoints: vec!["tcp://127.0.0.1:7000".into()],
    })?;
    if node.proposal_status(&receipt)? != DurableDirectoryStatus::Applied {
        return Err("proposal not yet committed and applied".into());
    }
    let query = DirQuery::Resolve {
        prefix_or_node: "worker".into(),
    };
    let before = node.query_local(&query)?.ok_or("missing applied entry")?;
    println!(
        "applied index={} revision={} endpoints={:?}",
        receipt.index(),
        before.version,
        before.endpoints
    );
    drop(node);
    let reopened = DurableDirectory::open(
        &directory,
        "example",
        "directory",
        vec![],
        Arc::new(RealClock::default()),
    )?;
    if reopened.query_local(&query)? != Some(before) {
        return Err("recovery changed the directory entry".into());
    }
    println!(
        "reopen verified revision={}",
        reopened.state_local()?.version
    );
    Ok(())
}
