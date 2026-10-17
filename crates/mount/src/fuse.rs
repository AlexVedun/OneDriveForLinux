//! FUSE adapter around a validated catalog snapshot, versioned cache and local drafts.

use crate::draft::{DraftFile, DraftIdentity, DraftStore};
use crate::{
    DownloadRequest, FileHandle as CachedHandle, MAX_READ_BYTES, Node, NodeKind, OpenError,
    VirtualTree,
};
use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
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
    handle: OpenHandle,
    writable: bool,
}

enum OpenHandle {
    Cached(CachedHandle),
    Draft(DraftFile),
    Pending(DraftIdentity),
}

pub struct VirtualFuse {
    tree: VirtualTree,
    cache_root: PathBuf,
    draft_root: Option<PathBuf>,
    cache_limit: u64,
    loader: Arc<ContentLoader>,
    open_lock: Mutex<()>,
    files: Mutex<HashMap<u64, OpenFile>>,
    next_handle: AtomicU64,
    uid: u32,
    gid: u32,
}

impl VirtualFuse {
    pub fn new(
        tree: VirtualTree,
        cache_root: PathBuf,
        cache_limit: u64,
        loader: Arc<ContentLoader>,
    ) -> Self {
        Self {
            tree,
            cache_root,
            draft_root: None,
            cache_limit,
            loader,
            open_lock: Mutex::new(()),
            files: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        }
    }

    pub fn new_writable(
        tree: VirtualTree,
        cache_root: PathBuf,
        draft_root: PathBuf,
        cache_limit: u64,
        loader: Arc<ContentLoader>,
    ) -> Self {
        let mut fuse = Self::new(tree, cache_root, cache_limit, loader);
        fuse.draft_root = Some(draft_root);
        fuse
    }

    pub fn mount(self, mountpoint: &Path) -> io::Result<BackgroundSession> {
        let mut options = Config::default();
        options.mount_options = vec![
            MountOption::DefaultPermissions,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::FSName("onedrive-linux".into()),
        ];
        if self.draft_root.is_none() {
            options.mount_options.push(MountOption::RO);
        }
        // A cold file open can wait for Graph. Keep directory requests responsive meanwhile.
        options.n_threads = Some(4);
        fuser::spawn_mount(self, mountpoint, &options)
    }

