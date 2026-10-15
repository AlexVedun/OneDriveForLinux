//! Read-only FUSE adapter around a validated catalog snapshot and versioned cache.

use crate::{DownloadRequest, FileHandle as CachedHandle, Node, NodeKind, OpenError, VirtualTree};
use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use onedrive_cache::CacheStore;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);

pub type ContentLoader = dyn Fn(&DownloadRequest) -> io::Result<()> + Send + Sync;

struct OpenFile {
    inode: u64,
    handle: CachedHandle,
}

pub struct ReadOnlyFuse {
    tree: VirtualTree,
    cache_root: PathBuf,
    cache_limit: u64,
    loader: Arc<ContentLoader>,
    open_lock: Mutex<()>,
    files: Mutex<HashMap<u64, OpenFile>>,
    next_handle: AtomicU64,
    uid: u32,
    gid: u32,
}

impl ReadOnlyFuse {
    pub fn new(
        tree: VirtualTree,
        cache_root: PathBuf,
        cache_limit: u64,
        loader: Arc<ContentLoader>,
    ) -> Self {
        Self {
            tree,
            cache_root,
            cache_limit,
            loader,
            open_lock: Mutex::new(()),
            files: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        }
    }

    pub fn mount(self, mountpoint: &Path) -> io::Result<BackgroundSession> {
        let mut options = Config::default();
        options.mount_options = vec![
            MountOption::RO,
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::FSName("onedrive-linux".into()),
        ];
        options.n_threads = Some(1);
        fuser::spawn_mount(self, mountpoint, &options)
    }

