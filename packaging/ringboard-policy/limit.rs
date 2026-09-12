// Shared by the patched Ringboard server and Wayland watcher.
use std::{fs::File, io::{self, Read}, path::Path};

pub fn load(path: &Path) -> io::Result<u64> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(16 * 1024 * 1024),
        Err(error) => return Err(error),
    };
    let mut value = String::new();
    file.take(33).read_to_string(&mut value)?;
    let size = value.trim().parse::<u64>().ok()
        .filter(|size| value.len() <= 32 && (64 * 1024..=512 * 1024 * 1024).contains(size))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid clipboard capture byte limit"))?;
    Ok(size.min(64 * 1024 * 1024))
}