    fn attr(&self, node: &Node) -> Result<FileAttr, Errno> {
        let directory = node.kind == NodeKind::Directory;
        let draft_size = if directory {
            None
        } else {
            self.draft_for_node(node)?
                .map(|draft| draft.len().map_err(|_| Errno::EIO))
                .transpose()?
        };
        let size = draft_size.unwrap_or(node.size);
        Ok(FileAttr {
            ino: INodeNo(node.inode),
            size,
            blocks: size.saturating_add(511) / 512,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: if directory {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if directory {
                0o500
            } else if self.draft_root.is_some() {
                0o600
            } else {
                0o400
            },
            nlink: if directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }

    fn draft_for_node(&self, node: &Node) -> Result<Option<DraftFile>, Errno> {
        let Some(root) = &self.draft_root else {
            return Ok(None);
        };
        DraftStore::open(root)
            .and_then(|store| store.recover_item(self.tree.drive_id(), &node.item_id))
            .map_err(|_| Errno::EIO)
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
        let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
        if (writable && self.draft_root.is_none()) || (!writable && flags.0 & libc::O_TRUNC != 0) {
            return Err(Errno::EROFS);
        }
        let _serialized = self.open_lock.lock().unwrap();
        let node = self.tree.getattr(inode).ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::File {
            return Err(Errno::EISDIR);
        }
        if let Some(draft) = self.draft_for_node(node)? {
            if writable && flags.0 & libc::O_TRUNC != 0 {
                draft.set_len(0).map_err(map_draft_error)?;
            }
            return Ok(self.insert_open(inode, OpenHandle::Draft(draft), writable));
        }
        if writable && flags.0 & libc::O_TRUNC != 0 {
            let request = self
                .tree
                .request_for_item(&node.item_id)
                .map_err(map_open_error)?;
            let store =
                DraftStore::open(self.draft_root.as_ref().unwrap()).map_err(map_draft_error)?;
            let draft = store
                .begin_empty(DraftIdentity {
                    drive_id: request.drive_id,
                    item_id: request.item_id,
                    base_etag: request.etag,
                })
                .map_err(map_draft_error)?;
            return Ok(self.insert_open(inode, OpenHandle::Draft(draft), true));
        }
        if writable {
            let request = self
                .tree
                .request_for_item(&node.item_id)
                .map_err(map_open_error)?;
            return Ok(self.insert_open(
                inode,
                OpenHandle::Pending(DraftIdentity {
                    drive_id: request.drive_id,
                    item_id: request.item_id,
                    base_etag: request.etag,
                }),
                true,
            ));
        }
        let opened = self.open_cached(inode)?;
        Ok(self.insert_open(inode, OpenHandle::Cached(opened), false))
    }

    fn open_cached(&self, inode: u64) -> Result<CachedHandle, Errno> {
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
        Ok(opened)
    }

    fn insert_open(&self, inode: u64, handle: OpenHandle, writable: bool) -> u64 {
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.files.lock().unwrap().insert(
            id,
            OpenFile {
                inode,
                handle,
                writable,
            },
        );
        id
    }

    fn read_file(&self, inode: u64, handle: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let files = self.files.lock().unwrap();
        let open = files
            .get(&handle)
            .filter(|open| open.inode == inode)
            .ok_or(Errno::EBADF)?;
        if self.draft_root.is_some() {
            let node = self.tree.getattr(inode).ok_or(Errno::ENOENT)?;
            if let Some(draft) = self.draft_for_node(node)? {
                return read_draft(&draft, offset, size);
            }
        }
        match &open.handle {
            OpenHandle::Cached(handle) => handle.read(offset, size).map_err(|_| Errno::EIO),
            OpenHandle::Draft(handle) => read_draft(handle, offset, size),
            OpenHandle::Pending(_) => self
                .open_cached(inode)?
                .read(offset, size)
                .map_err(|_| Errno::EIO),
        }
    }

    fn write_file(&self, inode: u64, handle: u64, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        let mut files = self.files.lock().unwrap();
        let open = files
            .get_mut(&handle)
            .filter(|open| open.inode == inode)
            .ok_or(Errno::EBADF)?;
        if !open.writable {
            return Err(Errno::EBADF);
        }
        if let OpenHandle::Pending(identity) = &open.handle {
            let store =
                DraftStore::open(self.draft_root.as_ref().unwrap()).map_err(map_draft_error)?;
            let draft = if let Some(existing) = store.recover(identity).map_err(|_| Errno::EIO)? {
                existing
            } else {
                let cached = self.open_cached(inode)?;
                store
                    .begin_from_file(identity.clone(), cached.source_file())
                    .map_err(map_draft_error)?
            };
            open.handle = OpenHandle::Draft(draft);
        }
        let OpenHandle::Draft(draft) = &open.handle else {
            return Err(Errno::EROFS);
        };
        let written = draft.write_at(offset, data).map_err(map_draft_error)?;
        u32::try_from(written).map_err(|_| Errno::EIO)
    }

    fn truncate_file(&self, inode: u64, handle: Option<u64>, size: u64) -> Result<(), Errno> {
        if size > i64::MAX as u64 {
            return Err(Errno::EINVAL);
        }
        let owned_handle = if let Some(handle) = handle {
            handle
        } else {
            self.open_file(
                inode,
                OpenFlags(libc::O_WRONLY | if size == 0 { libc::O_TRUNC } else { 0 }),
            )?
        };
        let result = (|| {
            let mut files = self.files.lock().unwrap();
            let open = files
                .get_mut(&owned_handle)
                .filter(|open| open.inode == inode && open.writable)
                .ok_or(Errno::EBADF)?;
            if let OpenHandle::Pending(identity) = &open.handle {
                let store =
                    DraftStore::open(self.draft_root.as_ref().unwrap()).map_err(map_draft_error)?;
                let draft = if size == 0 {
                    store.begin_empty(identity.clone())
                } else {
                    let cached = self.open_cached(inode)?;
                    store.begin_from_file(identity.clone(), cached.source_file())
                }
                .map_err(map_draft_error)?;
                open.handle = OpenHandle::Draft(draft);
            }
            let OpenHandle::Draft(draft) = &open.handle else {
                return Err(Errno::EROFS);
            };
            draft.set_len(size).map_err(map_draft_error)
        })();
        if handle.is_none() {
            self.files.lock().unwrap().remove(&owned_handle);
        }
        result
    }
}

fn read_draft(draft: &DraftFile, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
    if size > MAX_READ_BYTES {
        return Err(Errno::EINVAL);
    }
    let mut bytes = vec![0; size as usize];
    let count = draft.read_at(offset, &mut bytes).map_err(|_| Errno::EIO)?;
    bytes.truncate(count);
    Ok(bytes)
}

fn map_open_error(error: OpenError) -> Errno {
    match error {
        OpenError::MissingFile => Errno::ENOENT,
        OpenError::IsDirectory => Errno::EISDIR,
        _ => Errno::EIO,
    }
}

fn map_draft_error(error: io::Error) -> Errno {
    match error.raw_os_error() {
        Some(libc::ENOSPC) => Errno::ENOSPC,
        Some(libc::EDQUOT) => Errno::EDQUOT,
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

impl Filesystem for VirtualFuse {
    fn lookup(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.tree.lookup(parent.0, name) {
            Some(node) => match self.attr(node) {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(error) => reply.error(error),
            },
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
            Some(node) => match self.attr(node) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(error) => reply.error(error),
            },
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        handle: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if mode.is_some() || uid.is_some() || gid.is_some() || flags.is_some() {
            reply.error(Errno::EROFS);
            return;
        }
        let Some(size) = size else {
            reply.error(Errno::EROFS);
            return;
        };
        match self.truncate_file(inode.0, handle.map(|handle| handle.0), size) {
            Ok(()) => match self
                .tree
                .getattr(inode.0)
                .and_then(|node| self.attr(node).ok())
            {
                Some(attr) => reply.attr(&TTL, &attr),
                None => reply.error(Errno::EIO),
            },
            Err(error) => reply.error(error),
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
            Ok(handle) => reply.opened(
                FileHandle(handle),
                if self.draft_root.is_some() {
                    FopenFlags::FOPEN_DIRECT_IO
                } else {
                    FopenFlags::empty()
                },
            ),
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

    fn write(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.write_file(inode.0, handle.0, offset, data) {
            Ok(count) => reply.written(count),
            Err(error) => reply.error(error),
        }
    }

    fn flush(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        let files = self.files.lock().unwrap();
        match files.get(&handle.0).filter(|open| open.inode == inode.0) {
            Some(OpenFile {
                handle: OpenHandle::Draft(draft),
                ..
            }) => match draft.sync() {
                Ok(()) => reply.ok(),
                Err(_) => reply.error(Errno::EIO),
            },
            Some(_) => reply.ok(),
            None => reply.error(Errno::EBADF),
        }
    }

    fn fsync(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let files = self.files.lock().unwrap();
        match files.get(&handle.0).filter(|open| open.inode == inode.0) {
            Some(OpenFile {
                handle: OpenHandle::Draft(draft),
                ..
            }) => match draft.sync() {
                Ok(()) => reply.ok(),
                Err(_) => reply.error(Errno::EIO),
            },
            Some(_) => reply.ok(),
            None => reply.error(Errno::EBADF),
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
    use std::sync::{Condvar, mpsc};

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
        let fuse = VirtualFuse::new(tree, cache_root, 10, loader);
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
        assert_eq!(
            map_draft_error(io::Error::from_raw_os_error(libc::ENOSPC)),
            Errno::ENOSPC
        );
        assert_eq!(
            map_draft_error(io::Error::from_raw_os_error(libc::EDQUOT)),
            Errno::EDQUOT
        );
    }

    #[test]
    fn writable_open_keeps_durable_draft_separate_from_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache_root = dir.path().join("cache");
        let draft_root = dir.path().join("drafts");
        let source = dir.path().join("source");
        fs::write(&source, b"hello").unwrap();
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let request = tree.request_for_item("file").unwrap();
        CacheStore::open(&cache_root, 10)
            .unwrap()
            .put(&request.cache_key(), &source, 5)
            .unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            cache_root,
            draft_root.clone(),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let inode = fuse.tree.lookup(ROOT_INODE, "текст.txt").unwrap().inode;
        let reader = fuse.open_file(inode, OpenFlags(libc::O_RDONLY)).unwrap();
        let handle = fuse.open_file(inode, OpenFlags(libc::O_RDWR)).unwrap();
        assert_eq!(fuse.write_file(inode, handle, 1, b"a").unwrap(), 1);
        assert_eq!(fuse.read_file(inode, handle, 0, 5).unwrap(), b"hallo");
        assert_eq!(fuse.read_file(inode, reader, 0, 5).unwrap(), b"hallo");
        assert_eq!(
            fuse.attr(fuse.tree.getattr(inode).unwrap()).unwrap().size,
            5
        );
        drop(fuse);
        assert_eq!(fs::read(source).unwrap(), b"hello");
        let store = DraftStore::open(&draft_root).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        let draft = store.recover_item("drive", "file").unwrap().unwrap();
        let mut bytes = [0; 5];
        draft.read_at(0, &mut bytes).unwrap();
        assert_eq!(&bytes, b"hallo");
    }

    #[test]
    fn writable_mount_persists_truncating_replacement() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let cache_root = dir.path().join("cache");
        let draft_root = dir.path().join("drafts");
        let source = dir.path().join("source");
        fs::create_dir(&mountpoint).unwrap();
        fs::write(&source, b"hello").unwrap();
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let session = VirtualFuse::new_writable(
            tree,
            cache_root,
            draft_root.clone(),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        )
        .mount(&mountpoint)
        .unwrap();
        fs::write(mountpoint.join("текст.txt"), b"saved locally").unwrap();
        assert_eq!(
            fs::read(mountpoint.join("текст.txt")).unwrap(),
            b"saved locally"
        );
        session.umount_and_join().unwrap();
        let draft = DraftStore::open(&draft_root)
            .unwrap()
            .recover_item("drive", "file")
            .unwrap()
            .unwrap();
        assert_eq!(draft.len().unwrap(), 13);
        assert_eq!(fs::read(source).unwrap(), b"hello");
    }

    #[test]
    fn truncation_can_start_offline_without_cached_content() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let inode = fuse.tree.lookup(ROOT_INODE, "текст.txt").unwrap().inode;
        let handle = fuse
            .open_file(inode, OpenFlags(libc::O_WRONLY | libc::O_TRUNC))
            .unwrap();
        assert_eq!(fuse.write_file(inode, handle, 0, b"local").unwrap(), 5);
        assert_eq!(fuse.read_file(inode, handle, 0, 5).unwrap(), b"local");
        assert_eq!(
            DraftStore::open(&draft_root).unwrap().list().unwrap().len(),
            1
        );
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
        let session = VirtualFuse::new(tree, cache_root, 10, loader)
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

    #[test]
    fn directory_listing_stays_responsive_during_file_download() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        fs::create_dir(&mountpoint).unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let gate_for_loader = Arc::clone(&gate);
        let loader: Arc<ContentLoader> = Arc::new(move |_| {
            started_tx.send(()).unwrap();
            let (lock, wake) = &*gate_for_loader;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            Err(io::ErrorKind::NotConnected.into())
        });
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let session = VirtualFuse::new(tree, dir.path().join("cache"), 10, loader)
            .mount(&mountpoint)
            .unwrap();
        let file = mountpoint.join("текст.txt");
        let reader = std::thread::spawn(move || fs::read(file));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let (listing_tx, listing_rx) = mpsc::channel();
        let listing = std::thread::spawn(move || {
            listing_tx
                .send(fs::read_dir(mountpoint).unwrap().count())
                .unwrap();
        });
        let listing_result = listing_rx.recv_timeout(Duration::from_secs(2));
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        assert_eq!(
            reader.join().unwrap().unwrap_err().raw_os_error(),
            Some(libc::ENETDOWN)
        );
        listing.join().unwrap();
        assert_eq!(listing_result.unwrap(), 1);
        session.umount_and_join().unwrap();
    }
}
