//! Bounded reception through org.freedesktop.portal.FileTransfer.
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub const FILE_TRANSFER_MIME: &str = "application/vnd.portal.filetransfer";
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
struct Permit;
impl Permit {
    fn acquire() -> Result<Self, String> {
        let mut active = ACTIVE.load(Ordering::Acquire);
        while active < 8 {
            match ACTIVE.compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(Self),
                Err(current) => active = current,
            }
        }
        Err("Too many file-transfer portal requests".into())
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Decode a clipboard transfer key. A terminating NUL is accepted; embedded NULs are not.
pub fn file_transfer_key(bytes: &[u8]) -> Result<&str, String> {
    if bytes.len() > 4096 {
        return Err("File-transfer key is too large".into());
    }
    let key = std::str::from_utf8(bytes).map_err(|e| e.to_string())?.trim_end_matches('\0').trim();
    if key.is_empty() || key.contains('\0') {
        return Err("Invalid file-transfer key".into());
    }
    Ok(key)
}

/// Retrieve accessible local paths, with a five-second total deadline including connection setup.
pub fn retrieve_files(key: &str) -> Result<Vec<PathBuf>, String> {
    let _permit = Permit::acquire()?;
    if key.is_empty() || key.len() > 4096 || key.contains('\0') {
        return Err("Invalid file-transfer key".into());
    }
    async_io::block_on(futures_lite::future::race(retrieve_async(key), async {
        async_io::Timer::after(Duration::from_secs(5)).await;
        Err("file-transfer portal timed out".into())
    }))
}
async fn retrieve_async(key: &str) -> Result<Vec<PathBuf>, String> {
    let connection = zbus::connection::Builder::session()
        .map_err(|error| error.to_string())?
        .method_timeout(Duration::from_secs(5))
        .build()
        .await
        .map_err(|error| error.to_string())?;
    let options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> = std::collections::HashMap::new();
    let reply = connection
        .call_method(
            Some("org.freedesktop.portal.Documents"),
            "/org/freedesktop/portal/documents",
            Some("org.freedesktop.portal.FileTransfer"),
            "RetrieveFiles",
            &(key, options),
        )
        .await
        .map_err(|error| error.to_string())?;
    let paths = reply.body().deserialize::<Vec<String>>().map_err(|error| error.to_string())?;
    validate(paths)
}
fn validate(paths: Vec<String>) -> Result<Vec<PathBuf>, String> {
    if paths.len() > 4096 {
        return Err("file-transfer portal returned too many files".into());
    }
    let mut total = 0usize;
    let mut result = Vec::with_capacity(paths.len());
    for path in paths {
        total = total.saturating_add(path.len());
        if path.contains('\0') || path.len() > 65536 || total > 16 << 20 || !PathBuf::from(&path).is_absolute() {
            return Err("file-transfer portal returned invalid paths".into());
        }
        result.push(PathBuf::from(path));
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clipboard_keys_reject_malformed_or_unbounded_data() {
        assert_eq!(file_transfer_key(b"transfer-key\0"), Ok("transfer-key"));
        for bytes in [Vec::new(), vec![b'a'; 4097], b"a\0b".to_vec(), vec![255]] {
            assert!(file_transfer_key(&bytes).is_err());
        }
    }

    #[test]
    fn validates_portal_paths_without_delivering_partial_results() {
        assert_eq!(
            validate(vec!["/tmp/one space.png".into(), "/tmp/β.png".into()]),
            Ok(vec![PathBuf::from("/tmp/one space.png"), PathBuf::from("/tmp/β.png")])
        );
        for paths in [vec!["relative.png".into()], vec!["/tmp/a\0b".into()], vec!["/tmp/valid".into(), "invalid".into()], vec!["/tmp/a".into(); 4097]]
        {
            assert!(validate(paths).is_err());
        }
    }
}
