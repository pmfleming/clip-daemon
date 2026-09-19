// Included inside Ringboard's allocator module. This extension is AGPL-3.0-only,
// like the server it modifies. It does not change the legacy protocol layout.
impl Allocator {
    pub fn policy_limits(&self) -> [u8; 20] {
        let mut response = [0; 20];
        response[..4].copy_from_slice(b"CDS1");
        response[4..8].copy_from_slice(&self.rings[RingKind::Main].ring.capacity().to_le_bytes());
        response[8..12].copy_from_slice(&self.rings[RingKind::Favorites].ring.capacity().to_le_bytes());
        let limit = capture_limit::load(&ringboard_core::dirs::data_dir().join("clip-daemon-max-bytes")).unwrap_or(0);
        response[12..20].copy_from_slice(&limit.to_le_bytes());
        response
    }

    pub fn policy_request(&mut self, request: &[u8], fd: Option<OwnedFd>) -> u8 {
        // CDP1 | op:u8 | raw_id:u64 LE | expected_proof:32 | mime_len:u8 | mime
        if request.len() < 46 || &request[..4] != b"CDP1" {
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
        let op = request[4];
        if op != 1 && length != 0 { return 2; }
        if matches!(op, 1..=4) && self.policy_proof(id).ok() != Some(expected) {
            return 1;
        }
        let result = (|| -> Result<u8, CliError> {
            match op {
                1 => {
                    let Some(fd) = fd else { return Ok(2) };
                    self.policy_replace(id, fd, &mime)?;
                }
                2 => { if self.remove(id)?.error.is_some() { return Ok(1); } }
                3 | 4 => {
                    let to = if op == 3 { RingKind::Favorites } else { RingKind::Main };
                    if matches!(self.move_to_front(id, Some(to))?, MoveToFrontResponse::Error(_)) { return Ok(1); }
                }
                5 => {
                    let ids: Vec<_> = [RingKind::Main, RingKind::Favorites].into_iter()
                        .flat_map(|kind| RingReader::from_ring(&self.rings[kind].ring, kind).map(|entry| entry.id()))
                        .collect();
                    for id in ids { if self.remove(id)?.error.is_some() { return Ok(2); } }
                }
                6 => {
                    let Some(fd) = fd else { return Ok(2) };
                    return self.policy_remove_many(fd);
                }
                _ => return Ok(2),
            }
            Ok(0)
        })();
        match result {
            Ok(status) => status,
            Err(error) => {
                // Do not log clipboard contents or request payloads.
                log::error!("Policy mutation failed: {error}");
                2
            }
        }
    }

    // v2 capture: one reactor turn admits bytes, validates a duplicate candidate,
    // and either promotes it in its existing ring or adds to main. Status 3 is
    // explicitly uncertain (I/O may have followed eviction); never retry blindly.
    pub fn policy_capture(&mut self, request: &[u8], fd: OwnedFd) -> [u8; 13] {
        let result = self.policy_capture_inner(request, fd);
        let mut response = [0; 13];
        response[..4].copy_from_slice(b"CDR1");
        match result {
            Ok(Some(id)) => response[5..].copy_from_slice(&id.to_le_bytes()),
            Ok(None) => response[4] = 2,
            Err(_) => {
                log::error!("Capture persistence outcome is uncertain");
                response[4] = 3;
            }
        }
        response
    }

    fn policy_capture_inner(&mut self, request: &[u8], fd: OwnedFd) -> Result<Option<u64>, CliError> {
        use sha2::{Digest, Sha256};
        if request.len() < 46 || &request[..5] != b"CDP1\x08" { return Ok(None); }
        let length = usize::from(request[45]);
        if length > 96 || request.len() != 46 + length { return Ok(None); }
        let Some(mime) = std::str::from_utf8(&request[46..]).ok()
            .and_then(|mime| MimeType::from(mime).ok()) else { return Ok(None) };
        let Some(fd) = self.policy_admit(fd)? else { return Ok(None) };
        let mut file = File::from(fd);
        let mut digest = Sha256::new();
        digest.update(b"clip-daemon:entry-content:v1:");
        io::copy(&mut file, &mut digest).map_io_err(|| "Hash admitted capture")?;
        file.seek(SeekFrom::Start(0)).map_io_err(|| "Rewind admitted capture")?;
        let mut hash = Sha256::new();
        hash.update(b"clip-daemon:proof:v1:");
        hash.update(digest.finalize());
        if !is_plaintext_mime(&mime) { hash.update(mime.as_bytes()); }
        let proof: [u8; 32] = hash.finalize().into();
        if request[13..45] != proof { return Ok(None); }
        let id = u64::from_le_bytes(request[5..13].try_into().unwrap());
        if id != u64::MAX && self.policy_proof(id).ok() == Some(proof) {
            if let MoveToFrontResponse::Success { id } = self.move_to_front(id, None)? {
                return Ok(Some(id));
            }
        }
        // Already admitted: never evict or stage before the bounded snapshot.
        let id = self.add_internal(RingKind::Main, |head, data| data.alloc(file.into(), &mime, RingKind::Main, head))?;
        Ok(Some(composite_id(RingKind::Main, id)))
    }

    fn policy_remove_many(&mut self, fd: OwnedFd) -> Result<u8, CliError> {
        let file = File::from(fd);
        if !file.metadata().map_io_err(|| "Inspect deletion targets")?.is_file() { return Ok(2); }
        let mut bytes = Vec::new();
        file.take(4 + 5000 * 40 + 1).read_to_end(&mut bytes).map_io_err(|| "Read deletion targets")?;
        if bytes.len() < 4 { return Ok(2); }
        let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        if !(1..=5000).contains(&count) || bytes.len() != 4 + count * 40 { return Ok(2); }
        let mut ids = std::collections::HashSet::new();
        for target in bytes[4..].chunks_exact(40) {
            let id = u64::from_le_bytes(target[..8].try_into().unwrap());
            let proof: [u8; 32] = target[8..].try_into().unwrap();
            if !ids.insert(id) || self.policy_proof(id).ok() != Some(proof) { return Ok(1); }
        }
        // All proofs are validated before any delete, in one reactor turn.
        for id in ids { if self.remove(id)?.error.is_some() { return Ok(2); } }
        Ok(0)
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

    fn policy_admit(&self, fd: OwnedFd) -> Result<Option<OwnedFd>, CliError> {
        let limit = capture_limit::load(&ringboard_core::dirs::data_dir().join("clip-daemon-max-bytes"))
            .map_io_err(|| "Read capture byte limit")?;
        let input = File::from(fd);
        let metadata = input.metadata().map_io_err(|| "Inspect incoming clipboard data")?;
        if !metadata.is_file() || metadata.len() > limit { return Ok(None); }
        let mut snapshot = File::from(rustix::fs::memfd_create(c"ringboard-admission", rustix::fs::MemfdFlags::CLOEXEC)
            .map_io_err(|| "Create memory-only clipboard admission buffer")?);
        let size = io::copy(&mut input.take(limit + 1), &mut snapshot)
            .map_io_err(|| "Read bounded clipboard admission data")?;
        if size > limit { return Ok(None); }
        snapshot.seek(SeekFrom::Start(0)).map_io_err(|| "Rewind clipboard admission data")?;
        Ok(Some(snapshot.into()))
    }

    fn policy_replace(&mut self, id: u64, fd: OwnedFd, mime: &MimeType) -> Result<(), CliError> {
        let fd = self.policy_admit(fd)?.ok_or_else(|| CliError::Internal {
            context: "Replacement exceeds the configured limit or is not a regular file".into(),
        })?;
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
        let size = io::copy(&mut File::from(fd), &mut self.data.scratchpad)
            .map_io_err(|| "Stage admitted policy replacement")?;
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
