//! Filesystem path utilities for the ion-agent cache directory.
//!
//! Provides helpers to resolve ion-related directories and compute
//! aggregate statistics (e.g. total cache size).

use std::fs;
use std::path::PathBuf;

/// Resolve the ion home directory (`~/.ion`).
///
/// Falls back to `~/.pi` when `~/.ion` does not exist, mirroring the
/// behaviour used elsewhere in the codebase for backwards compatibility.
pub fn ion_home() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let ion_path = PathBuf::from(&home).join(".ion");
    if ion_path.exists() {
        return ion_path;
    }
    PathBuf::from(&home).join(".pi").join("agent")
}

/// Resolve the cache directory (`~/.ion/agent/cache`).
pub fn cache_dir() -> PathBuf {
    ion_home().join("agent").join("cache")
}

/// Calculate the total size (in bytes) of the `~/.ion/agent/cache/` directory.
///
/// Walks the directory recursively and sums up the size of every regular
/// file. Returns an error message as `String` if the directory cannot be
/// read.
pub fn cache_size() -> Result<u64, String> {
    let dir = cache_dir();

    // If the cache directory does not exist yet, the size is zero.
    if !dir.exists() {
        return Ok(0);
    }

    let mut total: u64 = 0;

    // Collect entries manually so we can handle errors gracefully instead of
    // short-circuiting on the first permission issue.
    let mut stack: Vec<PathBuf> = vec![dir];

    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(e) => e,
            Err(err) => {
                // Skip directories we cannot read rather than failing entirely.
                tracing::warn!("cache_size: skipping unreadable dir {:?}: {}", current, err);
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };

            if file_type.is_file() {
                if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            } else if file_type.is_dir() {
                stack.push(path);
            }
        }
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_cache_size() {
        // Create a temporary directory that mimics the cache layout.
        let tmp = tempfile::tempdir().expect("failed to create temp dir");

        // Make the temp dir look like an ion home: create `.ion` marker so
        // `ion_home()` resolves to `<tmp>/.ion`.
        let ion_home_dir = tmp.path().join(".ion");
        fs::create_dir_all(&ion_home_dir).expect("mkdir .ion");

        // Build the expected cache sub-tree inside `.ion/agent/cache`.
        let cache_root = ion_home_dir.join("agent").join("cache");
        fs::create_dir_all(cache_root.join("sub")).expect("mkdir sub");

        // Write two known files so we can verify the summed size.
        let data_a = b"hello world"; // 11 bytes
        let data_b = b"abcdabcdabcd"; // 12 bytes

        let mut f1 = fs::File::create(cache_root.join("a.bin")).expect("create a.bin");
        f1.write_all(data_a).expect("write a.bin");

        let mut f2 = fs::File::create(cache_root.join("sub").join("b.bin")).expect("create b.bin");
        f2.write_all(data_b).expect("write b.bin");

        // Point HOME at the temp directory so `cache_dir()` resolves here.
        // SAFETY: this is a single-threaded test with no other code racing on
        // the HOME environment variable.
        unsafe { std::env::set_var("HOME", tmp.path()); }

        let size = cache_size().expect("cache_size should succeed");
        let expected = (data_a.len() + data_b.len()) as u64;
        assert_eq!(size, expected, "cache_size should sum all files recursively");
    }
}
