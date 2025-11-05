# Bincode 2.0.1 compatibility oracle

These files were generated before the dependency migration, by actual Parrot
production encoders, WAL, DurableRaft and TransactionalInbox using bincode 2.0.1
and serde 1.0.229. SHA256.json fixes each file's bytes. Lock files were omitted.
The complete old generator source and lockfile, provenance, failed initial
fixture setup, successful generation log and executable digest are archived in
docs/review/independent-2026-10-08/codec-maintenance-242/.

Normal tests never regenerate these fixtures. They compare wire bytes and
decoded fields; restore copies of the old durable files; verify pending WAL
records and ACK suppression, committed/uncommitted Raft history and reconstructed
directory state, and atomic inbox business state with duplicate suppression;
then continue writing and reopen twice. The separate migration driver additionally
opens replacement-written files in the archived old executable, testing rollback
readability independently of the new implementation.

These are compatibility examples, not a promise that arbitrary schema changes
or arbitrary user-written Serde implementations are compatible. Preserve this
oracle when evolving the codec; use new versioned fixtures for intentional wire
changes rather than silently refreshing these files with the current encoder.
