use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Read file contents at a specific offset and length.
///
/// The length is client-supplied, so the buffer is bounded by the bytes left in the file
/// (itself at most `max_file_size`) rather than allocated at the requested size. A reply is
/// short only at end of file: clients read a short reply as the end of the stream.
pub fn read_file_range(
    path: &Path,
    offset: u64,
    length: u32,
    max_file_size: u64,
) -> anyhow::Result<Vec<u8>> {
    let meta = fs::metadata(path)?;
    if meta.len() > max_file_size {
        anyhow::bail!("File exceeds maximum size limit");
    }

    let len = u64::from(length).min(meta.len().saturating_sub(offset));

    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity(len as usize);
    file.take(len).read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_range_basic() {
        let dir = std::env::temp_dir().join(format!("macrdp-file-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test.bin");
        std::fs::write(&path, b"0123456789").unwrap();

        let data = read_file_range(&path, 3, 4, 1024).unwrap();
        assert_eq!(data, b"3456");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_range_clamps_oversized_request() {
        let dir = std::env::temp_dir().join(format!("macrdp-file-test3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("small.bin");
        std::fs::write(&path, b"0123456789").unwrap();

        assert_eq!(read_file_range(&path, 8, u32::MAX, 1024).unwrap(), b"89");
        assert!(read_file_range(&path, 20, u32::MAX, 1024).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_range_exceeds_max() {
        let dir = std::env::temp_dir().join(format!("macrdp-file-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("big.bin");
        std::fs::write(&path, vec![0u8; 200]).unwrap();

        let result = read_file_range(&path, 0, 100, 50);
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