    fn attr(&self, node: &Node) -> FileAttr {
        let directory = node.kind == NodeKind::Directory;
        FileAttr {
            ino: INodeNo(node.inode),
            size: node.size,
            blocks: node.size.saturating_add(511) / 512,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: if directory {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if directory { 0o500 } else { 0o400 },
            nlink: if directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn directory_entries(&self, inode: u64) -> Option<Vec<(u64, FileType, String)>> {
        let directory = self.tree.getattr(inode)?;
        let children = self.tree.readdir(inode)?;
        let mut entries = vec![
            (inode, FileType::Directory, ".".to_owned()),
            (directory.parent_inode, FileType::Directory, "..".to_owned()),
        ];
        entries.extend(children.into_iter().map(|node| {
            (
                node.inode,
                if node.kind == NodeKind::Directory {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                },
                node.name.clone(),
            )
        }));
        Some(entries)
    }

    fn open_file(&self, inode: u64, flags: OpenFlags) -> Result<u64, Errno> {
        if flags.acc_mode() != OpenAccMode::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
            return Err(Errno::EROFS);
        }
        let _serialized = self.open_lock.lock().unwrap();
        let mut cache =
            CacheStore::open(&self.cache_root, self.cache_limit).map_err(|_| Errno::EIO)?;
        let opened = match self.tree.open_cached(inode, &mut cache) {
            Ok(handle) => handle,
            Err(OpenError::NotCached(request)) => {
                drop(cache);
                (self.loader)(&request).map_err(map_loader_error)?;
                let mut cache =
                    CacheStore::open(&self.cache_root, self.cache_limit).map_err(|_| Errno::EIO)?;
                self.tree
                    .open_cached(inode, &mut cache)
                    .map_err(map_open_error)?
            }
            Err(error) => return Err(map_open_error(error)),
        };
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.files.lock().unwrap().insert(
            handle,
            OpenFile {
                inode,
                handle: opened,
            },
        );
        Ok(handle)
    }

    fn read_file(&self, inode: u64, handle: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let files = self.files.lock().unwrap();
        let open = files
            .get(&handle)
            .filter(|open| open.inode == inode)
            .ok_or(Errno::EBADF)?;
        open.handle.read(offset, size).map_err(|_| Errno::EIO)
    }
}

fn map_open_error(error: OpenError) -> Errno {
    match error {
        OpenError::MissingFile => Errno::ENOENT,
        OpenError::IsDirectory => Errno::EISDIR,
        _ => Errno::EIO,
    }
}

fn map_loader_error(error: io::Error) -> Errno {
    if matches!(
        error.kind(),
        io::ErrorKind::NotConnected
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::HostUnreachable
    ) {
        Errno::ENETDOWN
    } else {
        Errno::EIO
    }
}

impl Filesystem for ReadOnlyFuse {
    fn lookup(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.tree.lookup(parent.0, name) {
            Some(node) => reply.entry(&TTL, &self.attr(node), Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        _handle: Option<FileHandle>,
        reply: ReplyAttr,
    ) {
        match self.tree.getattr(inode.0) {
            Some(node) => reply.attr(&TTL, &self.attr(node)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _request: &Request,
        inode: INodeNo,
        _handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(entries) = self.directory_entries(inode.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let start = usize::try_from(offset)
            .unwrap_or(entries.len())
            .min(entries.len());
        for (index, (ino, kind, name)) in entries.into_iter().enumerate().skip(start) {
            if reply.add(INodeNo(ino), (index + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _request: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self.open_file(inode.0, flags) {
            Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::empty()),
            Err(error) => reply.error(error),
        }
    }

    fn read(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        match self.read_file(inode.0, handle.0, offset, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(error) => reply.error(error),
        }
    }

    fn release(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.files.lock().unwrap().remove(&handle.0);
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ROOT_INODE;
    use onedrive_catalog::CatalogItem;
    use std::fs;
    use std::sync::atomic::AtomicUsize;

    fn file_item() -> CatalogItem {
        CatalogItem {
            drive_id: "drive".into(),
            item_id: "file".into(),
            parent_id: Some("root".into()),
            name: Some("текст.txt".into()),
            kind: "file".into(),
            size: Some(5),
            etag: Some("v1".into()),
            ctag: None,
            modified: None,
            deleted: false,
        }
    }

    #[test]
    fn directory_and_read_only_open_use_cache_and_loader() {
        let dir = tempfile::tempdir().unwrap();
        let cache_root = dir.path().join("cache");
        let calls = Arc::new(AtomicUsize::new(0));
        let source = dir.path().join("source");
        fs::write(&source, b"hello").unwrap();
        let calls_for_loader = Arc::clone(&calls);
        let cache_for_loader = cache_root.clone();
        let loader: Arc<ContentLoader> = Arc::new(move |request| {
            calls_for_loader.fetch_add(1, Ordering::SeqCst);
            let mut cache = CacheStore::open(&cache_for_loader, 10).map_err(io::Error::other)?;
            cache
                .put(&request.cache_key(), &source, request.size)
                .map_err(io::Error::other)
        });
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = ReadOnlyFuse::new(tree, cache_root, 10, loader);
        let entries = fuse.directory_entries(ROOT_INODE).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.2.as_str())
                .collect::<Vec<_>>(),
            [".", "..", "текст.txt"]
        );
        let inode = fuse.tree.lookup(ROOT_INODE, "текст.txt").unwrap().inode;
        assert!(fuse.open_file(inode, OpenFlags(libc::O_WRONLY)).is_err());
        let handle = fuse.open_file(inode, OpenFlags(libc::O_RDONLY)).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(fuse.read_file(inode, handle, 1, 3).unwrap(), b"ell");
        assert!(fuse.read_file(ROOT_INODE, handle, 0, 1).is_err());
        fuse.files.lock().unwrap().remove(&handle);
        assert!(fuse.read_file(inode, handle, 0, 1).is_err());
        fuse.open_file(inode, OpenFlags(libc::O_RDONLY)).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unavailable_network_has_distinct_fuse_error() {
        assert_eq!(
            map_loader_error(io::ErrorKind::NotConnected.into()),
            Errno::ENETDOWN
        );
        assert_eq!(map_loader_error(io::Error::other("failed")), Errno::EIO);
    }

    #[test]
    fn mounted_tree_reads_cached_file_without_network() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let cache_root = dir.path().join("cache");
        let source = dir.path().join("source");
        fs::create_dir(&mountpoint).unwrap();
        fs::write(&source, b"hello").unwrap();
        let item = file_item();
        let mut on_demand = file_item();
        on_demand.item_id = "second".into();
        on_demand.name = Some("по-запросу.txt".into());
        let mut offline = file_item();
        offline.item_id = "offline".into();
        offline.name = Some("без-сети.txt".into());
        let tree = VirtualTree::from_items("drive", "root", [item, on_demand, offline]).unwrap();
        let request = tree.request_for_item("file").unwrap();
        let mut cache = CacheStore::open(&cache_root, 10).unwrap();
        cache.put(&request.cache_key(), &source, 5).unwrap();
        drop(cache);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_loader = Arc::clone(&calls);
        let cache_for_loader = cache_root.clone();
        let loader: Arc<ContentLoader> = Arc::new(move |request| {
            if request.item_id == "offline" {
                return Err(io::ErrorKind::NotConnected.into());
            }
            assert_eq!(request.item_id, "second");
            calls_for_loader.fetch_add(1, Ordering::SeqCst);
            let mut cache = CacheStore::open(&cache_for_loader, 10).map_err(io::Error::other)?;
            cache
                .put(&request.cache_key(), &source, request.size)
                .map_err(io::Error::other)
        });
        let session = ReadOnlyFuse::new(tree, cache_root, 10, loader)
            .mount(&mountpoint)
            .unwrap();
        let mut entries = fs::read_dir(&mountpoint)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            [
                std::ffi::OsString::from("без-сети.txt"),
                std::ffi::OsString::from("по-запросу.txt"),
                std::ffi::OsString::from("текст.txt")
            ]
        );
        assert_eq!(fs::read(mountpoint.join("текст.txt")).unwrap(), b"hello");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fs::read(mountpoint.join("по-запросу.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(
            fs::read(mountpoint.join("по-запросу.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read(mountpoint.join("без-сети.txt"))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENETDOWN)
        );
        assert_eq!(fs::read(mountpoint.join("текст.txt")).unwrap(), b"hello");
        assert!(fs::write(mountpoint.join("new.txt"), b"no").is_err());
        session.umount_and_join().unwrap();
    }
}
