//! Atomic journal replacement; the separately held directory lease is never replaced.
use super::*;
const NEXT: &str = ".crawl.journal.next";

struct Temporary {
    path: PathBuf,
    published: bool,
}
impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Journal {
    pub(super) fn encode(
        previous: [u8; 32],
        record: &Record,
    ) -> Result<([u8; 32], Vec<u8>), String> {
        let checksum = Self::checksum(previous, record)?;
        let mut bytes = serde_json::to_vec(&Frame {
            previous,
            record: record.clone(),
            checksum,
        })
        .map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        Ok((checksum, bytes))
    }
    pub(super) fn discard_stale_compaction(&self) -> Result<(), String> {
        let path = self.directory.join(NEXT);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                // The lease excludes a live writer. This reserved file was never
                // published; a complete main journal is the sole authority.
                std::fs::remove_file(path)
                    .map_err(|e| format!("remove incomplete compaction: {e}"))?;
                File::open(&self.directory)
                    .and_then(|f| f.sync_all())
                    .map_err(|e| format!("sync compaction cleanup: {e}"))
            }
            Ok(_) => Err("journal compaction path is not a regular file".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("inspect journal compaction: {error}")),
        }
    }
    fn write_compact_record(
        file: &mut File,
        digest: &mut [u8; 32],
        total: &mut u64,
        record: &Record,
    ) -> Result<(), String> {
        let (next, bytes) = Self::encode(*digest, record)?;
        if bytes.len() as u64 > MAX_RECORD || bytes.len() as u64 > MAX_LOG.saturating_sub(*total) {
            return Err("compact journal exceeds byte budget".into());
        }
        file.write_all(&bytes)
            .map_err(|e| format!("write compact journal: {e}"))?;
        *digest = next;
        *total += bytes.len() as u64;
        Ok(())
    }
    pub(super) fn publish_compaction(
        &mut self,
        replacement: Option<&Record>,
    ) -> Result<(), String> {
        if self.poisoned {
            return Err("crawl journal failed; reopen before further writes".into());
        }
        if replacement.is_none() && self.checkpoint.is_none() && self.reset.is_none() {
            return Ok(());
        }
        // Reject invalid or oversized requested operations before touching disk.
        if let Some(record) = replacement {
            self.validate(record)?;
            if Self::encode([0; 32], record)?.1.len() as u64 > MAX_RECORD {
                return Err("crawl journal byte budget exhausted".into());
            }
        }
        self.poisoned = true;
        let path = self.directory.join(NEXT);
        let mut output = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("create compact journal: {e}"))?;
        let mut temporary = Temporary {
            path,
            published: false,
        };
        output
            .write_all(FORMAT_MAGIC)
            .map_err(|e| format!("write compact journal header: {e}"))?;
        #[cfg(test)]
        super::tests::publication_stage(&self.directory, "header");
        let mut digest = [0; 32];
        let mut total = FORMAT_MAGIC.len() as u64;
        let reset = self.reset.as_ref().map(|(seeds, scope)| Record::Reset {
            seeds: seeds.clone(),
            scope: scope.clone(),
        });
        if let Some(record) = replacement.or(reset.as_ref()) {
            Self::write_compact_record(&mut output, &mut digest, &mut total, record)?;
        } else {
            Self::write_compact_record(
                &mut output,
                &mut digest,
                &mut total,
                &Record::Snapshot {
                    version: 2,
                    checkpoint: self.checkpoint.unwrap(),
                    tasks: self.tasks.len(),
                },
            )?;
            #[cfg(test)]
            super::tests::publication_stage(&self.directory, "snapshot");
            // Each URL is at most 8192 bytes; bounded chunks leave room for JSON
            // escaping and frame overhead without materializing the whole log.
            for tasks in self.tasks.chunks(64) {
                Self::write_compact_record(
                    &mut output,
                    &mut digest,
                    &mut total,
                    &Record::RestoreTasks {
                        tasks: tasks.to_vec(),
                    },
                )?;
            }
        }
        Self::write_compact_record(&mut output, &mut digest, &mut total, &Record::Seal)?;
        output
            .sync_all()
            .map_err(|e| format!("sync compact journal: {e}"))?;
        #[cfg(test)]
        super::tests::publication_stage(&self.directory, "synced");
        std::fs::rename(&temporary.path, self.directory.join("crawl.journal"))
            .map_err(|e| format!("publish compact journal: {e}"))?;
        temporary.published = true;
        // Even an ambiguous directory-sync failure must never append to the old,
        // unlinked inode. Keep the owner poisoned until the caller reopens.
        self.file = output;
        #[cfg(test)]
        super::tests::publication_stage(&self.directory, "renamed");
        File::open(&self.directory)
            .and_then(|f| f.sync_all())
            .map_err(|e| format!("sync compact journal directory: {e}"))?;
        #[cfg(test)]
        super::tests::publication_stage(&self.directory, "directory-synced");
        if let Some(record) = replacement.or(reset.as_ref()) {
            self.tasks.clear();
            self.positions.clear();
            self.checkpoint = None;
            self.reset = None;
            self.apply(record);
        }
        self.digest = digest;
        self.bytes = total;
        self.next_compaction = total.saturating_add(COMPACT_STEP);
        self.poisoned = false;
        Ok(())
    }
}
