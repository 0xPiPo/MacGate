use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use ironrdp_cliprdr::pdu::*;

use crate::file::read_file_range;


/// Bytes asked for per DATA request; larger files are fetched in successive chunks.
const INCOMING_CHUNK: u64 = 4 * 1024 * 1024;

pub struct FileTransferManager {
    local_files: Vec<PathBuf>,
    incoming_descriptors: Vec<FileDescriptor>,
    incoming_data: HashMap<u32, Vec<u8>>,
    incoming_complete: HashSet<u32>,
    /// Outstanding DATA request stream id -> (file index, bytes requested).
    stream_to_index: HashMap<u32, (u32, u32)>,
    temp_dir: PathBuf,
    max_file_size: u64,
    next_stream_id: u32,
}

impl FileTransferManager {
    pub fn new(temp_dir: PathBuf, max_file_size: u64) -> Self {
        use std::os::unix::fs::DirBuilderExt;
        // Owner-only: pasted files can be private.
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&temp_dir).ok();
        Self {
            local_files: Vec::new(),
            incoming_descriptors: Vec::new(),
            incoming_data: HashMap::new(),
            incoming_complete: HashSet::new(),
            stream_to_index: HashMap::new(),
            temp_dir,
            max_file_size,
            next_stream_id: 1,
        }
    }

    /// Largest file or clipboard payload accepted from or offered to the client.
    pub fn max_file_size(&self) -> u64 {
        self.max_file_size
    }

    pub fn set_local_files(&mut self, paths: Vec<PathBuf>) {
        self.local_files = paths;
    }

    pub fn build_file_list(&self) -> PackedFileList {
        let files = self
            .local_files
            .iter()
            .filter_map(|path| {
                let meta = fs::metadata(path).ok()?;
                let name = path.file_name()?.to_string_lossy().to_string();
                Some(FileDescriptor {
                    name,
                    file_size: Some(meta.len()),
                    attributes: None,
                    last_write_time: None,
                })
            })
            .collect();
        PackedFileList { files }
    }

    pub fn handle_contents_request(
        &mut self,
        request: &FileContentsRequest,
    ) -> FileContentsResponse<'static> {
        let index = request.index;
        let stream_id = request.stream_id;

        let Some(path) = self.local_files.get(index as usize) else {
            tracing::warn!(index, "File contents request for invalid file index");
            return FileContentsResponse::new_error(stream_id);
        };

        if request.flags.contains(FileContentsFlags::SIZE) {
            match fs::metadata(path) {
                Ok(meta) => {
                    if meta.len() > self.max_file_size {
                        tracing::warn!(
                            path = %path.display(),
                            size = meta.len(),
                            max = self.max_file_size,
                            "File exceeds size limit"
                        );
                        return FileContentsResponse::new_error(stream_id);
                    }
                    FileContentsResponse::new_size_response(stream_id, meta.len())
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "Failed to read file metadata");
                    FileContentsResponse::new_error(stream_id)
                }
            }
        } else if request.flags.contains(FileContentsFlags::DATA) {
            match read_file_range(path, request.position, request.requested_size, self.max_file_size)
            {
                Ok(data) => FileContentsResponse::new_data_response(stream_id, data),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "Failed to read file range");
                    FileContentsResponse::new_error(stream_id)
                }
            }
        } else {
            tracing::warn!("Unknown FileContentsRequest flags");
            FileContentsResponse::new_error(stream_id)
        }
    }

    pub fn set_incoming_descriptors(&mut self, descriptors: Vec<FileDescriptor>) {
        self.incoming_data.clear();
        self.incoming_complete.clear();
        self.stream_to_index.clear();
        self.incoming_descriptors = descriptors;
    }

    pub fn generate_contents_requests(&mut self) -> Vec<FileContentsRequest> {
        let mut requests = Vec::new();

        for index in 0..self.incoming_descriptors.len() {
            let desc = &self.incoming_descriptors[index];
            // `None`: the client did not send FD_FILESIZE; read until a short reply.
            let size = desc.file_size;
            if size.is_some_and(|size| size > self.max_file_size) {
                tracing::warn!(
                    name = ?desc.name,
                    size = desc.file_size,
                    max = self.max_file_size,
                    "Skipping incoming file over the size limit"
                );
                // Counted as done so the other files still complete.
                self.incoming_complete.insert(index as u32);
                continue;
            }
            let is_dir = desc
                .attributes
                .is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY));
            if is_dir {
                tracing::debug!(name = ?desc.name, "Skipping directory entry in incoming file list");
                self.incoming_complete.insert(index as u32);
                continue;
            }
            // Entries inside a copied folder (`folder\\a.txt`) are not pasted: skip them before
            // transferring their data, not after.
            if safe_file_name(&desc.name).is_none() {
                tracing::warn!(name = ?desc.name, "Skipping incoming file with a nested or unsafe path");
                self.incoming_complete.insert(index as u32);
                continue;
            }
            if size == Some(0) {
                // Nothing to fetch; flush still writes the empty file.
                self.incoming_data.insert(index as u32, Vec::new());
                self.incoming_complete.insert(index as u32);
                continue;
            }
            // The descriptor already carries the size, so no SIZE request is needed.
            requests.push(self.data_request(index as u32, 0, size));
        }

        requests
    }

    /// Next DATA request for `index`, starting at `position` of a file of `size` bytes
    /// (`None`: unknown, so a full chunk).
    fn data_request(&mut self, index: u32, position: u64, size: Option<u64>) -> FileContentsRequest {
        let stream_id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.wrapping_add(1);
        // Bounded by INCOMING_CHUNK, so the cast cannot truncate.
        let requested_size = size.map_or(INCOMING_CHUNK, |size| size.saturating_sub(position).min(INCOMING_CHUNK)) as u32;
        self.stream_to_index.insert(stream_id, (index, requested_size));
        FileContentsRequest {
            stream_id,
            index,
            flags: FileContentsFlags::DATA,
            position,
            requested_size,
            data_id: None,
        }
    }

    /// Records a DATA response. Returns the request for the next chunk while the file is
    /// still incomplete.
    pub fn handle_contents_response(&mut self, response: &FileContentsResponse<'_>) -> Option<FileContentsRequest> {
        let stream_id = response.stream_id();
        let Some((file_index, requested)) = self.stream_to_index.remove(&stream_id) else {
            tracing::warn!(stream_id, "Unknown stream_id in file contents response");
            return None;
        };

        if self.incoming_complete.contains(&file_index) {
            return None;
        }

        let data = response.data();
        let expected = self.incoming_descriptors.get(file_index as usize).and_then(|d| d.file_size);
        if data.is_empty() && expected.is_none() {
            // Unknown size: an empty reply is the end of the file.
            self.incoming_data.entry(file_index).or_default();
            self.incoming_complete.insert(file_index);
            return None;
        }
        if data.is_empty() {
            // An error response (or a truncated file): give up on this file so the rest of
            // the paste still completes.
            tracing::warn!(stream_id, file_index, "Empty data in file contents response, dropping file");
            self.incoming_data.remove(&file_index);
            self.incoming_complete.insert(file_index);
            return None;
        }

        let received = self.incoming_data.get(&file_index).map_or(0, Vec::len);
        if (received + data.len()) as u64 > self.max_file_size {
            tracing::warn!(file_index, max = self.max_file_size, "Incoming file exceeds the size limit, discarding it");
            self.incoming_data.remove(&file_index);
            self.incoming_complete.insert(file_index);
            return None;
        }
        let buf = self.incoming_data.entry(file_index).or_default();
        buf.extend_from_slice(data);
        let received = buf.len() as u64;

        // Known size: done once it all arrived. Unknown: a short reply is the end of the file.
        let done = match expected {
            Some(expected) => received >= expected,
            None => data.len() < requested as usize,
        };
        if done {
            self.incoming_complete.insert(file_index);
            None
        } else {
            Some(self.data_request(file_index, received, expected))
        }
    }

    pub fn all_incoming_complete(&self) -> bool {
        !self.incoming_descriptors.is_empty()
            && self.incoming_complete.len() == self.incoming_descriptors.len()
    }

    pub fn flush_incoming_files(&mut self) -> Vec<PathBuf> {
        let mut paths = Vec::new();

        for (index, desc) in self.incoming_descriptors.iter().enumerate() {
            let index = index as u32;
            if let Some(data) = self.incoming_data.remove(&index) {
                let Some(file_name) = safe_file_name(&desc.name) else {
                    tracing::warn!(name = ?desc.name, "Rejected file with suspicious path");
                    continue;
                };

                let file_path = self.temp_dir.join(file_name);
                // create_new: never follow or overwrite whatever already sits at that name.
                let _ = fs::remove_file(&file_path);
                let written = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&file_path)
                    .and_then(|mut f| std::io::Write::write_all(&mut f, &data));
                match written {
                    Ok(()) => {
                        tracing::debug!(path = ?file_path, bytes = data.len(), "Wrote incoming file");
                        paths.push(file_path);
                    }
                    Err(e) => {
                        tracing::warn!(path = ?file_path, error = %e, "Failed to write incoming file");
                    }
                }
            }
        }

        self.incoming_descriptors.clear();
        self.incoming_complete.clear();
        self.stream_to_index.clear();
        paths
    }

    /// Abandons an incoming paste (a newer copy superseded it). Replies still on the way
    /// then match no stream and are dropped.
    pub fn cancel_incoming(&mut self) {
        self.incoming_descriptors.clear();
        self.incoming_data.clear();
        self.incoming_complete.clear();
        self.stream_to_index.clear();
    }
}

