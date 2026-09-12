// Included inside Ringboard's allocator module. This extension is AGPL-3.0-only,
// like the server it modifies. It does not change the legacy protocol layout.
impl Allocator {
    pub fn policy_request(&mut self, request: &[u8], fd: Option<OwnedFd>) -> u8 {
        // CDP1 | op:u8 | raw_id:u64 LE | expected_proof:32 | mime_len:u8 | mime
        if request.len() < 46 || &request[..4] != b"CDP1" || request[4] != 1 {
            return 2;
        }
        let id = u64::from_le_bytes(request[5..13].try_into().unwrap());
        let expected: [u8; 32] = request[13..45].try_into().unwrap();
        let length = usize::from(request[45]);
        if request.len() != 46 + length || length > 96 {
            return 2;
        }
        let Some(mime) = std::str::from_utf8(&request[46..]).ok()
            .and_then(|mime| MimeType::from(mime).ok()) else { return 2 };
        let Some(fd) = fd else { return 2 };
        if self.policy_proof(id).ok() != Some(expected) {
            return 1;
        }
        match self.policy_replace(id, fd, &mime) {
            Ok(()) => 0,
            Err(error) => {
                // Do not log clipboard contents or request payloads.
                log::error!("Policy replacement failed: {error}");
                2
            }
        }
    }

    fn policy_proof(&self, id: u64) -> Result<[u8; 32], CliError> {
        use sha2::{Digest, Sha256};
        let mut directory = ringboard_core::dirs::data_dir();
        let database = ringboard_sdk::DatabaseReader::open(&mut directory)?;
        let entry = database.get_raw(id).map_err(|_| CliError::Internal {
            context: "Stale policy entry".into(),
        })?;
        let mut reader = ringboard_sdk::EntryReader::open(&mut directory)?;
        let mut file = entry.to_file(&mut reader)?;
        let mut mime = [0; 255];
        let length = match rustix::fs::fgetxattr(&*file, c"user.mime_type", &mut mime) {
            Ok(length) => length,
            Err(Errno::NODATA) => 0,
            Err(error) => return Err(ringboard_core::Error::Io {
                error: error.into(), context: "Could not read policy entry MIME".into(),
            }.into()),
        };
        let mut content = Sha256::new();
        content.update(b"clip-daemon:entry-content:v1:");
        io::copy(&mut *file, &mut content).map_io_err(|| "Could not hash policy entry")?;
        let mut proof = Sha256::new();
        proof.update(b"clip-daemon:proof:v1:");
        proof.update(content.finalize());
        proof.update(&mime[..length]);
        Ok(proof.finalize().into())
    }

    fn policy_replace(&mut self, id: u64, fd: OwnedFd, mime: &MimeType) -> Result<(), CliError> {
        let (ring, index, previous) = self.get_entry(id).map_err(|_| CliError::Internal {
            context: "Stale policy replacement".into(),
        })?;
        if previous == Entry::Uninitialized || self.data.metadata_dir.is_some() {
            return Err(CliError::Internal {
                context: "Policy replacement requires an existing entry and MIME xattr support".into(),
            });
        }
        // The extra slot is never published in the ring, counted toward retention,
        // or used by capture. Requests execute serially inside the server reactor.
        let staging = self.rings[ring].ring.capacity();
        let _ = self.data.free_direct(ring, staging); // retry prior orphan cleanup
        self.data.scratchpad.set_len(0).map_io_err(|| "Reset replacement staging")?;
        self.data.scratchpad.seek(SeekFrom::Start(0)).map_io_err(|| "Rewind replacement staging")?;
        let mut input = File::from(fd);
        if !input.metadata().map_io_err(|| "Inspect replacement")?.is_file() {
            return Err(CliError::Internal { context: "Replacement must be a regular file".into() });
        }
        const LIMIT: u64 = 64 * 1024 * 1024;
        let size = io::copy(&mut std::io::Read::by_ref(&mut input).take(LIMIT + 1), &mut self.data.scratchpad)
            .map_io_err(|| "Stage policy replacement")?;
        if size > LIMIT {
            self.data.scratchpad.seek(SeekFrom::Start(0)).map_io_err(|| "Reset oversized staging")?;
            return Err(CliError::Internal { context: "Replacement exceeds the policy limit".into() });
        }
        self.data.scratchpad.set_len(size).map_io_err(|| "Size replacement staging")?;
        self.data.scratchpad.sync_all().map_io_err(|| "Sync replacement staging")?;
        self.data.alloc_direct(size, mime, ring, staging)?;
        let mut from = [MaybeUninit::uninit(); 14];
        let from = direct_file_name(&mut from, ring, staging);
        let mut to = [MaybeUninit::uninit(); 14];
        let to = direct_file_name(&mut to, ring, index);
        let flags = if previous == Entry::File { RenameFlags::EXCHANGE } else { RenameFlags::empty() };
        if let Err(error) = renameat_with(&self.data.direct_dir, from, &self.data.direct_dir, to, flags)
            .map_io_err(|| "Install policy replacement") {
            let _ = self.data.free_direct(ring, staging);
            return Err(error.into());
        }
        if let Err(error) = self.rings[ring].writer.write(Entry::File, index) {
            // Preserve the original allocation if publishing the new slot fails.
            renameat_with(&self.data.direct_dir, to, &self.data.direct_dir, from, flags)
                .map_io_err(|| "Roll back policy replacement")?;
            let _ = self.data.free_direct(ring, staging);
            return Err(error.into());
        }
        if let Err(error) = self.data.free(previous, ring, staging) {
            log::warn!("Replacement committed; old allocation cleanup will be retried: {error}");
        }
        Ok(())
    }
}