/// A client-supplied name reduced to one plain path component, or `None` when it names a
/// directory, a parent, or an absolute path.
fn safe_file_name(name: &str) -> Option<&str> {
    use std::path::{Component, Path};
    if name.contains(['/', '\\', '\0']) {
        return None;
    }
    match Path::new(name).components().collect::<Vec<_>>().as_slice() {
        [Component::Normal(_)] => Some(name),
        _ => None,
    }
}

impl Drop for FileTransferManager {
    fn drop(&mut self) {
        // The directory is private to this session (see `MacClipboardFactory`).
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "macrdp-transfer-test-{}-{}",
            std::process::id(),
            name,
        ));
        fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn handle_size_request() {
        let dir = temp_dir("size");
        let file_path = dir.join("test.txt");
        fs::write(&file_path, b"hello world").unwrap();

        let mut mgr = FileTransferManager::new(dir.clone(), 1024 * 1024);
        mgr.set_local_files(vec![file_path]);

        let request = FileContentsRequest {
            stream_id: 1,
            index: 0,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: None,
        };

        let response = mgr.handle_contents_request(&request);
        assert_eq!(response.stream_id(), 1);
        let size = response.data_as_size().unwrap();
        assert_eq!(size, 11);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn handle_data_request() {
        let dir = temp_dir("data");
        let file_path = dir.join("data.bin");
        fs::write(&file_path, b"ABCDEF").unwrap();

        let mut mgr = FileTransferManager::new(dir.clone(), 1024 * 1024);
        mgr.set_local_files(vec![file_path]);

        let request = FileContentsRequest {
            stream_id: 2,
            index: 0,
            flags: FileContentsFlags::DATA,
            position: 2,
            requested_size: 3,
            data_id: None,
        };

        let response = mgr.handle_contents_request(&request);
        assert_eq!(response.data(), b"CDE");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn size_limit_enforcement() {
        let dir = temp_dir("limit");
        let file_path = dir.join("big.bin");
        fs::write(&file_path, vec![0u8; 200]).unwrap();

        let mut mgr = FileTransferManager::new(dir.clone(), 100);
        mgr.set_local_files(vec![file_path]);

        let request = FileContentsRequest {
            stream_id: 3,
            index: 0,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: None,
        };

        let response = mgr.handle_contents_request(&request);
        assert!(response.data().is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generate_and_handle_incoming() {
        let dir = temp_dir("incoming");
        let mut mgr = FileTransferManager::new(dir.clone(), 1024 * 1024);

        let descriptors = vec![
            FileDescriptor {
                name: "a.txt".to_string(),
                file_size: Some(5),
                attributes: None,
                last_write_time: None,
            },
            FileDescriptor {
                name: "b.txt".to_string(),
                file_size: Some(3),
                attributes: None,
                last_write_time: None,
            },
        ];
        mgr.set_incoming_descriptors(descriptors);

        let requests = mgr.generate_contents_requests();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|r| r.flags.contains(FileContentsFlags::DATA)));

        let data_stream_a = requests[0].stream_id;
        let data_stream_b = requests[1].stream_id;

        let resp_a = FileContentsResponse::new_data_response(data_stream_a, b"hello".to_vec());
        mgr.handle_contents_response(&resp_a);
        assert!(!mgr.all_incoming_complete());

        let resp_b = FileContentsResponse::new_data_response(data_stream_b, b"bye".to_vec());
        mgr.handle_contents_response(&resp_b);
        assert!(mgr.all_incoming_complete());

        let paths = mgr.flush_incoming_files();
        assert_eq!(paths.len(), 2);
        assert_eq!(fs::read_to_string(&paths[0]).unwrap(), "hello");
        assert_eq!(fs::read_to_string(&paths[1]).unwrap(), "bye");

        fs::remove_dir_all(&dir).ok();
    }

    fn descriptor(name: &str, size: u64) -> FileDescriptor {
        FileDescriptor { name: name.to_string(), file_size: Some(size), attributes: None, last_write_time: None }
    }

    #[test]
    fn oversized_incoming_file_is_not_requested() {
        let dir = temp_dir("oversized");
        let mut mgr = FileTransferManager::new(dir.clone(), 100);
        mgr.set_incoming_descriptors(vec![descriptor("big.bin", 200), descriptor("small.txt", 3)]);

        let requests = mgr.generate_contents_requests();
        assert!(requests.iter().all(|r| r.index == 1), "only the small file is requested");

        let data = requests.iter().find(|r| r.flags.contains(FileContentsFlags::DATA)).unwrap();
        mgr.handle_contents_response(&FileContentsResponse::new_data_response(data.stream_id, b"bye".to_vec()));
        assert!(mgr.all_incoming_complete());

        let paths = mgr.flush_incoming_files();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("small.txt"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn incoming_data_beyond_cap_is_discarded() {
        let dir = temp_dir("beyond-cap");
        let mut mgr = FileTransferManager::new(dir.clone(), 4);
        // The client declares 3 bytes, then sends more than the cap.
        mgr.set_incoming_descriptors(vec![descriptor("liar.bin", 3)]);
        let requests = mgr.generate_contents_requests();
        let data = requests.iter().find(|r| r.flags.contains(FileContentsFlags::DATA)).unwrap();

        mgr.handle_contents_response(&FileContentsResponse::new_data_response(data.stream_id, vec![7u8; 10]));
        assert!(mgr.incoming_data.is_empty());
        assert!(mgr.all_incoming_complete());
        assert!(mgr.flush_incoming_files().is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_and_directory_entries_complete_without_requests() {
        let dir = temp_dir("empty");
        let mut mgr = FileTransferManager::new(dir.clone(), 1024);
        let folder = FileDescriptor {
            name: "folder".to_string(),
            file_size: Some(0),
            attributes: Some(ClipboardFileAttributes::DIRECTORY),
            last_write_time: None,
        };
        mgr.set_incoming_descriptors(vec![folder, descriptor("empty.txt", 0)]);

        assert!(mgr.generate_contents_requests().is_empty());
        assert!(mgr.all_incoming_complete());
        let paths = mgr.flush_incoming_files();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("empty.txt"));
        assert_eq!(fs::read(&paths[0]).unwrap().len(), 0);
    }

    #[test]
    fn short_reply_requests_the_rest() {
        let dir = temp_dir("short");
        let mut mgr = FileTransferManager::new(dir.clone(), 1024);
        mgr.set_incoming_descriptors(vec![descriptor("a.txt", 5)]);
        let requests = mgr.generate_contents_requests();
        let data = requests.iter().find(|r| r.flags.contains(FileContentsFlags::DATA)).unwrap();

        let next = mgr
            .handle_contents_response(&FileContentsResponse::new_data_response(data.stream_id, b"he".to_vec()))
            .expect("follow-up request");
        assert_eq!((next.position, next.requested_size), (2, 3));
        assert!(!mgr.all_incoming_complete());

        let done = mgr.handle_contents_response(&FileContentsResponse::new_data_response(next.stream_id, b"llo".to_vec()));
        assert!(done.is_none());
        let paths = mgr.flush_incoming_files();
        assert_eq!(fs::read_to_string(&paths[0]).unwrap(), "hello");
    }

    #[test]
    fn unknown_size_reads_until_a_short_reply() {
        let dir = temp_dir("unknown-size");
        let mut mgr = FileTransferManager::new(dir.clone(), 64 * 1024 * 1024);
        mgr.set_incoming_descriptors(vec![FileDescriptor {
            name: "a.bin".to_string(),
            file_size: None,
            attributes: None,
            last_write_time: None,
        }]);
        let first = mgr.generate_contents_requests().pop().expect("a request despite no size");
        let chunk = vec![1u8; first.requested_size as usize];
        let next = mgr
            .handle_contents_response(&FileContentsResponse::new_data_response(first.stream_id, chunk))
            .expect("a full chunk asks for more");
        assert_eq!(next.position, u64::from(first.requested_size));
        assert!(mgr.handle_contents_response(&FileContentsResponse::new_data_response(next.stream_id, b"end".to_vec())).is_none());
        assert!(mgr.all_incoming_complete());
        let paths = mgr.flush_incoming_files();
        assert_eq!(fs::read(&paths[0]).unwrap().len(), first.requested_size as usize + 3);
    }

    #[test]
    fn nested_folder_entries_are_never_requested() {
        let dir = temp_dir("nested");
        let mut mgr = FileTransferManager::new(dir.clone(), 1024);
        mgr.set_incoming_descriptors(vec![descriptor("folder\\a.txt", 5), descriptor("../b.txt", 5)]);
        assert!(mgr.generate_contents_requests().is_empty());
        assert!(mgr.all_incoming_complete());
        assert!(mgr.flush_incoming_files().is_empty());
    }

    #[test]
    fn failed_reply_drops_only_that_file() {
        let dir = temp_dir("failed");
        let mut mgr = FileTransferManager::new(dir.clone(), 1024);
        mgr.set_incoming_descriptors(vec![descriptor("a.txt", 5)]);
        let requests = mgr.generate_contents_requests();
        let data = requests.iter().find(|r| r.flags.contains(FileContentsFlags::DATA)).unwrap();

        assert!(mgr.handle_contents_response(&FileContentsResponse::new_error(data.stream_id)).is_none());
        assert!(mgr.all_incoming_complete());
        assert!(mgr.flush_incoming_files().is_empty());
    }

    #[test]
    fn file_names_are_single_components() {
        assert_eq!(safe_file_name("a..b.txt"), Some("a..b.txt"));
        assert_eq!(safe_file_name("report.pdf"), Some("report.pdf"));
        for bad in ["..", ".", "", "../x", "a/b", "a\\b", "/etc/passwd"] {
            assert_eq!(safe_file_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_cancelled_paste_ignores_late_replies() {
        let dir = temp_dir("cancel");
        let mut mgr = FileTransferManager::new(dir.clone(), 1024 * 1024);
        mgr.set_local_files(vec![PathBuf::from("/tmp/fake")]);
        mgr.set_incoming_descriptors(vec![descriptor("a.txt", 5)]);
        let request = mgr.generate_contents_requests().pop().unwrap();
        mgr.cancel_incoming();
        let late = FileContentsResponse::new_data_response(request.stream_id, b"hello".to_vec());
        assert!(mgr.handle_contents_response(&late).is_none());
        assert!(!mgr.all_incoming_complete(), "nothing left to paste");
        assert_eq!(mgr.local_files.len(), 1, "the Mac's own copy is untouched");

        fs::remove_dir_all(&dir).ok();
    }
}
