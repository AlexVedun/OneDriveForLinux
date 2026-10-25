//! FUSE adapter around a validated catalog snapshot, versioned cache and local drafts.

use crate::directory::DirectoryStore;
use crate::draft::{DraftFile, DraftIdentity, DraftStore, NewFileLocation};
use crate::mutation::{Mutation, MutationKind, MutationStatus, MutationStore};
use crate::{
    DownloadRequest, FileHandle as CachedHandle, MAX_READ_BYTES, Node, NodeKind, OpenError,
    VirtualTree,
};
use fuser::{
    BackgroundSession, Config, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType,
    Filesystem, FopenFlags, Generation, INodeNo, MountOption, OpenAccMode, OpenFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyLseek,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request,
};
use onedrive_cache::CacheStore;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);

struct LocalSpace {
    blocks: u64,
    free_blocks: u64,
    available_blocks: u64,
    files: u64,
    free_files: u64,
    block_size: u32,
    fragment_size: u32,
    name_length: u32,
}

fn local_space(path: &Path) -> io::Result<LocalSpace> {
    let directory = File::open(path)?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::fstatvfs(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    let block_size = u32::try_from(stat.f_bsize).unwrap_or(u32::MAX).max(1);
    Ok(LocalSpace {
        blocks: stat.f_blocks,
        free_blocks: stat.f_bfree,
        available_blocks: stat.f_bavail,
        files: stat.f_files,
        free_files: stat.f_ffree,
        block_size,
        fragment_size: u32::try_from(stat.f_frsize).unwrap_or(block_size).max(1),
        name_length: u32::try_from(stat.f_namemax.min(255)).unwrap_or(255),
    })
}

pub type ContentLoader = dyn Fn(&DownloadRequest) -> io::Result<()> + Send + Sync;

#[derive(Clone)]
pub struct PublishedVersionController {
    drive_id: String,
    versions: Arc<Mutex<HashMap<u64, (String, u64)>>>,
    operations: Arc<Mutex<PublicationOperations>>,
    open_lock: Arc<Mutex<()>>,
    files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    local_files: Arc<Mutex<HashMap<u64, Node>>>,
    directory_aliases: Arc<Mutex<HashMap<String, u64>>>,
}

#[derive(Default)]
struct PublicationOperations {
    active: HashSet<u64>,
    opening: HashMap<u64, usize>,
}

pub struct PublicationLease {
    inode: u64,
    additional_inodes: Vec<u64>,
    operations: Arc<Mutex<PublicationOperations>>,
}

impl Drop for PublicationLease {
    fn drop(&mut self) {
        let mut operations = self.operations.lock().unwrap();
        operations.active.remove(&self.inode);
        for inode in &self.additional_inodes {
            operations.active.remove(inode);
        }
    }
}

struct OpeningFile {
    inode: u64,
    operations: Arc<Mutex<PublicationOperations>>,
}

impl Drop for OpeningFile {
    fn drop(&mut self) {
        let mut operations = self.operations.lock().unwrap();
        let count = operations.opening.get_mut(&self.inode).unwrap();
        *count -= 1;
        if *count == 0 {
            operations.opening.remove(&self.inode);
        }
    }
}

impl PublishedVersionController {
    /// Reserve an idle existing file until its uploaded draft is acknowledged.
    pub fn try_begin_existing(
        &self,
        drive_id: &str,
        item_id: &str,
    ) -> io::Result<PublicationLease> {
        self.try_begin(drive_id, item_id, false)
    }

    /// Reserve a newly created file while its temporary ID is published.
    pub fn try_begin_new(&self, drive_id: &str, item_id: &str) -> io::Result<PublicationLease> {
        self.try_begin(drive_id, item_id, true)
    }

    /// Reserve a local directory while Graph creates its remote counterpart.
    pub fn try_begin_directory(
        &self,
        drive_id: &str,
        item_id: &str,
    ) -> io::Result<PublicationLease> {
        if drive_id != self.drive_id || !item_id.starts_with("local-dir:") {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let inode = crate::stable_inode(drive_id, item_id);
        let _serialized = self.open_lock.lock().unwrap();
        if self
            .local_files
            .lock()
            .unwrap()
            .get(&inode)
            .is_none_or(|node| node.item_id != item_id || node.kind != NodeKind::Directory)
            || !self.operations.lock().unwrap().active.insert(inode)
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(PublicationLease {
            inode,
            additional_inodes: Vec::new(),
            operations: Arc::clone(&self.operations),
        })
    }

    fn try_begin(
        &self,
        drive_id: &str,
        item_id: &str,
        new_file: bool,
    ) -> io::Result<PublicationLease> {
        if drive_id != self.drive_id
            || item_id.is_empty()
            || item_id.starts_with("local:") != new_file
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let inode = crate::stable_inode(drive_id, item_id);
        let _serialized = self.open_lock.lock().unwrap();
        let local = self.local_files.lock().unwrap();
        if new_file
            && local
                .get(&inode)
                .is_none_or(|node| node.item_id != item_id || node.kind != NodeKind::File)
        {
            return Err(io::ErrorKind::NotFound.into());
        }
        let additional_inodes: Vec<_> = if new_file {
            Vec::new()
        } else {
            local
                .values()
                .filter(|node| node.item_id == item_id && node.inode != inode)
                .map(|node| node.inode)
                .collect()
        };
        drop(local);
        let files = self.files.lock().unwrap();
        let mut operations = self.operations.lock().unwrap();
        if files.values().any(|open| {
            open.inode == inode
                || match &open.handle {
                    OpenHandle::Draft(draft) => {
                        draft.identity().drive_id == drive_id && draft.identity().item_id == item_id
                    }
                    OpenHandle::Pending(identity) => {
                        identity.drive_id == drive_id && identity.item_id == item_id
                    }
                    OpenHandle::Cached(_) => false,
                }
        }) || std::iter::once(&inode)
            .chain(&additional_inodes)
            .any(|inode| {
                operations.opening.contains_key(inode) || operations.active.contains(inode)
            })
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        operations.active.insert(inode);
        operations.active.extend(additional_inodes.iter().copied());
        Ok(PublicationLease {
            inode,
            additional_inodes,
            operations: Arc::clone(&self.operations),
        })
    }

    /// Make a verified upload visible to an already mounted filesystem.
    pub fn publish_existing(&self, item_id: &str, etag: String, size: u64) {
        let inode = crate::stable_inode(&self.drive_id, item_id);
        let _serialized = self.open_lock.lock().unwrap();
        let local = self.local_files.lock().unwrap();
        let mut versions = self.versions.lock().unwrap();
        versions.insert(inode, (etag.clone(), size));
        for node in local.values().filter(|node| node.item_id == item_id) {
            versions.insert(node.inode, (etag.clone(), size));
        }
    }

    /// Check publication while the local draft still exists, before acknowledging it.
    pub fn validate_new(
        &self,
        lease: &PublicationLease,
        local_id: &str,
        remote_id: &str,
        etag: &str,
    ) -> io::Result<()> {
        let inode = crate::stable_inode(&self.drive_id, local_id);
        if !local_id.starts_with("local:")
            || remote_id.is_empty()
            || remote_id.starts_with("local:")
            || remote_id.starts_with("local-dir:")
            || etag.is_empty()
            || lease.inode != inode
            || !Arc::ptr_eq(&lease.operations, &self.operations)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let _serialized = self.open_lock.lock().unwrap();
        let local = self.local_files.lock().unwrap();
        if local
            .values()
            .any(|node| node.inode != inode && node.item_id == remote_id)
            || local
                .get(&inode)
                .is_none_or(|node| node.item_id != local_id || node.kind != NodeKind::File)
            || !self.operations.lock().unwrap().active.contains(&inode)
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }

    /// Keep the mounted inode and path while replacing its temporary Graph ID.
    pub fn publish_new(
        &self,
        lease: &PublicationLease,
        local_id: &str,
        remote_id: &str,
        etag: String,
        size: u64,
    ) -> io::Result<()> {
        self.validate_new(lease, local_id, remote_id, &etag)?;
        let inode = crate::stable_inode(&self.drive_id, local_id);
        let _serialized = self.open_lock.lock().unwrap();
        let mut local = self.local_files.lock().unwrap();
        if local
            .values()
            .any(|node| node.inode != inode && node.item_id == remote_id)
            || local
                .get(&inode)
                .is_none_or(|node| node.item_id != local_id || node.kind != NodeKind::File)
            || !self.operations.lock().unwrap().active.contains(&inode)
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let node = local.get_mut(&inode).unwrap();
        node.item_id = remote_id.to_owned();
        node.etag = Some(etag);
        node.size = size;
        Ok(())
    }

    /// Check the mounted directory before storing the verified Graph mapping.
    pub fn validate_directory(
        &self,
        lease: &PublicationLease,
        local_id: &str,
        remote_id: &str,
        etag: &str,
    ) -> io::Result<()> {
        let inode = crate::stable_inode(&self.drive_id, local_id);
        if !local_id.starts_with("local-dir:")
            || remote_id.is_empty()
            || remote_id.starts_with("local:")
            || remote_id.starts_with("local-dir:")
            || etag.is_empty()
            || lease.inode != inode
            || !Arc::ptr_eq(&lease.operations, &self.operations)
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let _serialized = self.open_lock.lock().unwrap();
        let local = self.local_files.lock().unwrap();
        if local
            .values()
            .any(|node| node.inode != inode && node.item_id == remote_id)
            || local
                .get(&inode)
                .is_none_or(|node| node.item_id != local_id || node.kind != NodeKind::Directory)
            || !self.operations.lock().unwrap().active.contains(&inode)
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }

    /// Give a mounted local directory its Graph identity without moving its inode.
    pub fn publish_directory(
        &self,
        lease: &PublicationLease,
        local_id: &str,
        remote_id: &str,
        etag: &str,
    ) -> io::Result<()> {
        self.validate_directory(lease, local_id, remote_id, etag)?;
        let inode = crate::stable_inode(&self.drive_id, local_id);
        let _serialized = self.open_lock.lock().unwrap();
        let mut local = self.local_files.lock().unwrap();
        if local
            .values()
            .any(|node| node.inode != inode && node.item_id == remote_id)
            || local
                .get(&inode)
                .is_none_or(|node| node.item_id != local_id || node.kind != NodeKind::Directory)
            || !self.operations.lock().unwrap().active.contains(&inode)
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let node = local.get_mut(&inode).unwrap();
        node.item_id = remote_id.to_owned();
        node.etag = Some(etag.to_owned());
        self.directory_aliases
            .lock()
            .unwrap()
            .insert(remote_id.to_owned(), inode);
        Ok(())
    }
}

struct OpenFile {
    inode: u64,
    handle: OpenHandle,
    readable: bool,
    writable: bool,
    append: bool,
}

type DirectoryEntry = (u64, FileType, String);

struct OpenDirectory {
    inode: u64,
    entries: Arc<Vec<DirectoryEntry>>,
}

#[derive(Clone)]
enum OpenHandle {
    Cached(Arc<CachedHandle>),
    Draft(Arc<DraftFile>),
    Pending(DraftIdentity),
}

pub struct VirtualFuse {
    tree: VirtualTree,
    cache_root: PathBuf,
    draft_root: Option<PathBuf>,
    cache_limit: u64,
    loader: Arc<ContentLoader>,
    open_lock: Arc<Mutex<()>>,
    download_lock: Mutex<()>,
    files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    directories: Mutex<HashMap<u64, OpenDirectory>>,
    local_files: Arc<Mutex<HashMap<u64, Node>>>,
    detached_files: Mutex<HashMap<u64, Node>>,
    directory_aliases: Arc<Mutex<HashMap<String, u64>>>,
    remote_changes: Mutex<HashMap<u64, Mutation>>,
    published_versions: Arc<Mutex<HashMap<u64, (String, u64)>>>,
    publication_operations: Arc<Mutex<PublicationOperations>>,
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
            open_lock: Arc::new(Mutex::new(())),
            download_lock: Mutex::new(()),
            files: Arc::new(Mutex::new(HashMap::new())),
            directories: Mutex::new(HashMap::new()),
            local_files: Arc::new(Mutex::new(HashMap::new())),
            detached_files: Mutex::new(HashMap::new()),
            directory_aliases: Arc::new(Mutex::new(HashMap::new())),
            remote_changes: Mutex::new(HashMap::new()),
            published_versions: Arc::new(Mutex::new(HashMap::new())),
            publication_operations: Arc::new(Mutex::new(PublicationOperations::default())),
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
        self.load_local_files()?;
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

    pub fn published_version_controller(&self) -> PublishedVersionController {
        PublishedVersionController {
            drive_id: self.tree.drive_id().to_owned(),
            versions: Arc::clone(&self.published_versions),
            operations: Arc::clone(&self.publication_operations),
            open_lock: Arc::clone(&self.open_lock),
            files: Arc::clone(&self.files),
            local_files: Arc::clone(&self.local_files),
            directory_aliases: Arc::clone(&self.directory_aliases),
        }
    }

    fn is_publishing(&self, inode: u64) -> bool {
        self.publication_operations
            .lock()
            .unwrap()
            .active
            .contains(&inode)
    }

    fn mark_opening(&self, inode: u64) -> OpeningFile {
        *self
            .publication_operations
            .lock()
            .unwrap()
            .opening
            .entry(inode)
            .or_default() += 1;
        OpeningFile {
            inode,
            operations: Arc::clone(&self.publication_operations),
        }
    }

    fn attr(&self, node: &Node) -> Result<FileAttr, Errno> {
        let store = if node.kind == NodeKind::Directory {
            None
        } else {
            self.draft_root
                .as_ref()
                .map(|root| DraftStore::open(root).map_err(|_| Errno::EIO))
                .transpose()?
        };
        self.attr_with_store(node, store.as_ref())
    }

    fn attr_with_store(&self, node: &Node, store: Option<&DraftStore>) -> Result<FileAttr, Errno> {
        let directory = node.kind == NodeKind::Directory;
        let detached = self
            .detached_files
            .lock()
            .unwrap()
            .contains_key(&node.inode);
        let draft_attributes = if detached {
            let files = self.files.lock().unwrap();
            let draft = files.values().find_map(|open| {
                if open.inode == node.inode
                    && let OpenHandle::Draft(draft) = &open.handle
                {
                    return Some(draft);
                }
                None
            });
            Some(
                draft
                    .ok_or(Errno::ENOENT)?
                    .attributes()
                    .map_err(|_| Errno::EIO)?,
            )
        } else if directory {
            None
        } else {
            store
                .map(|store| {
                    store
                        .recover_item(self.tree.drive_id(), &node.item_id)
                        .map_err(|_| Errno::EIO)
                })
                .transpose()?
                .flatten()
                .map(|draft| draft.attributes().map_err(|_| Errno::EIO))
                .transpose()?
        };
        let size = draft_attributes.map_or(node.size, |attributes| attributes.0);
        Ok(FileAttr {
            ino: INodeNo(node.inode),
            size,
            blocks: size.saturating_add(511) / 512,
            atime: draft_attributes.map_or(UNIX_EPOCH, |attributes| attributes.1),
            mtime: draft_attributes.map_or(UNIX_EPOCH, |attributes| attributes.2),
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: if directory {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if directory {
                if self.draft_root.is_some() {
                    0o700
                } else {
                    0o500
                }
            } else if self.draft_root.is_some() {
                0o600
            } else {
                0o400
            },
            nlink: if directory {
                2
            } else if detached {
                0
            } else {
                1
            },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }

    fn directory_entry_attr(
        &self,
        inode: u64,
        store: Option<&DraftStore>,
    ) -> Result<FileAttr, Errno> {
        let node = self.node(inode).ok_or(Errno::ENOENT)?;
        self.attr_with_store(&node, store)
    }

    fn check_xattr_node(&self, inode: u64) -> Result<(), Errno> {
        self.node(inode).map(|_| ()).ok_or(Errno::ENOENT)
    }

    fn reject_xattr_write(&self, inode: u64) -> Errno {
        if self.check_xattr_node(inode).is_err() {
            Errno::ENOENT
        } else if self.draft_root.is_some() {
            Errno::EOPNOTSUPP
        } else {
            Errno::EROFS
        }
    }

    fn draft_for_node(&self, node: &Node) -> Result<Option<DraftFile>, Errno> {
        let Some(root) = &self.draft_root else {
            return Ok(None);
        };
        DraftStore::open(root)
            .and_then(|store| store.recover_item(self.tree.drive_id(), &node.item_id))
            .map_err(|_| Errno::EIO)
    }

    fn node(&self, inode: u64) -> Option<Node> {
        let local = self
            .local_files
            .lock()
            .unwrap()
            .get(&inode)
            .cloned()
            .or_else(|| self.detached_files.lock().unwrap().get(&inode).cloned());
        let mut node = if let Some(local) = local {
            self.remote_node(&local)?
        } else {
            self.tree
                .getattr(inode)
                .and_then(|node| self.remote_node(node))?
        };
        if let Some((etag, size)) = self.published_versions.lock().unwrap().get(&inode) {
            node.etag = Some(etag.clone());
            node.size = *size;
        }
        Some(node)
    }

    fn request_for_node(&self, node: &Node) -> Result<DownloadRequest, OpenError> {
        let etag = node
            .etag
            .clone()
            .filter(|etag| !etag.is_empty())
            .ok_or(OpenError::MissingVersion)?;
        Ok(DownloadRequest {
            drive_id: self.tree.drive_id().to_owned(),
            item_id: node.item_id.clone(),
            etag,
            size: node.size,
        })
    }

    fn remote_parent_id(&self, parent: &Node) -> Result<String, Errno> {
        if !parent.item_id.starts_with("local-dir:") {
            return Ok(parent.item_id.clone());
        }
        let root = self.draft_root.as_ref().ok_or(Errno::EROFS)?;
        DirectoryStore::open(root)
            .and_then(|store| store.remote_parent_id(&parent.item_id))
            .map_err(map_draft_error)?
            .ok_or(Errno::EBUSY)
    }

    fn remote_node(&self, node: &Node) -> Option<Node> {
        let remote_inode = crate::stable_inode(self.tree.drive_id(), &node.item_id);
        let mutation = self
            .remote_changes
            .lock()
            .unwrap()
            .get(&remote_inode)
            .cloned();
        match mutation.map(|mutation| mutation.kind) {
            Some(MutationKind::Delete) => None,
            Some(MutationKind::Rename {
                target_parent_id,
                target_name,
            }) => {
                let mut moved = node.clone();
                moved.parent_inode =
                    if target_parent_id == self.tree.getattr(crate::ROOT_INODE)?.item_id {
                        crate::ROOT_INODE
                    } else {
                        self.directory_aliases
                            .lock()
                            .unwrap()
                            .get(&target_parent_id)
                            .copied()
                            .unwrap_or_else(|| {
                                crate::stable_inode(self.tree.drive_id(), &target_parent_id)
                            })
                    };
                moved.name = target_name;
                Some(moved)
            }
            None => Some(node.clone()),
        }
    }

    fn target_parent_matches(&self, parent: u64, parent_id: &str, target_parent_id: &str) -> bool {
        parent_id == target_parent_id
            || self
                .directory_aliases
                .lock()
                .unwrap()
                .get(target_parent_id)
                .is_some_and(|inode| *inode == parent)
    }

    fn name_taken(
        &self,
        parent: u64,
        parent_id: &str,
        name: &str,
        except_inode: Option<u64>,
        local: &HashMap<u64, Node>,
    ) -> bool {
        let folded = name.to_lowercase();
        self.tree.readdir(parent).is_some_and(|children| {
            children.into_iter().any(|child| {
                self.remote_node(child).is_some_and(|visible| {
                    visible.parent_inode == parent
                        && Some(visible.inode) != except_inode
                        && visible.name.to_lowercase() == folded
                })
            })
        }) || local.values().any(|child| {
            self.remote_node(child).is_some_and(|visible| {
                visible.parent_inode == parent
                    && Some(visible.inode) != except_inode
                    && visible.name.to_lowercase() == folded
            })
        }) || self
            .remote_changes
            .lock()
            .unwrap()
            .values()
            .any(|mutation| {
                matches!(&mutation.kind, MutationKind::Rename { target_parent_id, target_name }
                    if self.target_parent_matches(parent, parent_id, target_parent_id)
                        && Some(crate::stable_inode(&mutation.drive_id, &mutation.item_id)) != except_inode
                        && target_name.to_lowercase() == folded)
            })
    }

    fn recovery_name(
        &self,
        parent: &Node,
        inode: u64,
        original: &str,
        orphaned: bool,
        local: &HashMap<u64, Node>,
    ) -> String {
        let kind = if orphaned { "orphan" } else { "conflict" };
        for attempt in 1_u64.. {
            let mut candidate = if attempt == 1 {
                format!(".{kind}-{inode}-")
            } else {
                format!(".{kind}-{inode}-{attempt}-")
            };
            for character in original.chars() {
                if candidate.len() + character.len_utf8() > 255 {
                    break;
                }
                candidate.push(character);
            }
            if !self.name_taken(parent.inode, &parent.item_id, &candidate, None, local) {
                return candidate;
            }
        }
        unreachable!("recovery name space exhausted")
    }

    fn lookup_node(&self, parent: u64, name: &str) -> Option<Node> {
        if name == "." {
            return self.node(parent);
        }
        if name == ".." {
            return self.node(self.node(parent)?.parent_inode);
        }
        let local_inodes: Vec<_> = self
            .local_files
            .lock()
            .unwrap()
            .values()
            .map(|node| node.inode)
            .collect();
        for inode in local_inodes {
            if let Some(node) = self.node(inode)
                && node.parent_inode == parent
                && node.name == name
            {
                return Some(node);
            }
        }
        let changes: Vec<_> = self
            .remote_changes
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for mutation in changes {
            if let MutationKind::Rename {
                target_parent_id,
                target_name,
            } = &mutation.kind
                && target_name == name
                && self.node(parent).is_some_and(|node| {
                    self.target_parent_matches(parent, &node.item_id, target_parent_id)
                })
            {
                return self
                    .tree
                    .getattr(crate::stable_inode(&mutation.drive_id, &mutation.item_id))
                    .and_then(|node| self.node(node.inode));
            }
        }
        self.tree
            .lookup(parent, name)
            .and_then(|node| self.node(node.inode))
            .filter(|node| node.parent_inode == parent && node.name == name)
    }

    fn visible_child(&self, parent: u64, name: &str, local: &HashMap<u64, Node>) -> Option<Node> {
        let visible = local
            .values()
            .filter_map(|node| self.remote_node(node))
            .find(|node| node.parent_inode == parent && node.name == name)
            .or_else(|| {
                let parent_id = &local
                    .get(&parent)
                    .or_else(|| self.tree.getattr(parent))?
                    .item_id;
                let mutation = self.remote_changes
                    .lock()
                    .unwrap()
                    .values()
                    .find(|mutation| {
                        matches!(&mutation.kind, MutationKind::Rename { target_parent_id, target_name }
                            if self.target_parent_matches(parent, parent_id, target_parent_id)
                                && target_name == name)
                    })
                    .cloned()?;
                self.tree
                    .getattr(crate::stable_inode(&mutation.drive_id, &mutation.item_id))
                    .and_then(|node| self.remote_node(node))
            })
            .or_else(|| {
                self.tree
                    .lookup(parent, name)
                    .and_then(|node| self.remote_node(node))
                    .filter(|node| node.parent_inode == parent && node.name == name)
            });
        visible.map(|mut node| {
            if let Some((etag, size)) = self.published_versions.lock().unwrap().get(&node.inode) {
                node.etag = Some(etag.clone());
                node.size = *size;
            }
            node
        })
    }

    fn load_local_files(&self) -> io::Result<()> {
        let Some(root) = &self.draft_root else {
            return Ok(());
        };
        let store = DraftStore::open(root)?;
        let mutation_store = MutationStore::open(root)?;
        let mut changes = self.remote_changes.lock().unwrap();
        for mutation in mutation_store.list()? {
            if mutation.drive_id != self.tree.drive_id() {
                continue;
            }
            let inode = crate::stable_inode(&mutation.drive_id, &mutation.item_id);
            if mutation.status == MutationStatus::Applied {
                let confirmed = match &mutation.kind {
                    MutationKind::Delete => self.tree.getattr(inode).is_none(),
                    MutationKind::Rename {
                        target_parent_id,
                        target_name,
                    } => self.tree.getattr(inode).is_some_and(|node| {
                        node.name == *target_name
                            && self
                                .tree
                                .getattr(node.parent_inode)
                                .is_some_and(|parent| parent.item_id == *target_parent_id)
                    }),
                };
                if confirmed {
                    mutation_store.remove(&mutation)?;
                    continue;
                }
            }
            changes.insert(inode, mutation);
        }
        drop(changes);
        let mut local = self.local_files.lock().unwrap();
        let directory_store = DirectoryStore::open(root)?;
        let mut directories: Vec<_> = directory_store
            .list()?
            .into_iter()
            .filter(|directory| directory.drive_id == self.tree.drive_id())
            .collect();
        let mappings: HashMap<_, _> = directories
            .iter()
            .filter_map(|directory| {
                directory
                    .remote_id
                    .as_ref()
                    .map(|remote| (directory.item_id.clone(), remote.clone()))
            })
            .collect();
        for directory in &directories {
            if let Some(remote_id) = &directory.remote_id
                && !directory.seen_remote
                && self
                    .tree
                    .getattr(crate::stable_inode(&directory.drive_id, remote_id))
                    .is_some()
            {
                directory_store.mark_seen(directory)?;
            }
        }
        directories.retain(|directory| {
            !directory.seen_remote
                && directory.remote_id.as_ref().is_none_or(|remote_id| {
                    self.tree
                        .getattr(crate::stable_inode(&directory.drive_id, remote_id))
                        .is_none()
                })
        });
        while !directories.is_empty() {
            let before = directories.len();
            directories.retain(|directory| {
                let parent_id = mappings
                    .get(&directory.parent_id)
                    .filter(|remote_id| {
                        self.tree
                            .getattr(crate::stable_inode(&directory.drive_id, remote_id))
                            .is_some()
                    })
                    .unwrap_or(&directory.parent_id);
                let parent = self
                    .tree
                    .getattr(crate::stable_inode(&directory.drive_id, parent_id))
                    .or_else(|| {
                        self.tree
                            .getattr(crate::ROOT_INODE)
                            .filter(|node| node.item_id == *parent_id)
                    })
                    .cloned()
                    .or_else(|| {
                        local
                            .values()
                            .find(|node| node.item_id == *parent_id)
                            .cloned()
                    });
                let Some(parent) = parent.filter(|node| node.kind == NodeKind::Directory) else {
                    return true;
                };
                let inode = crate::stable_inode(&directory.drive_id, &directory.item_id);
                if self.tree.getattr(inode).is_none() && !local.contains_key(&inode) {
                    let name = if self.name_taken(
                        parent.inode,
                        &parent.item_id,
                        &directory.name,
                        None,
                        &local,
                    ) {
                        self.recovery_name(&parent, inode, &directory.name, false, &local)
                    } else {
                        directory.name.clone()
                    };
                    local.insert(
                        inode,
                        Node {
                            inode,
                            parent_inode: parent.inode,
                            item_id: directory.item_id.clone(),
                            name,
                            kind: NodeKind::Directory,
                            size: 0,
                            etag: None,
                            ctag: None,
                        },
                    );
                }
                false
            });
            if directories.len() == before {
                // Keep descendants available even if a remote parent vanished.
                for directory in directories.drain(..) {
                    let inode = crate::stable_inode(&directory.drive_id, &directory.item_id);
                    let root = self.tree.getattr(crate::ROOT_INODE).expect("virtual root");
                    let name = self.recovery_name(root, inode, &directory.name, true, &local);
                    local.insert(
                        inode,
                        Node {
                            inode,
                            parent_inode: crate::ROOT_INODE,
                            item_id: directory.item_id,
                            name,
                            kind: NodeKind::Directory,
                            size: 0,
                            etag: None,
                            ctag: None,
                        },
                    );
                }
            }
        }
        for identity in store.list()? {
            if identity.drive_id != self.tree.drive_id() {
                continue;
            }
            let inode = crate::stable_inode(&identity.drive_id, &identity.item_id);
            let Some(location) = &identity.new_file else {
                if self.tree.getattr(inode).is_none() && !local.contains_key(&inode) {
                    let root = self.tree.getattr(crate::ROOT_INODE).expect("virtual root");
                    let name = self.recovery_name(root, inode, "remote-file", true, &local);
                    local.insert(
                        inode,
                        Node {
                            inode,
                            parent_inode: crate::ROOT_INODE,
                            item_id: identity.item_id,
                            name,
                            kind: NodeKind::File,
                            size: 0,
                            etag: None,
                            ctag: None,
                        },
                    );
                }
                continue;
            };
            let parent_id = mappings
                .get(&location.parent_id)
                .filter(|remote_id| {
                    self.tree
                        .getattr(crate::stable_inode(&identity.drive_id, remote_id))
                        .is_some()
                })
                .unwrap_or(&location.parent_id);
            let parent = self
                .tree
                .getattr(crate::stable_inode(&identity.drive_id, parent_id))
                .or_else(|| {
                    self.tree
                        .getattr(crate::ROOT_INODE)
                        .filter(|node| node.item_id == *parent_id)
                })
                .cloned()
                .or_else(|| {
                    local
                        .values()
                        .find(|node| node.item_id == *parent_id)
                        .cloned()
                });
            let (parent, orphaned) = match parent
                .as_ref()
                .filter(|node| node.kind == NodeKind::Directory)
            {
                Some(parent) => (parent, false),
                None => (
                    self.tree.getattr(crate::ROOT_INODE).expect("virtual root"),
                    true,
                ),
            };
            let mut visible_name = location.name.clone();
            if orphaned
                || self.name_taken(parent.inode, &parent.item_id, &visible_name, None, &local)
            {
                let inode = crate::stable_inode(&identity.drive_id, &identity.item_id);
                visible_name = self.recovery_name(parent, inode, &location.name, orphaned, &local);
            }
            if self.tree.getattr(inode).is_some() || local.contains_key(&inode) {
                continue;
            }
            local.insert(
                inode,
                Node {
                    inode,
                    parent_inode: parent.inode,
                    item_id: identity.item_id,
                    name: visible_name,
                    kind: NodeKind::File,
                    size: 0,
                    etag: None,
                    ctag: None,
                },
            );
        }
        let mut aliases = self.directory_aliases.lock().unwrap();
        for (local_id, remote_id) in mappings {
            if let Some(node) = local.values().find(|node| node.item_id == local_id) {
                aliases.insert(remote_id, node.inode);
            }
        }
        Ok(())
    }

    fn create_file(&self, parent: u64, name: &str, flags: OpenFlags) -> Result<(Node, u64), Errno> {
        let Some(root) = &self.draft_root else {
            return Err(Errno::EROFS);
        };
        if !NewFileLocation::valid_name(name) {
            return Err(Errno::EINVAL);
        }
        let _serialized = self.open_lock.lock().unwrap();
        let parent_node = self.node(parent).ok_or(Errno::ENOENT)?;
        if parent_node.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        let mut local = self.local_files.lock().unwrap();
        if self.name_taken(parent, &parent_node.item_id, name, None, &local) {
            return Err(Errno::EEXIST);
        }
        let store = DraftStore::open(root).map_err(map_draft_error)?;
        let temporary = tempfile::NamedTempFile::new_in(root).map_err(map_draft_error)?;
        let random = temporary.path().file_name().unwrap().to_string_lossy();
        let item_id = format!("local:{random}");
        drop(temporary);
        let identity = DraftIdentity {
            drive_id: self.tree.drive_id().to_owned(),
            item_id: item_id.clone(),
            base_etag: "local-new".to_owned(),
            new_file: Some(NewFileLocation {
                parent_id: parent_node.item_id.clone(),
                name: name.to_owned(),
            }),
        };
        let inode = crate::stable_inode(self.tree.drive_id(), &item_id);
        if self.tree.getattr(inode).is_some() || local.contains_key(&inode) {
            return Err(Errno::EEXIST);
        }
        let draft = store.begin_empty(identity).map_err(map_draft_error)?;
        let node = Node {
            inode,
            parent_inode: parent,
            item_id,
            name: name.to_owned(),
            kind: NodeKind::File,
            size: 0,
            etag: None,
            ctag: None,
        };
        local.insert(inode, node.clone());
        let handle = self.insert_open(inode, OpenHandle::Draft(Arc::new(draft)), flags);
        Ok((node, handle))
    }

    fn create_regular_node(&self, parent: u64, name: &str, mode: u32) -> Result<Node, Errno> {
        if !matches!(mode & libc::S_IFMT, 0 | libc::S_IFREG) {
            return Err(Errno::EOPNOTSUPP);
        }
        let (node, handle) = self.create_file(parent, name, OpenFlags(libc::O_WRONLY))?;
        self.files.lock().unwrap().remove(&handle);
        Ok(node)
    }

    fn queue_remote_change(&self, node: &Node, kind: MutationKind) -> Result<(), Errno> {
        let Some(root) = &self.draft_root else {
            return Err(Errno::EROFS);
        };
        let remote_inode = crate::stable_inode(self.tree.drive_id(), &node.item_id);
        if self
            .files
            .lock()
            .unwrap()
            .values()
            .any(|open| open.writable && (open.inode == node.inode || open.inode == remote_inode))
        {
            return Err(Errno::EBUSY);
        }
        if self.draft_for_node(node)?.is_some() {
            return Err(Errno::EBUSY);
        }
        let parent = self.node(node.parent_inode).ok_or(Errno::EIO)?;
        let mutation = Mutation {
            drive_id: self.tree.drive_id().to_owned(),
            item_id: node.item_id.clone(),
            parent_id: self.remote_parent_id(&parent)?,
            name: node.name.clone(),
            etag: node
                .etag
                .clone()
                .filter(|etag| !etag.is_empty())
                .ok_or(Errno::EIO)?,
            directory: node.kind == NodeKind::Directory,
            kind,
            status: MutationStatus::Pending,
        };
        let mut changes = self.remote_changes.lock().unwrap();
        if changes.contains_key(&remote_inode) {
            return Err(Errno::EBUSY);
        }
        DraftStore::open(root).map_err(map_draft_error)?;
        MutationStore::open(root)
            .and_then(|store| store.add(&mutation))
            .map_err(map_draft_error)?;
        changes.insert(remote_inode, mutation);
        Ok(())
    }

    fn remove_local_file(&self, parent: u64, name: &str) -> Result<(), Errno> {
        let _serialized = self.open_lock.lock().unwrap();
        let Some(root) = &self.draft_root else {
            return Err(Errno::EROFS);
        };
        let mut local = self.local_files.lock().unwrap();
        let node = self
            .visible_child(parent, name, &local)
            .ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::File {
            return Err(Errno::EISDIR);
        }
        if self.is_publishing(node.inode) {
            return Err(Errno::EBUSY);
        }
        if !node.item_id.starts_with("local:") {
            drop(local);
            return self.queue_remote_change(&node, MutationKind::Delete);
        }
        let files = self.files.lock().unwrap();
        if files
            .values()
            .any(|open| open.inode == node.inode && !matches!(open.handle, OpenHandle::Draft(_)))
        {
            return Err(Errno::EIO);
        }
        DraftStore::open(root)
            .and_then(|store| store.remove_new(self.tree.drive_id(), &node.item_id))
            .map_err(map_draft_error)?;
        let mut detached = false;
        for open in files.values().filter(|open| open.inode == node.inode) {
            if let OpenHandle::Draft(draft) = &open.handle {
                draft.mark_detached();
                detached = true;
            }
        }
        if detached {
            self.detached_files
                .lock()
                .unwrap()
                .insert(node.inode, node.clone());
        }
        local.remove(&node.inode);
        Ok(())
    }

    fn create_directory(&self, parent: u64, name: &str) -> Result<Node, Errno> {
        let Some(root) = &self.draft_root else {
            return Err(Errno::EROFS);
        };
        if !NewFileLocation::valid_name(name) {
            return Err(Errno::EINVAL);
        }
        let _serialized = self.open_lock.lock().unwrap();
        let mut local = self.local_files.lock().unwrap();
        let parent_node = local
            .get(&parent)
            .or_else(|| self.tree.getattr(parent))
            .ok_or(Errno::ENOENT)?;
        if parent_node.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        if self.name_taken(parent, &parent_node.item_id, name, None, &local) {
            return Err(Errno::EEXIST);
        }
        DraftStore::open(root).map_err(map_draft_error)?;
        let directory = DirectoryStore::open(root)
            .and_then(|store| store.create(self.tree.drive_id(), &parent_node.item_id, name))
            .map_err(map_draft_error)?;
        let inode = crate::stable_inode(self.tree.drive_id(), &directory.item_id);
        let node = Node {
            inode,
            parent_inode: parent,
            item_id: directory.item_id,
            name: name.to_owned(),
            kind: NodeKind::Directory,
            size: 0,
            etag: None,
            ctag: None,
        };
        local.insert(inode, node.clone());
        Ok(node)
    }

    fn remove_local_directory(&self, parent: u64, name: &str) -> Result<(), Errno> {
        let _serialized = self.open_lock.lock().unwrap();
        let Some(root) = &self.draft_root else {
            return Err(Errno::EROFS);
        };
        let local = self.local_files.lock().unwrap();
        let node = self
            .visible_child(parent, name, &local)
            .ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        if self.is_publishing(node.inode) {
            return Err(Errno::EBUSY);
        }
        drop(local);
        if self.directory_entries(node.inode).ok_or(Errno::EIO)?.len() > 2 {
            return Err(Errno::ENOTEMPTY);
        }
        if self
            .tree
            .readdir(node.inode)
            .is_some_and(|children| !children.is_empty())
        {
            return Err(Errno::EBUSY);
        }
        if !node.item_id.starts_with("local-dir:") {
            return self.queue_remote_change(&node, MutationKind::Delete);
        }
        let mut local = self.local_files.lock().unwrap();
        let store = DirectoryStore::open(root).map_err(map_draft_error)?;
        let directory = store
            .list()
            .map_err(map_draft_error)?
            .into_iter()
            .find(|directory| directory.item_id == node.item_id)
            .ok_or(Errno::EIO)?;
        if directory.remote_id.is_some() || directory.created_remote_id.is_some() {
            return Err(Errno::EBUSY);
        }
        store.remove(&directory).map_err(map_draft_error)?;
        local.remove(&node.inode);
        Ok(())
    }

    fn rename_local_file(
        &self,
        parent: u64,
        name: &str,
        new_parent: u64,
        new_name: &str,
        flags: fuser::RenameFlags,
    ) -> Result<(), Errno> {
        let _serialized = self.open_lock.lock().unwrap();
        let Some(root) = &self.draft_root else {
            return Err(Errno::EROFS);
        };
        if flags.bits() & !libc::RENAME_NOREPLACE != 0 {
            return Err(Errno::EINVAL);
        }
        if !NewFileLocation::valid_name(new_name) {
            return Err(Errno::EINVAL);
        }
        let parent_node = self.node(new_parent).ok_or(Errno::ENOENT)?;
        if parent_node.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        let mut local = self.local_files.lock().unwrap();
        let node = self
            .visible_child(parent, name, &local)
            .ok_or(Errno::ENOENT)?;
        if parent == new_parent && name == new_name {
            return Ok(());
        }
        let target = self.visible_child(new_parent, new_name, &local);
        if self.is_publishing(node.inode)
            || target
                .as_ref()
                .is_some_and(|target| self.is_publishing(target.inode))
        {
            return Err(Errno::EBUSY);
        }
        if let Some(target) = target.clone()
            && node.item_id.starts_with("local:")
            && target.kind == NodeKind::File
            && !target.item_id.starts_with("local:")
            && flags.bits() & libc::RENAME_NOREPLACE == 0
        {
            if self
                .files
                .lock()
                .unwrap()
                .values()
                .any(|open| open.inode == target.inode)
            {
                return Err(Errno::EBUSY);
            }
            if self
                .remote_changes
                .lock()
                .unwrap()
                .contains_key(&crate::stable_inode(self.tree.drive_id(), &target.item_id))
            {
                return Err(Errno::EEXIST);
            }
            let store = DraftStore::open(root).map_err(map_draft_error)?;
            let source = store
                .recover_item(self.tree.drive_id(), &node.item_id)
                .map_err(map_draft_error)?
                .ok_or(Errno::EIO)?;
            let snapshot = store
                .snapshot(source.identity())
                .map_err(map_draft_error)?
                .ok_or(Errno::EIO)?;
            let request = self.request_for_node(&target).map_err(map_open_error)?;
            let identity = DraftIdentity {
                drive_id: request.drive_id,
                item_id: request.item_id,
                base_etag: request.etag,
                new_file: None,
            };
            let replacement_draft = if self.draft_for_node(&target)?.is_some() {
                store.replace_from_file(&identity, snapshot.file.as_file())
            } else {
                store.begin_from_file(identity, snapshot.file.as_file())
            }
            .map_err(map_draft_error)?;
            store
                .remove_new(self.tree.drive_id(), &node.item_id)
                .map_err(map_draft_error)?;
            if target.inode != node.inode {
                local.remove(&target.inode);
            }
            let replacement_draft = Arc::new(replacement_draft);
            for open in self.files.lock().unwrap().values_mut() {
                if open.inode == node.inode {
                    open.handle = OpenHandle::Draft(Arc::clone(&replacement_draft));
                }
            }
            let replacement = local.get_mut(&node.inode).ok_or(Errno::EIO)?;
            replacement.parent_inode = new_parent;
            replacement.name = new_name.to_owned();
            replacement.item_id = target.item_id;
            replacement.size = snapshot.size;
            replacement.etag = target.etag;
            return Ok(());
        }
        if let Some(target) = target
            && flags.bits() & libc::RENAME_NOREPLACE == 0
            && node.item_id.starts_with("local:")
            && target.item_id.starts_with("local:")
            && node.kind == NodeKind::File
            && target.kind == NodeKind::File
        {
            let files = self.files.lock().unwrap();
            if files.values().any(|open| {
                open.inode == target.inode && !matches!(open.handle, OpenHandle::Draft(_))
            }) {
                return Err(Errno::EIO);
            }
            let store = DraftStore::open(root).map_err(map_draft_error)?;
            let old_location = NewFileLocation {
                parent_id: local
                    .get(&parent)
                    .or_else(|| self.tree.getattr(parent))
                    .ok_or(Errno::ENOENT)?
                    .item_id
                    .clone(),
                name: name.to_owned(),
            };
            store
                .relocate_new(
                    self.tree.drive_id(),
                    &node.item_id,
                    NewFileLocation {
                        parent_id: parent_node.item_id,
                        name: new_name.to_owned(),
                    },
                )
                .map_err(map_draft_error)?;
            if let Err(error) = store.remove_new(self.tree.drive_id(), &target.item_id) {
                let _ = store.relocate_new(self.tree.drive_id(), &node.item_id, old_location);
                return Err(map_draft_error(error));
            }
            let mut detached = false;
            for open in files.values().filter(|open| open.inode == target.inode) {
                if let OpenHandle::Draft(draft) = &open.handle {
                    draft.mark_detached();
                    detached = true;
                }
            }
            if detached {
                self.detached_files
                    .lock()
                    .unwrap()
                    .insert(target.inode, target.clone());
            }
            local.remove(&target.inode);
            let moved = local.get_mut(&node.inode).ok_or(Errno::EIO)?;
            moved.parent_inode = new_parent;
            moved.name = new_name.to_owned();
            return Ok(());
        }
        if let Some(target) = local
            .values()
            .find(|entry| entry.parent_inode == new_parent && entry.name == new_name)
            .cloned()
            && flags.bits() & libc::RENAME_NOREPLACE == 0
            && node.item_id.starts_with("local-dir:")
            && target.item_id.starts_with("local-dir:")
            && node.kind == NodeKind::Directory
            && target.kind == NodeKind::Directory
        {
            if local
                .values()
                .any(|child| child.parent_inode == target.inode)
            {
                return Err(Errno::ENOTEMPTY);
            }
            let mut ancestor = new_parent;
            while ancestor != crate::ROOT_INODE {
                if ancestor == node.inode {
                    return Err(Errno::EINVAL);
                }
                ancestor = local
                    .get(&ancestor)
                    .or_else(|| self.tree.getattr(ancestor))
                    .ok_or(Errno::ENOENT)?
                    .parent_inode;
            }
            let store = DirectoryStore::open(root).map_err(map_draft_error)?;
            let directories = store.list().map_err(map_draft_error)?;
            let source = directories
                .iter()
                .find(|directory| directory.item_id == node.item_id)
                .ok_or(Errno::EIO)?;
            let destination = directories
                .iter()
                .find(|directory| directory.item_id == target.item_id)
                .ok_or(Errno::EIO)?;
            if source.remote_id.is_some()
                || source.created_remote_id.is_some()
                || destination.remote_id.is_some()
                || destination.created_remote_id.is_some()
            {
                return Err(Errno::EBUSY);
            }
            store
                .relocate(source, &parent_node.item_id, new_name)
                .map_err(map_draft_error)?;
            if let Err(error) = store.remove(destination) {
                let _ = store.relocate(source, &source.parent_id, &source.name);
                return Err(map_draft_error(error));
            }
            local.remove(&target.inode);
            let moved = local.get_mut(&node.inode).ok_or(Errno::EIO)?;
            moved.parent_inode = new_parent;
            moved.name = new_name.to_owned();
            return Ok(());
        }
        if self.name_taken(
            new_parent,
            &parent_node.item_id,
            new_name,
            Some(node.inode),
            &local,
        ) {
            return Err(Errno::EEXIST);
        }
        if node.kind == NodeKind::Directory {
            let mut ancestor = new_parent;
            while ancestor != crate::ROOT_INODE {
                if ancestor == node.inode {
                    return Err(Errno::EINVAL);
                }
                ancestor = local
                    .get(&ancestor)
                    .or_else(|| self.tree.getattr(ancestor))
                    .ok_or(Errno::ENOENT)?
                    .parent_inode;
            }
        }
        if !node.item_id.starts_with("local:") && !node.item_id.starts_with("local-dir:") {
            let target_parent_id = self.remote_parent_id(&parent_node)?;
            drop(local);
            return self.queue_remote_change(
                &node,
                MutationKind::Rename {
                    target_parent_id,
                    target_name: new_name.to_owned(),
                },
            );
        }
        if node.kind == NodeKind::Directory {
            let store = DirectoryStore::open(root).map_err(map_draft_error)?;
            let directory = store
                .list()
                .map_err(map_draft_error)?
                .into_iter()
                .find(|directory| directory.item_id == node.item_id)
                .ok_or(Errno::EIO)?;
            if directory.remote_id.is_some() || directory.created_remote_id.is_some() {
                return Err(Errno::EBUSY);
            }
            store
                .relocate(&directory, &parent_node.item_id, new_name)
                .map_err(map_draft_error)?;
        } else {
            DraftStore::open(root)
                .and_then(|store| {
                    store.relocate_new(
                        self.tree.drive_id(),
                        &node.item_id,
                        NewFileLocation {
                            parent_id: parent_node.item_id,
                            name: new_name.to_owned(),
                        },
                    )
                })
                .map_err(map_draft_error)?;
        }
        let moved = local.get_mut(&node.inode).ok_or(Errno::EIO)?;
        moved.parent_inode = new_parent;
        moved.name = new_name.to_owned();
        Ok(())
    }

    fn directory_entries(&self, inode: u64) -> Option<Vec<DirectoryEntry>> {
        let directory = self.node(inode)?;
        if directory.kind != NodeKind::Directory {
            return None;
        }
        let children = self.tree.readdir(inode).unwrap_or_default();
        let mut entries = vec![
            (inode, FileType::Directory, ".".to_owned()),
            (directory.parent_inode, FileType::Directory, "..".to_owned()),
        ];
        entries.extend(
            children
                .into_iter()
                .filter_map(|node| self.remote_node(node))
                .filter(|node| node.parent_inode == inode)
                .map(|node| {
                    (
                        node.inode,
                        if node.kind == NodeKind::Directory {
                            FileType::Directory
                        } else {
                            FileType::RegularFile
                        },
                        node.name.clone(),
                    )
                }),
        );
        let changes: Vec<_> = self
            .remote_changes
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for mutation in changes {
            if let MutationKind::Rename {
                target_parent_id, ..
            } = &mutation.kind
                && self.target_parent_matches(inode, &directory.item_id, target_parent_id)
                && let Some(source) = self
                    .tree
                    .getattr(crate::stable_inode(&mutation.drive_id, &mutation.item_id))
                && source.parent_inode != inode
                && let Some(node) = self.remote_node(source)
            {
                entries.push((
                    node.inode,
                    if node.kind == NodeKind::Directory {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    },
                    node.name,
                ));
            }
        }
        let local_inodes: Vec<_> = self
            .local_files
            .lock()
            .unwrap()
            .values()
            .map(|node| node.inode)
            .collect();
        let local_entries: Vec<_> = local_inodes
            .into_iter()
            .filter_map(|local_inode| self.node(local_inode))
            .filter(|node| node.parent_inode == inode)
            .map(|node| {
                (
                    node.inode,
                    if node.kind == NodeKind::Directory {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    },
                    node.name.clone(),
                )
            })
            .collect();
        entries.retain(|entry| {
            entry.2 == "."
                || entry.2 == ".."
                || !local_entries.iter().any(|local| local.2 == entry.2)
        });
        entries.extend(local_entries);
        entries[2..].sort_by(|left, right| left.2.cmp(&right.2));
        Some(entries)
    }

    fn open_directory(&self, inode: u64) -> Result<u64, Errno> {
        let node = self.node(inode).ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        let entries = self.directory_entries(inode).ok_or(Errno::ENOENT)?;
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.directories.lock().unwrap().insert(
            handle,
            OpenDirectory {
                inode,
                entries: Arc::new(entries),
            },
        );
        Ok(handle)
    }

    fn directory_entries_for_handle(
        &self,
        inode: u64,
        handle: u64,
    ) -> Result<Arc<Vec<DirectoryEntry>>, Errno> {
        self.directories
            .lock()
            .unwrap()
            .get(&handle)
            .filter(|open| open.inode == inode)
            .map(|open| Arc::clone(&open.entries))
            .ok_or(Errno::EBADF)
    }

    fn close_directory(&self, inode: u64, handle: u64) -> Result<(), Errno> {
        let mut directories = self.directories.lock().unwrap();
        if directories
            .get(&handle)
            .is_none_or(|open| open.inode != inode)
        {
            return Err(Errno::EBADF);
        }
        directories.remove(&handle);
        Ok(())
    }

    fn open_file(&self, inode: u64, flags: OpenFlags) -> Result<u64, Errno> {
        let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
        if (writable && self.draft_root.is_none()) || (!writable && flags.0 & libc::O_TRUNC != 0) {
            return Err(Errno::EROFS);
        }
        let _serialized = self.open_lock.lock().unwrap();
        if self.is_publishing(inode) {
            return Err(Errno::EBUSY);
        }
        let node = self.node(inode).ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::File {
            return Err(Errno::EISDIR);
        }
        if let Some(draft) = self.draft_for_node(&node)? {
            if writable && flags.0 & libc::O_TRUNC != 0 {
                draft.set_len(0).map_err(map_draft_error)?;
            }
            return Ok(self.insert_open(inode, OpenHandle::Draft(Arc::new(draft)), flags));
        }
        if node.item_id.starts_with("local:") {
            return Err(Errno::EIO);
        }
        if writable && flags.0 & libc::O_TRUNC != 0 {
            let request = self.request_for_node(&node).map_err(map_open_error)?;
            let store =
                DraftStore::open(self.draft_root.as_ref().unwrap()).map_err(map_draft_error)?;
            let draft = store
                .begin_empty(DraftIdentity {
                    drive_id: request.drive_id,
                    item_id: request.item_id,
                    base_etag: request.etag,
                    new_file: None,
                })
                .map_err(map_draft_error)?;
            return Ok(self.insert_open(inode, OpenHandle::Draft(Arc::new(draft)), flags));
        }
        if writable {
            let request = self.request_for_node(&node).map_err(map_open_error)?;
            return Ok(self.insert_open(
                inode,
                OpenHandle::Pending(DraftIdentity {
                    drive_id: request.drive_id,
                    item_id: request.item_id,
                    base_etag: request.etag,
                    new_file: None,
                }),
                flags,
            ));
        }
        let _opening = self.mark_opening(inode);
        drop(_serialized);
        let opened = self.open_cached(inode)?;
        let _serialized = self.open_lock.lock().unwrap();
        if self.is_publishing(inode) {
            return Err(Errno::EBUSY);
        }
        let node = self.node(inode).ok_or(Errno::ENOENT)?;
        if let Some(draft) = self.draft_for_node(&node)? {
            return Ok(self.insert_open(inode, OpenHandle::Draft(Arc::new(draft)), flags));
        }
        Ok(self.insert_open(inode, OpenHandle::Cached(Arc::new(opened)), flags))
    }

    fn open_cached(&self, inode: u64) -> Result<CachedHandle, Errno> {
        match self.cached_handle(inode) {
            Ok(handle) => return Ok(handle),
            Err(OpenError::NotCached(_)) => {}
            Err(error) => return Err(map_open_error(error)),
        }
        // Only a cache miss waits for another download. Cached files remain
        // available while Graph is transferring an unrelated file.
        let _serialized = self.download_lock.lock().unwrap();
        match self.cached_handle(inode) {
            Ok(handle) => return Ok(handle),
            Err(OpenError::NotCached(request)) => {
                (self.loader)(&request).map_err(map_loader_error)?;
            }
            Err(error) => return Err(map_open_error(error)),
        }
        self.cached_handle(inode).map_err(map_open_error)
    }

    fn cached_handle(&self, inode: u64) -> Result<CachedHandle, OpenError> {
        let mut cache = CacheStore::open_waiting(&self.cache_root, self.cache_limit)
            .map_err(OpenError::Cache)?;
        let node = self.node(inode).ok_or(OpenError::MissingFile)?;
        if node.kind != NodeKind::File {
            return Err(OpenError::IsDirectory);
        }
        VirtualTree::open_cached_request(self.request_for_node(&node)?, &mut cache)
    }

    fn insert_open(&self, inode: u64, handle: OpenHandle, flags: OpenFlags) -> u64 {
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.files.lock().unwrap().insert(
            id,
            OpenFile {
                inode,
                handle,
                readable: flags.acc_mode() != OpenAccMode::O_WRONLY,
                writable: flags.acc_mode() != OpenAccMode::O_RDONLY,
                append: flags.0 & libc::O_APPEND != 0,
            },
        );
        id
    }

    fn read_file(&self, inode: u64, handle: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let opened = self
            .files
            .lock()
            .unwrap()
            .get(&handle)
            .filter(|open| open.inode == inode)
            .filter(|open| open.readable)
            .ok_or(Errno::EBADF)?
            .handle
            .clone();
        if size == 0 {
            return Ok(Vec::new());
        }
        if self.draft_root.is_some()
            && let Some(node) = self.node(inode)
            && let Some(draft) = self.draft_for_node(&node)?
        {
            return read_draft(&draft, offset, size);
        }
        if matches!(opened, OpenHandle::Pending(_)) {
            // A cold read may wait for Graph. Recheck the draft after the load:
            // another handle may have replaced the remote version meanwhile.
            let cached = self.open_cached(inode);
            let _serialized = self.open_lock.lock().unwrap();
            let opened = self
                .files
                .lock()
                .unwrap()
                .get(&handle)
                .filter(|open| open.inode == inode)
                .filter(|open| open.readable)
                .ok_or(Errno::EBADF)?
                .handle
                .clone();
            if let OpenHandle::Draft(draft) = &opened {
                return read_draft(draft, offset, size);
            }
            if let Some(node) = self.node(inode)
                && let Some(draft) = self.draft_for_node(&node)?
            {
                return read_draft(&draft, offset, size);
            }
            return cached?.read(offset, size).map_err(|_| Errno::EIO);
        }
        match &opened {
            OpenHandle::Cached(handle) => handle.read(offset, size).map_err(|_| Errno::EIO),
            OpenHandle::Draft(handle) => read_draft(handle, offset, size),
            OpenHandle::Pending(_) => unreachable!(),
        }
    }

    fn materialize_pending(&self, inode: u64, handle: u64, empty: bool) -> Result<(), Errno> {
        {
            let files = self.files.lock().unwrap();
            let open = files
                .get(&handle)
                .filter(|open| open.inode == inode && open.writable)
                .ok_or(Errno::EBADF)?;
            if matches!(open.handle, OpenHandle::Draft(_)) {
                return Ok(());
            }
            if !matches!(open.handle, OpenHandle::Pending(_)) {
                return Err(Errno::EROFS);
            }
        }
        let identity = {
            let files = self.files.lock().unwrap();
            let open = files
                .get(&handle)
                .filter(|open| open.inode == inode && open.writable)
                .ok_or(Errno::EBADF)?;
            match &open.handle {
                OpenHandle::Draft(_) => return Ok(()),
                OpenHandle::Pending(identity) => identity.clone(),
                OpenHandle::Cached(_) => return Err(Errno::EROFS),
            }
        };
        let store = DraftStore::open(self.draft_root.as_ref().ok_or(Errno::EROFS)?)
            .map_err(map_draft_error)?;
        // Download outside open_lock. A concurrent truncation can create a
        // draft during the wait, so recover it before using the cached source.
        let cached = if !empty && store.recover(&identity).map_err(map_draft_error)?.is_none() {
            Some(self.open_cached(inode))
        } else {
            None
        };
        let _serialized = self.open_lock.lock().unwrap();
        {
            let files = self.files.lock().unwrap();
            let open = files
                .get(&handle)
                .filter(|open| open.inode == inode && open.writable)
                .ok_or(Errno::EBADF)?;
            if matches!(open.handle, OpenHandle::Draft(_)) {
                return Ok(());
            }
        }
        let draft = if let Some(existing) = store.recover(&identity).map_err(map_draft_error)? {
            existing
        } else if empty {
            store.begin_empty(identity).map_err(map_draft_error)?
        } else {
            let cached = cached.ok_or(Errno::EIO)??;
            store
                .begin_from_file(identity, cached.source_file())
                .map_err(map_draft_error)?
        };
        let mut files = self.files.lock().unwrap();
        let open = files
            .get_mut(&handle)
            .filter(|open| open.inode == inode && open.writable)
            .ok_or(Errno::EBADF)?;
        if matches!(open.handle, OpenHandle::Pending(_)) {
            open.handle = OpenHandle::Draft(Arc::new(draft));
        }
        Ok(())
    }

    fn write_file(&self, inode: u64, handle: u64, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        {
            let files = self.files.lock().unwrap();
            let open = files
                .get(&handle)
                .filter(|open| open.inode == inode && open.writable)
                .ok_or(Errno::EBADF)?;
            if data.is_empty() {
                return Ok(0);
            }
            if !open.append
                && offset
                    .checked_add(u64::try_from(data.len()).map_err(|_| Errno::EINVAL)?)
                    .is_none_or(|end| end > i64::MAX as u64)
            {
                return Err(Errno::EINVAL);
            }
        }
        self.materialize_pending(inode, handle, false)?;
        let _serialized = self.open_lock.lock().unwrap();
        let files = self.files.lock().unwrap();
        let open = files
            .get(&handle)
            .filter(|open| open.inode == inode)
            .ok_or(Errno::EBADF)?;
        if !open.writable {
            return Err(Errno::EBADF);
        }
        let OpenHandle::Draft(draft) = &open.handle else {
            return Err(Errno::EROFS);
        };
        let draft = Arc::clone(draft);
        let append = open.append;
        drop(files);
        let written = if append {
            draft.append(data)
        } else {
            draft.write_at(offset, data)
        }
        .map_err(map_draft_error)?;
        u32::try_from(written).map_err(|_| Errno::EIO)
    }

    fn sync_file(&self, inode: u64, handle: u64) -> Result<(), Errno> {
        let opened = self
            .files
            .lock()
            .unwrap()
            .get(&handle)
            .filter(|open| open.inode == inode)
            .ok_or(Errno::EBADF)?
            .handle
            .clone();
        if let OpenHandle::Draft(draft) = opened {
            draft.sync().map_err(map_draft_error)?;
        }
        Ok(())
    }

    fn close_file(&self, inode: u64, handle: u64) {
        let mut files = self.files.lock().unwrap();
        if files.get(&handle).is_some_and(|open| open.inode == inode) {
            files.remove(&handle);
        }
        if !files.values().any(|open| open.inode == inode) {
            self.detached_files.lock().unwrap().remove(&inode);
        }
    }

    fn sync_directory(&self, inode: u64) -> Result<(), Errno> {
        let node = self.node(inode).ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        if let Some(root) = &self.draft_root {
            File::open(root)
                .and_then(|directory| directory.sync_all())
                .map_err(map_draft_error)?;
        }
        Ok(())
    }

    fn local_space(&self) -> Result<LocalSpace, Errno> {
        local_space(self.draft_root.as_deref().unwrap_or(&self.cache_root)).map_err(map_draft_error)
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
            self.materialize_pending(inode, owned_handle, size == 0)?;
            let _serialized = self.open_lock.lock().unwrap();
            let files = self.files.lock().unwrap();
            let open = files
                .get(&owned_handle)
                .filter(|open| open.inode == inode && open.writable)
                .ok_or(Errno::EBADF)?;
            let OpenHandle::Draft(draft) = &open.handle else {
                return Err(Errno::EROFS);
            };
            let draft = Arc::clone(draft);
            drop(files);
            draft.set_len(size).map_err(map_draft_error)
        })();
        if handle.is_none() {
            self.files.lock().unwrap().remove(&owned_handle);
        }
        result
    }

    fn allocate_file(
        &self,
        inode: u64,
        handle: u64,
        offset: u64,
        length: u64,
        mode: i32,
    ) -> Result<(), Errno> {
        if mode != 0 && mode != libc::FALLOC_FL_KEEP_SIZE {
            return Err(Errno::EOPNOTSUPP);
        }
        if length == 0
            || offset
                .checked_add(length)
                .is_none_or(|end| end > i64::MAX as u64)
        {
            return Err(Errno::EINVAL);
        }
        self.materialize_pending(inode, handle, false)?;
        let _serialized = self.open_lock.lock().unwrap();
        let files = self.files.lock().unwrap();
        let open = files
            .get(&handle)
            .filter(|open| open.inode == inode && open.writable)
            .ok_or(Errno::EBADF)?;
        let OpenHandle::Draft(draft) = &open.handle else {
            return Err(Errno::EROFS);
        };
        let draft = Arc::clone(draft);
        drop(files);
        draft
            .allocate(offset, length, mode == libc::FALLOC_FL_KEEP_SIZE)
            .map_err(|error| match error.raw_os_error() {
                Some(libc::EOPNOTSUPP) => Errno::EOPNOTSUPP,
                Some(libc::EINVAL) => Errno::EINVAL,
                _ => map_draft_error(error),
            })
    }

    fn seek_file(&self, inode: u64, handle: u64, offset: i64, whence: i32) -> Result<i64, Errno> {
        if offset < 0 || !matches!(whence, libc::SEEK_DATA | libc::SEEK_HOLE) {
            return Err(Errno::EINVAL);
        }
        let opened = self
            .files
            .lock()
            .unwrap()
            .get(&handle)
            .filter(|open| open.inode == inode)
            .ok_or(Errno::EBADF)?
            .handle
            .clone();
        let node = self.node(inode).ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::File {
            return Err(Errno::EISDIR);
        }
        if self.detached_files.lock().unwrap().contains_key(&inode)
            && let OpenHandle::Draft(draft) = &opened
        {
            return draft.seek_data_hole(offset, whence).map_err(map_seek_error);
        }
        if let Some(draft) = self.draft_for_node(&node)? {
            return draft.seek_data_hole(offset, whence).map_err(map_seek_error);
        }
        // Graph exposes file size, but not physical allocation. A regular file may
        // legally report all bytes as data and one implicit hole at EOF.
        if offset as u64 >= node.size {
            return Err(Errno::ENXIO);
        }
        if whence == libc::SEEK_DATA {
            Ok(offset)
        } else {
            i64::try_from(node.size).map_err(|_| Errno::EOVERFLOW)
        }
    }

    // Keep the FUSE source and destination arguments explicit to avoid swapping offsets.
    #[allow(clippy::too_many_arguments)]
    fn copy_range(
        &self,
        source_inode: u64,
        source_handle: u64,
        source_offset: u64,
        target_inode: u64,
        target_handle: u64,
        target_offset: u64,
        length: u64,
    ) -> Result<u32, Errno> {
        if source_inode == target_inode
            || source_offset > i64::MAX as u64
            || target_offset > i64::MAX as u64
        {
            return Err(Errno::EINVAL);
        }
        {
            let files = self.files.lock().unwrap();
            if files
                .get(&source_handle)
                .is_none_or(|open| open.inode != source_inode || !open.readable)
                || files
                    .get(&target_handle)
                    .is_none_or(|open| open.inode != target_inode || !open.writable || open.append)
            {
                return Err(Errno::EBADF);
            }
        }
        if matches!(
            (self.node(source_inode), self.node(target_inode)),
            (Some(source), Some(target)) if source.item_id == target.item_id
        ) {
            return Err(Errno::EINVAL);
        }
        let length = length
            .min(u32::MAX as u64)
            .min((i64::MAX as u64).saturating_sub(source_offset))
            .min((i64::MAX as u64).saturating_sub(target_offset));
        let mut copied = 0_u64;
        while copied < length {
            let amount = (length - copied).min(1024 * 1024) as u32;
            let bytes =
                match self.read_file(source_inode, source_handle, source_offset + copied, amount) {
                    Ok(bytes) => bytes,
                    Err(error) if copied == 0 => return Err(error),
                    Err(_) => break,
                };
            if bytes.is_empty() {
                break;
            }
            let written = match self.write_file(
                target_inode,
                target_handle,
                target_offset + copied,
                &bytes,
            ) {
                Ok(written) if written > 0 => written,
                Ok(_) if copied == 0 => return Err(Errno::EIO),
                Err(error) if copied == 0 => return Err(error),
                _ => break,
            };
            copied += u64::from(written);
            if written as usize != bytes.len() {
                break;
            }
        }
        Ok(copied as u32)
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
    if error.kind() == io::ErrorKind::WouldBlock {
        return Errno::EBUSY;
    }
    match error.raw_os_error() {
        Some(libc::ENOSPC) => Errno::ENOSPC,
        Some(libc::EDQUOT) => Errno::EDQUOT,
        _ => Errno::EIO,
    }
}

fn map_seek_error(error: io::Error) -> Errno {
    match error.raw_os_error() {
        Some(libc::ENXIO) => Errno::ENXIO,
        Some(libc::EINVAL) => Errno::EINVAL,
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
        match self.lookup_node(parent.0, name) {
            Some(node) => match self.attr(&node) {
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
        match self.node(inode.0) {
            Some(node) => match self.attr(&node) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(error) => reply.error(error),
            },
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setxattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(self.reject_xattr_write(inode.0));
    }

    fn getxattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        match self.check_xattr_node(inode.0) {
            Ok(()) => reply.error(Errno::ENODATA),
            Err(error) => reply.error(error),
        }
    }

    fn listxattr(&self, _request: &Request, inode: INodeNo, size: u32, reply: ReplyXattr) {
        if let Err(error) = self.check_xattr_node(inode.0) {
            reply.error(error);
        } else if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn removexattr(&self, _request: &Request, inode: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(self.reject_xattr_write(inode.0));
    }

    fn setattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        handle: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if uid.is_some() || gid.is_some() || flags.is_some() {
            reply.error(Errno::EROFS);
            return;
        }
        if mode.is_some()
            && (self.draft_root.is_none()
                || self
                    .node(inode.0)
                    .is_none_or(|node| node.kind != NodeKind::File))
        {
            reply.error(Errno::EROFS);
            return;
        }
        let result = if let Some(size) = size {
            self.truncate_file(inode.0, handle.map(|handle| handle.0), size)
        } else if (mode.is_some() || atime.is_some() || mtime.is_some())
            && self.draft_root.is_some()
        {
            // OneDrive has no Unix mode metadata; accept the copy tool's chmod.
            Ok(())
        } else {
            Err(Errno::EROFS)
        };
        let result = result.and_then(|()| {
            if atime.is_none() && mtime.is_none() {
                return Ok(());
            }
            let node = self.node(inode.0).ok_or(Errno::ENOENT)?;
            let draft = if self.detached_files.lock().unwrap().contains_key(&inode.0) {
                let files = self.files.lock().unwrap();
                let open = files
                    .get(&handle.ok_or(Errno::EBADF)?.0)
                    .filter(|open| open.inode == inode.0)
                    .ok_or(Errno::EBADF)?;
                match &open.handle {
                    OpenHandle::Draft(draft) => Arc::clone(draft),
                    _ => return Err(Errno::EBADF),
                }
            } else {
                Arc::new(self.draft_for_node(&node)?.ok_or(Errno::EROFS)?)
            };
            let resolve = |time| match time {
                fuser::TimeOrNow::SpecificTime(time) => time,
                fuser::TimeOrNow::Now => std::time::SystemTime::now(),
            };
            draft
                .set_times(atime.map(resolve), mtime.map(resolve))
                .map_err(|_| Errno::EIO)
        });
        match result {
            Ok(()) => match self.node(inode.0).and_then(|node| self.attr(&node).ok()) {
                Some(attr) => reply.attr(&TTL, &attr),
                None => reply.error(Errno::EIO),
            },
            Err(error) => reply.error(error),
        }
    }

    fn opendir(&self, _request: &Request, inode: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.open_directory(inode.0) {
            Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::empty()),
            Err(error) => reply.error(error),
        }
    }

    fn readdir(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let entries = match self.directory_entries_for_handle(inode.0, handle.0) {
            Ok(entries) => entries,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let start = usize::try_from(offset)
            .unwrap_or(entries.len())
            .min(entries.len());
        for (index, (ino, kind, name)) in entries.iter().enumerate().skip(start) {
            if reply.add(INodeNo(*ino), (index + 1) as u64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let entries = match self.directory_entries_for_handle(inode.0, handle.0) {
            Ok(entries) => entries,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let start = usize::try_from(offset)
            .unwrap_or(entries.len())
            .min(entries.len());
        let store = if entries[start..]
            .iter()
            .any(|(_, kind, _)| *kind == FileType::RegularFile)
        {
            match self
                .draft_root
                .as_ref()
                .map(|root| DraftStore::open(root).map_err(|_| Errno::EIO))
                .transpose()
            {
                Ok(store) => store,
                Err(error) => {
                    reply.error(error);
                    return;
                }
            }
        } else {
            None
        };
        for (index, (entry_inode, _, name)) in entries.iter().enumerate().skip(start) {
            let attr = self.directory_entry_attr(*entry_inode, store.as_ref());
            let attr = match attr {
                Ok(attr) => attr,
                Err(Errno::ENOENT) => continue,
                Err(error) => {
                    reply.error(error);
                    return;
                }
            };
            if reply.add(
                attr.ino,
                (index + 1) as u64,
                name,
                &TTL,
                &attr,
                Generation(0),
            ) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        match self.close_directory(inode.0, handle.0) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn create(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.create_file(parent.0, name, OpenFlags(flags)) {
            Ok((node, handle)) => match self.attr(&node) {
                Ok(attr) => reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(handle),
                    FopenFlags::FOPEN_DIRECT_IO,
                ),
                Err(error) => reply.error(error),
            },
            Err(error) => reply.error(error),
        }
    }

    fn mknod(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        let result = name
            .to_str()
            .ok_or(Errno::EINVAL)
            .and_then(|name| self.create_regular_node(parent.0, name, mode));
        match result {
            Ok(node) => match self.attr(&node) {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(error) => reply.error(error),
            },
            Err(error) => reply.error(error),
        }
    }

    fn unlink(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match name
            .to_str()
            .ok_or(Errno::EINVAL)
            .and_then(|name| self.remove_local_file(parent.0, name))
        {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn mkdir(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let result = name
            .to_str()
            .ok_or(Errno::EINVAL)
            .and_then(|name| self.create_directory(parent.0, name));
        match result {
            Ok(node) => match self.attr(&node) {
                Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
                Err(error) => reply.error(error),
            },
            Err(error) => reply.error(error),
        }
    }

    fn rmdir(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = name
            .to_str()
            .ok_or(Errno::EINVAL)
            .and_then(|name| self.remove_local_directory(parent.0, name));
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn rename(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let result = name
            .to_str()
            .zip(new_name.to_str())
            .ok_or(Errno::EINVAL)
            .and_then(|(name, new_name)| {
                self.rename_local_file(parent.0, name, new_parent.0, new_name, flags)
            });
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
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

    fn copy_file_range(
        &self,
        _request: &Request,
        source_inode: INodeNo,
        source_handle: FileHandle,
        source_offset: u64,
        target_inode: INodeNo,
        target_handle: FileHandle,
        target_offset: u64,
        length: u64,
        flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        if !flags.is_empty() {
            reply.error(Errno::EINVAL);
            return;
        }
        match self.copy_range(
            source_inode.0,
            source_handle.0,
            source_offset,
            target_inode.0,
            target_handle.0,
            target_offset,
            length,
        ) {
            Ok(count) => reply.written(count),
            Err(error) => reply.error(error),
        }
    }

    fn fallocate(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        match self.allocate_file(inode.0, handle.0, offset, length, mode) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn lseek(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        match self.seek_file(inode.0, handle.0, offset, whence) {
            Ok(offset) => reply.offset(offset),
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
        match self.sync_file(inode.0, handle.0) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
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
        match self.sync_file(inode.0, handle.0) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn fsyncdir(
        &self,
        _request: &Request,
        inode: INodeNo,
        _handle: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.sync_directory(inode.0) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn statfs(&self, _request: &Request, _inode: INodeNo, reply: ReplyStatfs) {
        match self.local_space() {
            Ok(space) => reply.statfs(
                space.blocks,
                space.free_blocks,
                space.available_blocks,
                space.files,
                space.free_files,
                space.block_size,
                space.name_length,
                space.fragment_size,
            ),
            Err(error) => reply.error(error),
        }
    }

    fn release(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.close_file(inode.0, handle.0);
        reply.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ROOT_INODE;
    use onedrive_catalog::CatalogItem;
    use std::fs;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileExt;
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
    fn virtual_storage_reports_local_draft_space_and_syncs_directories() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&draft_root).unwrap();
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root,
            20,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let space = fuse.local_space().unwrap();
        assert!(space.blocks > 0);
        assert!(space.block_size > 0);
        assert!(space.available_blocks <= space.free_blocks);
        assert!(space.free_blocks <= space.blocks);
        assert_eq!(fuse.sync_directory(ROOT_INODE), Ok(()));
        let file_inode = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode;
        assert_eq!(fuse.sync_directory(file_inode), Err(Errno::ENOTDIR));
        assert_eq!(fuse.sync_directory(u64::MAX), Err(Errno::ENOENT));
    }

    #[test]
    fn xattrs_are_absent_and_cannot_be_written_to_virtual_items() {
        let dir = tempfile::tempdir().unwrap();
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let file = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        let (local, handle) = fuse
            .create_file(ROOT_INODE, "local.txt", OpenFlags(libc::O_WRONLY))
            .unwrap();
        fuse.files.lock().unwrap().remove(&handle);

        for inode in [ROOT_INODE, file.inode, local.inode] {
            assert_eq!(fuse.check_xattr_node(inode), Ok(()));
            assert_eq!(fuse.reject_xattr_write(inode), Errno::EOPNOTSUPP);
        }
        assert_eq!(fuse.check_xattr_node(u64::MAX), Err(Errno::ENOENT));
        assert_eq!(fuse.reject_xattr_write(u64::MAX), Errno::ENOENT);

        let readonly = VirtualFuse::new(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("readonly-cache"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        assert_eq!(readonly.reject_xattr_write(file.inode), Errno::EROFS);
    }

    #[test]
    fn mounted_virtual_xattrs_have_explicit_absent_and_unsupported_results() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        fs::create_dir(&mountpoint).unwrap();
        let session = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        )
        .mount(&mountpoint)
        .unwrap();
        let path = CString::new(mountpoint.join("текст.txt").as_os_str().as_bytes()).unwrap();
        let name = CString::new("user.example").unwrap();
        assert_eq!(
            unsafe { libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0) },
            0
        );
        let mut names = [0_u8; 16];
        assert_eq!(
            unsafe { libc::listxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) },
            0
        );
        assert_eq!(
            unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ENODATA)
        );
        assert_eq!(
            unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), b"x".as_ptr().cast(), 1, 0) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::EOPNOTSUPP)
        );
        assert_eq!(
            unsafe { libc::removexattr(path.as_ptr(), name.as_ptr()) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::EOPNOTSUPP)
        );
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_virtual_directory_supports_fsync_and_statvfs() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        fs::create_dir(&mountpoint).unwrap();
        let session = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            dir.path().join("drafts"),
            20,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        )
        .mount(&mountpoint)
        .unwrap();
        File::open(&mountpoint).unwrap().sync_all().unwrap();
        let space = local_space(&mountpoint).unwrap();
        assert!(space.blocks > 0);
        assert!(space.available_blocks > 0);
        session.umount_and_join().unwrap();
    }

    #[test]
    fn published_version_replaces_mounted_metadata_and_cache_request() {
        let dir = tempfile::tempdir().unwrap();
        let cache_root = dir.path().join("cache");
        let old_source = dir.path().join("old");
        let new_source = dir.path().join("new");
        fs::write(&old_source, b"hello").unwrap();
        fs::write(&new_source, b"updated").unwrap();
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let old_request = tree.request_for_item("file").unwrap();
        let mut new_request = old_request.clone();
        new_request.etag = "v2".into();
        new_request.size = 7;
        let mut cache = CacheStore::open(&cache_root, 20).unwrap();
        cache.put(&old_request.cache_key(), &old_source, 5).unwrap();
        drop(cache);
        let fuse = VirtualFuse::new_writable(
            tree,
            cache_root,
            dir.path().join("drafts"),
            20,
            Arc::new(|_| panic!("both versions are already cached")),
        );
        let inode = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode;
        let old = fuse.open_file(inode, OpenFlags(libc::O_RDONLY)).unwrap();
        assert_eq!(fuse.read_file(inode, old, 0, 10).unwrap(), b"hello");
        fuse.files.lock().unwrap().remove(&old);

        CacheStore::open(&fuse.cache_root, 20)
            .unwrap()
            .put(&new_request.cache_key(), &new_source, 7)
            .unwrap();

        let controller = fuse.published_version_controller();
        controller.publish_existing("file", "v2".into(), 7);
        let node = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        assert_eq!(node.etag.as_deref(), Some("v2"));
        assert_eq!(fuse.attr(&node).unwrap().size, 7);
        let new = fuse.open_file(inode, OpenFlags(libc::O_RDONLY)).unwrap();
        assert_eq!(fuse.read_file(inode, new, 0, 10).unwrap(), b"updated");
        fuse.files.lock().unwrap().remove(&new);
        let writable = fuse.open_file(inode, OpenFlags(libc::O_RDWR)).unwrap();
        let files = fuse.files.lock().unwrap();
        let OpenHandle::Pending(identity) = &files.get(&writable).unwrap().handle else {
            panic!("expected a pending remote write");
        };
        assert_eq!(identity.base_etag, "v2");
    }

    #[test]
    fn publication_lease_requires_idle_file_and_blocks_fuse_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let mut other = file_item();
        other.item_id = "other".into();
        other.name = Some("other.txt".into());
        let tree = VirtualTree::from_items("drive", "root", [file_item(), other]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| panic!("publication must not download")),
        );
        let inode = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode;
        let controller = fuse.published_version_controller();
        let open = fuse.open_file(inode, OpenFlags(libc::O_RDWR)).unwrap();
        assert_eq!(
            controller
                .try_begin_existing("drive", "file")
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        fuse.files.lock().unwrap().remove(&open);
        let opening = fuse.mark_opening(inode);
        assert_eq!(
            controller
                .try_begin_existing("drive", "file")
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(opening);

        let lease = controller.try_begin_existing("drive", "file").unwrap();
        let other_inode = fuse.lookup_node(ROOT_INODE, "other.txt").unwrap().inode;
        assert!(fuse.open_file(other_inode, OpenFlags(libc::O_RDWR)).is_ok());
        assert_eq!(
            fuse.open_file(inode, OpenFlags(libc::O_RDWR)),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            fuse.open_file(inode, OpenFlags(libc::O_RDONLY)),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            fuse.remove_local_file(ROOT_INODE, "текст.txt"),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "текст.txt",
                ROOT_INODE,
                "moved.txt",
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            controller
                .try_begin_existing("drive", "file")
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(lease);
        assert!(fuse.open_file(inode, OpenFlags(libc::O_RDWR)).is_ok());
    }

    #[test]
    fn mounted_file_reads_published_upload_without_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let cache_root = dir.path().join("cache");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            cache_root.clone(),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("published content should be cached")),
        );
        let controller = fuse.published_version_controller();
        let session = fuse.mount(&mountpoint).unwrap();
        let path = mountpoint.join("текст.txt");
        fs::write(&path, b"updated").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let lease = loop {
            match controller.try_begin_existing("drive", "file") {
                Ok(lease) => break lease,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("closed file did not become idle: {error}"),
            }
        };
        assert_eq!(
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .err()
                .unwrap()
                .raw_os_error(),
            Some(libc::EBUSY)
        );
        let store = DraftStore::open(&draft_root).unwrap();
        let draft = store.recover_item("drive", "file").unwrap().unwrap();
        let snapshot = store.snapshot(draft.identity()).unwrap().unwrap();
        let request = DownloadRequest {
            drive_id: "drive".into(),
            item_id: "file".into(),
            etag: "v2".into(),
            size: snapshot.size,
        };
        CacheStore::open(&cache_root, 20)
            .unwrap()
            .put(&request.cache_key(), snapshot.file.path(), snapshot.size)
            .unwrap();
        controller.publish_existing("file", "v2".into(), snapshot.size);
        assert!(store.acknowledge(&snapshot).unwrap());
        drop(lease);
        assert_eq!(fs::read(&path).unwrap(), b"updated");
        fs::write(&path, b"later").unwrap();
        let revised = store.recover_item("drive", "file").unwrap().unwrap();
        assert_eq!(revised.identity().base_etag, "v2");
        session.umount_and_join().unwrap();
    }

    #[test]
    fn published_new_file_keeps_inode_and_uses_remote_identity() {
        let dir = tempfile::tempdir().unwrap();
        let cache_root = dir.path().join("cache");
        let draft_root = dir.path().join("drafts");
        let tree = VirtualTree::from_items("drive", "root", []).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            cache_root.clone(),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("published file should be cached")),
        );
        let (local, handle) = fuse
            .create_file(ROOT_INODE, "created.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(local.inode, handle, 0, b"created").unwrap();
        let controller = fuse.published_version_controller();
        assert_eq!(
            controller
                .try_begin_new("drive", &local.item_id)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        fuse.files.lock().unwrap().remove(&handle);
        let lease = controller.try_begin_new("drive", &local.item_id).unwrap();
        assert_eq!(
            fuse.open_file(local.inode, OpenFlags(libc::O_RDONLY)),
            Err(Errno::EBUSY)
        );
        let store = DraftStore::open(&draft_root).unwrap();
        let draft = store
            .recover_item("drive", &local.item_id)
            .unwrap()
            .unwrap();
        let snapshot = store.snapshot(draft.identity()).unwrap().unwrap();
        let request = DownloadRequest {
            drive_id: "drive".into(),
            item_id: "remote-created".into(),
            etag: "v1".into(),
            size: snapshot.size,
        };
        CacheStore::open(&cache_root, 20)
            .unwrap()
            .put(&request.cache_key(), snapshot.file.path(), snapshot.size)
            .unwrap();
        controller
            .validate_new(&lease, &local.item_id, "remote-created", "v1")
            .unwrap();
        assert!(store.acknowledge(&snapshot).unwrap());
        controller
            .publish_new(&lease, &local.item_id, "remote-created", "v1".into(), 7)
            .unwrap();
        drop(lease);

        let published = fuse.lookup_node(ROOT_INODE, "created.txt").unwrap();
        assert_eq!(published.inode, local.inode);
        assert_eq!(published.item_id, "remote-created");
        assert_eq!(published.etag.as_deref(), Some("v1"));
        let read = fuse
            .open_file(local.inode, OpenFlags(libc::O_RDONLY))
            .unwrap();
        assert_eq!(
            fuse.read_file(local.inode, read, 0, 10).unwrap(),
            b"created"
        );
        fuse.files.lock().unwrap().remove(&read);
        let write = fuse
            .open_file(local.inode, OpenFlags(libc::O_RDWR))
            .unwrap();
        let files = fuse.files.lock().unwrap();
        let OpenHandle::Pending(identity) = &files.get(&write).unwrap().handle else {
            panic!("published file must use a remote write identity");
        };
        assert_eq!(identity.item_id, "remote-created");
        assert_eq!(identity.base_etag, "v1");
        drop(files);
        fuse.files.lock().unwrap().remove(&write);

        fuse.rename_local_file(
            ROOT_INODE,
            "created.txt",
            ROOT_INODE,
            "renamed.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "created.txt").is_none());
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "renamed.txt").unwrap().item_id,
            "remote-created"
        );
        assert!(
            fuse.directory_entries(ROOT_INODE)
                .unwrap()
                .iter()
                .any(|entry| entry.2 == "renamed.txt")
        );
        let deleted_inode = crate::stable_inode("drive", "local:second");
        fuse.local_files.lock().unwrap().insert(
            deleted_inode,
            Node {
                inode: deleted_inode,
                parent_inode: ROOT_INODE,
                item_id: "remote-second".into(),
                name: "deleted.txt".into(),
                kind: NodeKind::File,
                size: 1,
                etag: Some("v1".into()),
                ctag: None,
            },
        );
        fuse.remove_local_file(ROOT_INODE, "deleted.txt").unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "deleted.txt").is_none());
        assert!(
            MutationStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .iter()
                .any(|mutation| mutation.item_id == "remote-second")
        );
    }

    #[test]
    fn edited_new_draft_stays_local_after_upload_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let tree = VirtualTree::from_items("drive", "root", []).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("local draft should not be downloaded")),
        );
        let (local, handle) = fuse
            .create_file(ROOT_INODE, "edited.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(local.inode, handle, 0, b"first").unwrap();
        fuse.files.lock().unwrap().remove(&handle);
        let controller = fuse.published_version_controller();
        let lease = controller.try_begin_new("drive", &local.item_id).unwrap();
        let store = DraftStore::open(&draft_root).unwrap();
        let draft = store
            .recover_item("drive", &local.item_id)
            .unwrap()
            .unwrap();
        let snapshot = store.snapshot(draft.identity()).unwrap().unwrap();
        controller
            .validate_new(&lease, &local.item_id, "remote-created", "v1")
            .unwrap();
        draft.write_at(0, b"later").unwrap();
        assert!(!store.acknowledge(&snapshot).unwrap());
        drop(lease);
        let visible = fuse.lookup_node(ROOT_INODE, "edited.txt").unwrap();
        assert_eq!(visible.inode, local.inode);
        assert_eq!(visible.item_id, local.item_id);
        assert!(
            store
                .recover_item("drive", &local.item_id)
                .unwrap()
                .is_some()
        );
        let read = fuse
            .open_file(local.inode, OpenFlags(libc::O_RDONLY))
            .unwrap();
        assert_eq!(fuse.read_file(local.inode, read, 0, 5).unwrap(), b"later");
    }

    #[test]
    fn published_directories_keep_children_and_resolve_remote_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let tree = VirtualTree::from_items("drive", "root", []).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("local file should not be downloaded")),
        );
        let source = fuse.create_directory(ROOT_INODE, "source").unwrap();
        let target = fuse.create_directory(ROOT_INODE, "target").unwrap();
        let (child, handle) = fuse
            .create_file(source.inode, "child.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(child.inode, handle, 0, b"child").unwrap();
        fuse.files.lock().unwrap().remove(&handle);
        let controller = fuse.published_version_controller();
        let source_lease = controller
            .try_begin_directory("drive", &source.item_id)
            .unwrap();
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "source",
                ROOT_INODE,
                "other",
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            fuse.remove_local_directory(ROOT_INODE, "source"),
            Err(Errno::EBUSY)
        );
        let directories = DirectoryStore::open(&draft_root).unwrap();
        let entries = directories.list().unwrap();
        let created_source = directories
            .record_created(
                entries
                    .iter()
                    .find(|entry| entry.item_id == source.item_id)
                    .unwrap(),
                "remote-source",
            )
            .unwrap();
        directories
            .mark_uploaded(&created_source, "remote-source")
            .unwrap();
        controller
            .publish_directory(&source_lease, &source.item_id, "remote-source", "v1")
            .unwrap();
        drop(source_lease);
        let target_lease = controller
            .try_begin_directory("drive", &target.item_id)
            .unwrap();
        let created_target = directories
            .record_created(
                entries
                    .iter()
                    .find(|entry| entry.item_id == target.item_id)
                    .unwrap(),
                "remote-target",
            )
            .unwrap();
        directories
            .mark_uploaded(&created_target, "remote-target")
            .unwrap();
        controller
            .publish_directory(&target_lease, &target.item_id, "remote-target", "v1")
            .unwrap();
        drop(target_lease);
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "source").unwrap().inode,
            source.inode
        );
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "source").unwrap().item_id,
            "remote-source"
        );
        assert_eq!(
            fuse.lookup_node(source.inode, "child.txt").unwrap().inode,
            child.inode
        );

        let store = DraftStore::open(&draft_root).unwrap();
        let draft = store
            .recover_item("drive", &child.item_id)
            .unwrap()
            .unwrap();
        let snapshot = store.snapshot(draft.identity()).unwrap().unwrap();
        assert!(store.acknowledge(&snapshot).unwrap());
        let child_lease = controller.try_begin_new("drive", &child.item_id).unwrap();
        controller
            .publish_new(&child_lease, &child.item_id, "remote-child", "v1".into(), 5)
            .unwrap();
        drop(child_lease);
        fuse.rename_local_file(
            source.inode,
            "child.txt",
            target.inode,
            "moved.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert!(fuse.lookup_node(source.inode, "child.txt").is_none());
        assert_eq!(
            fuse.lookup_node(target.inode, "moved.txt").unwrap().inode,
            child.inode
        );
        let mutation = MutationStore::open(&draft_root)
            .unwrap()
            .list()
            .unwrap()
            .remove(0);
        assert_eq!(mutation.item_id, "remote-child");
        assert_eq!(mutation.parent_id, "remote-source");
        assert_eq!(
            mutation.kind,
            MutationKind::Rename {
                target_parent_id: "remote-target".into(),
                target_name: "moved.txt".into(),
            }
        );
    }

    #[test]
    fn directory_waiting_for_graph_verification_cannot_be_moved_or_removed() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let tree = VirtualTree::from_items("drive", "root", []).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        let folder = fuse.create_directory(ROOT_INODE, "folder").unwrap();
        let store = DirectoryStore::open(&draft_root).unwrap();
        let local = store.list().unwrap().remove(0);
        store.record_created(&local, "remote-folder").unwrap();
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "folder",
                ROOT_INODE,
                "renamed",
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            fuse.remove_local_directory(ROOT_INODE, "folder"),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "folder").unwrap().inode,
            folder.inode
        );
    }

    #[test]
    fn uploaded_directory_stays_busy_after_remount_until_catalog_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", []).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        let folder = fuse.create_directory(ROOT_INODE, "folder").unwrap();
        let store = DirectoryStore::open(&draft_root).unwrap();
        let directory = store.list().unwrap().remove(0);
        let created = store.record_created(&directory, "remote-folder").unwrap();
        store.mark_uploaded(&created, "remote-folder").unwrap();
        drop(fuse);

        let remounted = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", []).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        remounted.load_local_files().unwrap();
        assert_eq!(
            remounted.lookup_node(ROOT_INODE, "folder").unwrap().inode,
            folder.inode
        );
        assert_eq!(
            remounted.rename_local_file(
                ROOT_INODE,
                "folder",
                ROOT_INODE,
                "renamed",
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            remounted.remove_local_directory(ROOT_INODE, "folder"),
            Err(Errno::EBUSY)
        );
        assert_eq!(store.list().unwrap()[0].name, "folder");
        assert_eq!(
            store.list().unwrap()[0].remote_id.as_deref(),
            Some("remote-folder")
        );
        drop(remounted);

        let mut remote_folder = file_item();
        remote_folder.item_id = "remote-folder".into();
        remote_folder.name = Some("folder".into());
        remote_folder.kind = "folder".into();
        let confirmed = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [remote_folder]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        confirmed.load_local_files().unwrap();
        assert_eq!(
            confirmed.lookup_node(ROOT_INODE, "folder").unwrap().item_id,
            "remote-folder"
        );
        confirmed
            .remove_local_directory(ROOT_INODE, "folder")
            .unwrap();
        assert_eq!(
            MutationStore::open(&draft_root).unwrap().list().unwrap()[0].kind,
            MutationKind::Delete
        );
    }

    #[test]
    fn moving_remote_file_waits_for_local_parent_to_be_uploaded() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        let folder = fuse.create_directory(ROOT_INODE, "folder").unwrap();
        let store = DirectoryStore::open(&draft_root).unwrap();
        let move_file = || {
            fuse.rename_local_file(
                ROOT_INODE,
                "текст.txt",
                folder.inode,
                "moved.txt",
                fuser::RenameFlags::empty(),
            )
        };
        assert_eq!(move_file(), Err(Errno::EBUSY));
        let local = store.list().unwrap().remove(0);
        let created = store.record_created(&local, "remote-folder").unwrap();
        assert_eq!(move_file(), Err(Errno::EBUSY));
        assert!(fuse.lookup_node(ROOT_INODE, "текст.txt").is_some());
        assert!(
            MutationStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );

        store.mark_uploaded(&created, "remote-folder").unwrap();
        move_file().unwrap();
        assert_eq!(
            MutationStore::open(&draft_root).unwrap().list().unwrap()[0].kind,
            MutationKind::Rename {
                target_parent_id: "remote-folder".into(),
                target_name: "moved.txt".into(),
            }
        );
    }

    #[test]
    fn mounted_directory_survives_publication_without_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let tree = VirtualTree::from_items("drive", "root", []).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("local file should not be downloaded")),
        );
        let controller = fuse.published_version_controller();
        let session = fuse.mount(&mountpoint).unwrap();
        let folder = mountpoint.join("new folder");
        fs::create_dir(&folder).unwrap();
        let child = folder.join("child.txt");
        fs::write(&child, b"child").unwrap();
        let directories = DirectoryStore::open(&draft_root).unwrap();
        let local = directories.list().unwrap().remove(0);
        let lease = controller
            .try_begin_directory("drive", &local.item_id)
            .unwrap();
        let created = directories.record_created(&local, "remote-folder").unwrap();
        directories
            .mark_uploaded(&created, "remote-folder")
            .unwrap();
        controller
            .publish_directory(&lease, &local.item_id, "remote-folder", "v1")
            .unwrap();
        drop(lease);
        assert_eq!(fs::read(&child).unwrap(), b"child");
        assert!(
            fs::read_dir(&folder)
                .unwrap()
                .any(|entry| { entry.unwrap().file_name() == "child.txt" })
        );
        fs::write(folder.join("another.txt"), b"another").unwrap();
        let drafts = DraftStore::open(&draft_root).unwrap().list().unwrap();
        assert!(drafts.iter().any(|draft| {
            draft.new_file.as_ref().is_some_and(|location| {
                location.name == "another.txt" && location.parent_id == "remote-folder"
            })
        }));
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_new_file_survives_publication_without_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let cache_root = dir.path().join("cache");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let tree = VirtualTree::from_items("drive", "root", []).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            cache_root.clone(),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("published file should be cached")),
        );
        let controller = fuse.published_version_controller();
        let session = fuse.mount(&mountpoint).unwrap();
        let path = mountpoint.join("created.txt");
        fs::write(&path, b"created").unwrap();
        let store = DraftStore::open(&draft_root).unwrap();
        let local_id = store.list().unwrap()[0].item_id.clone();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let lease = loop {
            match controller.try_begin_new("drive", &local_id) {
                Ok(lease) => break lease,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("closed new file did not become idle: {error}"),
            }
        };
        let draft = store.recover_item("drive", &local_id).unwrap().unwrap();
        let snapshot = store.snapshot(draft.identity()).unwrap().unwrap();
        let request = DownloadRequest {
            drive_id: "drive".into(),
            item_id: "remote-created".into(),
            etag: "v1".into(),
            size: snapshot.size,
        };
        CacheStore::open(&cache_root, 20)
            .unwrap()
            .put(&request.cache_key(), snapshot.file.path(), snapshot.size)
            .unwrap();
        assert!(store.acknowledge(&snapshot).unwrap());
        controller
            .publish_new(&lease, &local_id, "remote-created", "v1".into(), 7)
            .unwrap();
        drop(lease);
        assert_eq!(fs::read(&path).unwrap(), b"created");
        fs::write(&path, b"later").unwrap();
        let revised = store
            .recover_item("drive", "remote-created")
            .unwrap()
            .unwrap();
        assert_eq!(revised.identity().base_etag, "v1");
        session.umount_and_join().unwrap();
    }

    #[test]
    fn seek_data_and_hole_use_draft_or_catalog_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| panic!("seek should not download remote content")),
        );
        let remote = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        let remote_handle = fuse
            .open_file(remote.inode, OpenFlags(libc::O_RDWR))
            .unwrap();
        assert_eq!(
            fuse.seek_file(remote.inode, remote_handle, 2, libc::SEEK_DATA),
            Ok(2)
        );
        assert_eq!(
            fuse.seek_file(remote.inode, remote_handle, 2, libc::SEEK_HOLE),
            Ok(5)
        );
        assert_eq!(
            fuse.seek_file(remote.inode, remote_handle, 5, libc::SEEK_DATA),
            Err(Errno::ENXIO)
        );
        let (local, local_handle) = fuse
            .create_file(ROOT_INODE, "sparse.bin", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.truncate_file(local.inode, Some(local_handle), 8192)
            .unwrap();
        fuse.write_file(local.inode, local_handle, 4096, b"x")
            .unwrap();
        assert_eq!(
            fuse.seek_file(local.inode, local_handle, 4096, libc::SEEK_DATA),
            Ok(4096)
        );
        assert!(
            fuse.seek_file(local.inode, local_handle, 0, libc::SEEK_HOLE)
                .unwrap()
                <= 8192
        );
        assert_eq!(
            fuse.seek_file(local.inode, local_handle, 8192, libc::SEEK_HOLE),
            Err(Errno::ENXIO)
        );
        assert_eq!(
            fuse.seek_file(local.inode, local_handle, -1, libc::SEEK_DATA),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            fuse.seek_file(local.inode, 999, 0, libc::SEEK_DATA),
            Err(Errno::EBADF)
        );
    }

    #[test]
    fn mounted_seek_data_and_hole_survive_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        fs::create_dir(&mountpoint).unwrap();
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
                dir.path().join("cache"),
                dir.path().join("drafts"),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let session = mount();
        let path = mountpoint.join("sparse.bin");
        fs::write(&path, b"abc").unwrap();
        let file = fs::File::open(&path).unwrap();
        assert_eq!(
            unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_DATA) },
            0
        );
        assert_eq!(
            unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_HOLE) },
            3
        );
        assert_eq!(
            unsafe { libc::lseek(file.as_raw_fd(), 3, libc::SEEK_DATA) },
            -1
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ENXIO));
        drop(file);
        session.umount_and_join().unwrap();
        let session = mount();
        let file = fs::File::open(&path).unwrap();
        assert_eq!(
            unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_HOLE) },
            3
        );
        drop(file);
        session.umount_and_join().unwrap();
    }

    #[test]
    fn copy_range_writes_local_draft_and_checks_handles() {
        let dir = tempfile::tempdir().unwrap();
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let (source, source_handle) = fuse
            .create_file(ROOT_INODE, "source.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(source.inode, source_handle, 0, b"abcdef")
            .unwrap();
        let (target, target_handle) = fuse
            .create_file(ROOT_INODE, "target.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        assert_eq!(
            fuse.copy_range(
                source.inode,
                source_handle,
                2,
                target.inode,
                target_handle,
                3,
                10
            )
            .unwrap(),
            4
        );
        assert_eq!(
            fuse.read_file(target.inode, target_handle, 0, 10).unwrap(),
            b"\0\0\0cdef"
        );
        assert_eq!(
            fuse.copy_range(
                source.inode,
                source_handle,
                0,
                target.inode,
                target_handle,
                0,
                0
            )
            .unwrap(),
            0
        );
        assert_eq!(
            fuse.copy_range(
                source.inode,
                source_handle,
                0,
                source.inode,
                source_handle,
                0,
                1
            ),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            fuse.copy_range(source.inode, source_handle, 0, target.inode, 999, 0, 1),
            Err(Errno::EBADF)
        );
        let readonly = fuse
            .open_file(target.inode, OpenFlags(libc::O_RDONLY))
            .unwrap();
        assert_eq!(
            fuse.copy_range(source.inode, source_handle, 0, target.inode, readonly, 0, 1),
            Err(Errno::EBADF)
        );
        let append_target = fuse
            .open_file(target.inode, OpenFlags(libc::O_WRONLY | libc::O_APPEND))
            .unwrap();
        assert_eq!(
            fuse.copy_range(
                source.inode,
                source_handle,
                0,
                target.inode,
                append_target,
                0,
                1
            ),
            Err(Errno::EBADF)
        );
        let write_only_source = fuse
            .open_file(source.inode, OpenFlags(libc::O_WRONLY))
            .unwrap();
        assert_eq!(
            fuse.copy_range(
                source.inode,
                write_only_source,
                0,
                target.inode,
                target_handle,
                0,
                1
            ),
            Err(Errno::EBADF)
        );
        assert_eq!(
            fuse.read_file(target.inode, target_handle, 0, 10).unwrap(),
            b"\0\0\0cdef"
        );
        assert_eq!(
            fuse.copy_range(
                source.inode,
                source_handle,
                0,
                target.inode,
                target_handle,
                i64::MAX as u64 + 1,
                1
            ),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn allocate_file_rejects_invalid_requests_without_modifying_draft() {
        let dir = tempfile::tempdir().unwrap();
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let (node, handle) = fuse
            .create_file(ROOT_INODE, "new.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        assert_eq!(
            fuse.allocate_file(node.inode, handle, 0, 1, libc::FALLOC_FL_PUNCH_HOLE),
            Err(Errno::EOPNOTSUPP)
        );
        assert_eq!(
            fuse.allocate_file(node.inode, handle, 0, 0, 0),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            fuse.allocate_file(node.inode, handle, i64::MAX as u64, 2, 0),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            fuse.allocate_file(node.inode, 999, 0, 1, 0),
            Err(Errno::EBADF)
        );
        let readonly = fuse
            .open_file(node.inode, OpenFlags(libc::O_RDONLY))
            .unwrap();
        assert_eq!(
            fuse.allocate_file(node.inode, readonly, 0, 1, 0),
            Err(Errno::EBADF)
        );
        assert_eq!(fuse.read_file(node.inode, handle, 0, 1).unwrap(), b"");
        let remote_inode = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode;
        let remote_handle = fuse
            .open_file(remote_inode, OpenFlags(libc::O_RDWR))
            .unwrap();
        assert_eq!(
            fuse.allocate_file(
                remote_inode,
                remote_handle,
                0,
                1,
                libc::FALLOC_FL_PUNCH_HOLE
            ),
            Err(Errno::EOPNOTSUPP)
        );
        assert!(
            DraftStore::open(&dir.path().join("drafts"))
                .unwrap()
                .recover_item("drive", "file")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn allocate_remote_file_preserves_cached_content_in_draft() {
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
        let inode = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode;
        let handle = fuse.open_file(inode, OpenFlags(libc::O_RDWR)).unwrap();
        fuse.allocate_file(inode, handle, 8, 8, 0).unwrap();
        assert_eq!(
            fuse.read_file(inode, handle, 0, 16).unwrap(),
            b"hello\0\0\0\0\0\0\0\0\0\0\0"
        );
        let draft = DraftStore::open(&draft_root)
            .unwrap()
            .recover_item("drive", "file")
            .unwrap()
            .unwrap();
        assert_eq!(draft.identity().base_etag, "v1");
        assert_eq!(draft.len().unwrap(), 16);
        assert_eq!(fs::read(source).unwrap(), b"hello");
    }

    #[test]
    fn copy_range_from_cached_remote_file_creates_local_draft() {
        let dir = tempfile::tempdir().unwrap();
        let cache_root = dir.path().join("cache");
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
            dir.path().join("drafts"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let source_inode = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode;
        let source_handle = fuse
            .open_file(source_inode, OpenFlags(libc::O_RDONLY))
            .unwrap();
        let (target, target_handle) = fuse
            .create_file(ROOT_INODE, "copy.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        assert_eq!(
            fuse.copy_range(
                source_inode,
                source_handle,
                0,
                target.inode,
                target_handle,
                0,
                5
            )
            .unwrap(),
            5
        );
        assert_eq!(
            fuse.read_file(target.inode, target_handle, 0, 5).unwrap(),
            b"hello"
        );
        assert_eq!(fs::read(source).unwrap(), b"hello");
    }

    #[test]
    fn mounted_copy_file_range_survives_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let session = mount();
        fs::write(mountpoint.join("source.txt"), b"abcdef").unwrap();
        let source = fs::File::open(mountpoint.join("source.txt")).unwrap();
        let target = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(mountpoint.join("target.txt"))
            .unwrap();
        let mut source_offset = 1_i64;
        let mut target_offset = 0_i64;
        let copied = unsafe {
            libc::copy_file_range(
                source.as_raw_fd(),
                &mut source_offset,
                target.as_raw_fd(),
                &mut target_offset,
                10,
                0,
            )
        };
        assert_eq!(copied, 5, "{}", io::Error::last_os_error());
        assert_eq!((source_offset, target_offset), (6, 5));
        drop((source, target));
        assert_eq!(fs::read(mountpoint.join("target.txt")).unwrap(), b"bcdef");
        session.umount_and_join().unwrap();
        let session = mount();
        assert_eq!(fs::read(mountpoint.join("target.txt")).unwrap(), b"bcdef");
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_fallocate_preserves_data_and_size_after_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        fs::create_dir(&mountpoint).unwrap();
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
                dir.path().join("cache"),
                dir.path().join("drafts"),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let session = mount();
        let path = mountpoint.join("preallocated.txt");
        fs::write(&path, b"abc").unwrap();
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert_eq!(
            unsafe { libc::fallocate(file.as_raw_fd(), libc::FALLOC_FL_KEEP_SIZE, 8, 8) },
            0,
            "{}",
            io::Error::last_os_error()
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), 3);
        assert_eq!(
            unsafe { libc::fallocate(file.as_raw_fd(), 0, 8, 8) },
            0,
            "{}",
            io::Error::last_os_error()
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), 16);
        drop(file);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..3], b"abc");
        assert_eq!(&bytes[3..], &[0; 13]);
        session.umount_and_join().unwrap();
        let session = mount();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        session.umount_and_join().unwrap();
    }

    #[test]
    fn new_file_can_be_renamed_and_removed_without_leaving_draft() {
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
        let (node, handle) = fuse
            .create_file(ROOT_INODE, "first.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(node.inode, handle, 0, b"local").unwrap();
        fuse.rename_local_file(
            ROOT_INODE,
            "first.txt",
            ROOT_INODE,
            "second.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "first.txt").is_none());
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "second.txt").unwrap().inode,
            node.inode
        );
        fuse.close_file(node.inode, handle);
        drop(fuse);

        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        fuse.load_local_files().unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "first.txt").is_none());
        assert!(fuse.lookup_node(ROOT_INODE, "second.txt").is_some());
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "second.txt",
                ROOT_INODE,
                "текст.txt",
                fuser::RenameFlags::from_bits_truncate(libc::RENAME_NOREPLACE),
            ),
            Err(Errno::EEXIST)
        );
        fuse.remove_local_file(ROOT_INODE, "second.txt").unwrap();
        assert!(
            DraftStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
        fuse.remove_local_file(ROOT_INODE, "текст.txt").unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "текст.txt").is_none());
        assert_eq!(
            MutationStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn unlinked_open_local_file_stays_usable_until_last_close() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let make_fuse = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", []).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
        };
        let fuse = make_fuse();
        let (old, writer) = fuse
            .create_file(ROOT_INODE, "note.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(old.inode, writer, 0, b"old").unwrap();
        let reader = fuse
            .open_file(old.inode, OpenFlags(libc::O_RDONLY))
            .unwrap();

        fuse.remove_local_file(ROOT_INODE, "note.txt").unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "note.txt").is_none());
        assert_eq!(fuse.directory_entries(ROOT_INODE).unwrap().len(), 2);
        assert!(
            DraftStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
        assert_eq!(fuse.attr(&fuse.node(old.inode).unwrap()).unwrap().nlink, 0);
        fuse.write_file(old.inode, writer, 3, b"!").unwrap();
        fuse.truncate_file(old.inode, Some(writer), 3).unwrap();
        fuse.sync_file(old.inode, writer).unwrap();
        assert_eq!(fuse.attr(&fuse.node(old.inode).unwrap()).unwrap().size, 3);
        assert_eq!(fuse.seek_file(old.inode, writer, 0, libc::SEEK_HOLE), Ok(3));
        assert_eq!(fuse.read_file(old.inode, reader, 0, 4).unwrap(), b"old");

        let (replacement, replacement_handle) = fuse
            .create_file(ROOT_INODE, "note.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(replacement.inode, replacement_handle, 0, b"new")
            .unwrap();
        assert_ne!(old.inode, replacement.inode);
        fuse.close_file(old.inode, writer);
        assert_eq!(fuse.read_file(old.inode, reader, 0, 4).unwrap(), b"old");
        fuse.close_file(old.inode, reader);
        assert!(fuse.node(old.inode).is_none());
        assert!(fuse.detached_files.lock().unwrap().is_empty());
        fuse.close_file(replacement.inode, replacement_handle);
        drop(fuse);

        let fuse = make_fuse();
        fuse.load_local_files().unwrap();
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "note.txt").unwrap().inode,
            replacement.inode
        );
        assert_eq!(
            DraftStore::open(&draft_root).unwrap().list().unwrap().len(),
            1
        );
    }

    #[test]
    fn editor_style_rename_replaces_remote_file_with_durable_draft() {
        let dir = tempfile::tempdir().unwrap();
        let cache_root = dir.path().join("cache");
        let draft_root = dir.path().join("drafts");
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            cache_root.clone(),
            draft_root.clone(),
            100,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let (temporary, handle) = fuse
            .create_file(ROOT_INODE, "swap.tmp", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(temporary.inode, handle, 0, b"replacement")
            .unwrap();
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "swap.tmp",
                ROOT_INODE,
                "текст.txt",
                fuser::RenameFlags::from_bits_truncate(libc::RENAME_NOREPLACE),
            ),
            Err(Errno::EEXIST)
        );
        fuse.rename_local_file(
            ROOT_INODE,
            "swap.tmp",
            ROOT_INODE,
            "текст.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert_eq!(fuse.sync_file(temporary.inode, handle), Ok(()));
        assert_eq!(
            fuse.read_file(temporary.inode, handle, 0, 11).unwrap(),
            b"replacement"
        );
        let controller = fuse.published_version_controller();
        assert_eq!(
            controller
                .try_begin_existing("drive", "file")
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        fuse.write_file(temporary.inode, handle, 11, b"!").unwrap();
        fuse.close_file(temporary.inode, handle);
        let lease = controller.try_begin_existing("drive", "file").unwrap();
        assert_eq!(
            fuse.open_file(temporary.inode, OpenFlags(libc::O_RDONLY)),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            fuse.remove_local_file(ROOT_INODE, "текст.txt"),
            Err(Errno::EBUSY)
        );
        assert!(fuse.lookup_node(ROOT_INODE, "swap.tmp").is_none());
        let drafts = DraftStore::open(&draft_root).unwrap().list().unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            (drafts[0].item_id.as_str(), drafts[0].base_etag.as_str()),
            ("file", "v1")
        );
        let draft = DraftStore::open(&draft_root)
            .unwrap()
            .recover(&drafts[0])
            .unwrap()
            .unwrap();
        let mut bytes = [0; 12];
        assert_eq!(draft.read_at(0, &mut bytes).unwrap(), 12);
        assert_eq!(&bytes, b"replacement!");
        let store = DraftStore::open(&draft_root).unwrap();
        let snapshot = store.snapshot(draft.identity()).unwrap().unwrap();
        let request = DownloadRequest {
            drive_id: "drive".into(),
            item_id: "file".into(),
            etag: "v2".into(),
            size: snapshot.size,
        };
        CacheStore::open(&cache_root, 100)
            .unwrap()
            .put(&request.cache_key(), snapshot.file.path(), snapshot.size)
            .unwrap();
        controller.publish_existing("file", "v2".into(), snapshot.size);
        assert!(store.acknowledge(&snapshot).unwrap());
        drop(lease);
        let replacement = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        assert_eq!(replacement.inode, temporary.inode);
        assert_eq!(replacement.etag.as_deref(), Some("v2"));
        let reopened = fuse
            .open_file(replacement.inode, OpenFlags(libc::O_RDONLY))
            .unwrap();
        assert_eq!(
            fuse.read_file(replacement.inode, reopened, 0, 12).unwrap(),
            b"replacement!"
        );
        fuse.close_file(replacement.inode, reopened);
        let writable = fuse
            .open_file(replacement.inode, OpenFlags(libc::O_RDWR))
            .unwrap();
        let files = fuse.files.lock().unwrap();
        let OpenHandle::Pending(identity) = &files.get(&writable).unwrap().handle else {
            panic!("expected the published version for a new edit");
        };
        assert_eq!(identity.base_etag, "v2");
        drop(files);
        fuse.close_file(replacement.inode, writable);
        let (second, second_handle) = fuse
            .create_file(ROOT_INODE, "second.tmp", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(second.inode, second_handle, 0, b"second")
            .unwrap();
        fuse.close_file(second.inode, second_handle);
        fuse.rename_local_file(
            ROOT_INODE,
            "second.tmp",
            ROOT_INODE,
            "текст.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        let second_draft = store.recover_item("drive", "file").unwrap().unwrap();
        assert_eq!(second_draft.identity().base_etag, "v2");
    }

    #[test]
    fn deleting_published_editor_replacement_hides_remote_item() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        let (temporary, handle) = fuse
            .create_file(ROOT_INODE, "swap.tmp", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(temporary.inode, handle, 0, b"replacement")
            .unwrap();
        fuse.close_file(temporary.inode, handle);
        fuse.rename_local_file(
            ROOT_INODE,
            "swap.tmp",
            ROOT_INODE,
            "текст.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        let store = DraftStore::open(&draft_root).unwrap();
        let draft = store.recover_item("drive", "file").unwrap().unwrap();
        let snapshot = store.snapshot(draft.identity()).unwrap().unwrap();
        let controller = fuse.published_version_controller();
        let lease = controller.try_begin_existing("drive", "file").unwrap();
        controller.publish_existing("file", "v2".into(), snapshot.size);
        assert!(store.acknowledge(&snapshot).unwrap());
        drop(lease);

        fuse.remove_local_file(ROOT_INODE, "текст.txt").unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "текст.txt").is_none());
        assert!(
            !fuse
                .directory_entries(ROOT_INODE)
                .unwrap()
                .iter()
                .any(|entry| entry.2 == "текст.txt")
        );
        let changes = MutationStore::open(&draft_root).unwrap().list().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, MutationKind::Delete);
        assert_eq!(changes[0].etag, "v2");
        drop(fuse);
        let remounted = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root,
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        remounted.load_local_files().unwrap();
        assert!(remounted.lookup_node(ROOT_INODE, "текст.txt").is_none());
    }

    #[test]
    fn renaming_published_editor_replacement_tracks_remote_item() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        let (temporary, handle) = fuse
            .create_file(ROOT_INODE, "swap.tmp", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(temporary.inode, handle, 0, b"replacement")
            .unwrap();
        fuse.close_file(temporary.inode, handle);
        fuse.rename_local_file(
            ROOT_INODE,
            "swap.tmp",
            ROOT_INODE,
            "текст.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        let store = DraftStore::open(&draft_root).unwrap();
        let draft = store.recover_item("drive", "file").unwrap().unwrap();
        let snapshot = store.snapshot(draft.identity()).unwrap().unwrap();
        let controller = fuse.published_version_controller();
        let lease = controller.try_begin_existing("drive", "file").unwrap();
        controller.publish_existing("file", "v2".into(), snapshot.size);
        assert!(store.acknowledge(&snapshot).unwrap());
        drop(lease);

        fuse.rename_local_file(
            ROOT_INODE,
            "текст.txt",
            ROOT_INODE,
            "renamed.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "текст.txt").is_none());
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "renamed.txt").unwrap().inode,
            temporary.inode
        );
        let entries = fuse.directory_entries(ROOT_INODE).unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.2 == "renamed.txt")
                .count(),
            1
        );
        let changes = MutationStore::open(&draft_root).unwrap().list().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].etag, "v2");
        assert!(matches!(
            &changes[0].kind,
            MutationKind::Rename { target_name, .. } if target_name == "renamed.txt"
        ));
        drop(fuse);
        let remounted = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root,
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        remounted.load_local_files().unwrap();
        assert!(remounted.lookup_node(ROOT_INODE, "текст.txt").is_none());
        assert_eq!(
            remounted
                .lookup_node(ROOT_INODE, "renamed.txt")
                .unwrap()
                .item_id,
            "file"
        );
    }

    #[test]
    fn editor_rename_waits_for_open_remote_target_before_replacing_its_draft() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(|_| panic!("no download expected")),
        );
        let target = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        let target_handle = fuse
            .open_file(target.inode, OpenFlags(libc::O_RDWR | libc::O_TRUNC))
            .unwrap();
        fuse.write_file(target.inode, target_handle, 0, b"old")
            .unwrap();
        let (temporary, source_handle) = fuse
            .create_file(ROOT_INODE, "swap.tmp", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(temporary.inode, source_handle, 0, b"new")
            .unwrap();

        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "swap.tmp",
                ROOT_INODE,
                "текст.txt",
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EBUSY)
        );
        fuse.write_file(target.inode, target_handle, 3, b"!")
            .unwrap();
        assert_eq!(
            fuse.read_file(target.inode, target_handle, 0, 4).unwrap(),
            b"old!"
        );
        assert!(fuse.lookup_node(ROOT_INODE, "swap.tmp").is_some());
        assert_eq!(
            DraftStore::open(&draft_root).unwrap().list().unwrap().len(),
            2
        );

        fuse.files.lock().unwrap().remove(&target_handle);
        fuse.rename_local_file(
            ROOT_INODE,
            "swap.tmp",
            ROOT_INODE,
            "текст.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "swap.tmp").is_none());
        assert_eq!(
            fuse.read_file(temporary.inode, source_handle, 0, 3)
                .unwrap(),
            b"new"
        );
        assert_eq!(
            DraftStore::open(&draft_root).unwrap().list().unwrap().len(),
            1
        );
    }

    #[test]
    fn repeated_editor_replace_waits_for_open_local_alias() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        let (first, first_handle) = fuse
            .create_file(ROOT_INODE, "first.tmp", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(first.inode, first_handle, 0, b"first")
            .unwrap();
        fuse.rename_local_file(
            ROOT_INODE,
            "first.tmp",
            ROOT_INODE,
            "текст.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        let (second, second_handle) = fuse
            .create_file(ROOT_INODE, "second.tmp", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(second.inode, second_handle, 0, b"second")
            .unwrap();
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "second.tmp",
                ROOT_INODE,
                "текст.txt",
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EBUSY)
        );
        fuse.write_file(first.inode, first_handle, 5, b"!").unwrap();
        assert_eq!(
            fuse.read_file(first.inode, first_handle, 0, 6).unwrap(),
            b"first!"
        );
        fuse.close_file(first.inode, first_handle);
        fuse.rename_local_file(
            ROOT_INODE,
            "second.tmp",
            ROOT_INODE,
            "текст.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "first.tmp").is_none());
        assert!(fuse.lookup_node(ROOT_INODE, "second.tmp").is_none());
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode,
            second.inode
        );
        assert_eq!(
            fuse.read_file(second.inode, second_handle, 0, 6).unwrap(),
            b"second"
        );
        fuse.close_file(second.inode, second_handle);
        let store = DraftStore::open(&draft_root).unwrap();
        let drafts = store.list().unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].item_id, "file");
        let draft = store.recover(&drafts[0]).unwrap().unwrap();
        let mut bytes = [0; 6];
        assert_eq!(draft.read_at(0, &mut bytes).unwrap(), 6);
        assert_eq!(&bytes, b"second");
    }

    #[test]
    fn remote_mutation_waits_for_writable_original_inode_after_editor_replace() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        let original_inode = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode;
        let (temporary, handle) = fuse
            .create_file(ROOT_INODE, "swap.tmp", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(temporary.inode, handle, 0, b"replacement")
            .unwrap();
        fuse.close_file(temporary.inode, handle);
        fuse.rename_local_file(
            ROOT_INODE,
            "swap.tmp",
            ROOT_INODE,
            "текст.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        let store = DraftStore::open(&draft_root).unwrap();
        let draft = store.recover_item("drive", "file").unwrap().unwrap();
        let snapshot = store.snapshot(draft.identity()).unwrap().unwrap();
        let controller = fuse.published_version_controller();
        let lease = controller.try_begin_existing("drive", "file").unwrap();
        controller.publish_existing("file", "v2".into(), snapshot.size);
        assert!(store.acknowledge(&snapshot).unwrap());
        drop(lease);

        let stale = fuse
            .open_file(original_inode, OpenFlags(libc::O_RDWR))
            .unwrap();
        assert_eq!(
            fuse.remove_local_file(ROOT_INODE, "текст.txt"),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "текст.txt",
                ROOT_INODE,
                "renamed.txt",
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EBUSY)
        );
        assert!(
            MutationStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
        fuse.close_file(original_inode, stale);
        fuse.remove_local_file(ROOT_INODE, "текст.txt").unwrap();
    }

    #[test]
    fn copy_range_rejects_two_inodes_of_the_same_remote_file() {
        let dir = tempfile::tempdir().unwrap();
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            dir.path().join("drafts"),
            20,
            Arc::new(|_| panic!("no download expected")),
        );
        let remote_inode = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode;
        let (local, handle) = fuse
            .create_file(ROOT_INODE, "swap.tmp", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(local.inode, handle, 0, b"content").unwrap();
        fuse.close_file(local.inode, handle);
        fuse.rename_local_file(
            ROOT_INODE,
            "swap.tmp",
            ROOT_INODE,
            "текст.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        let source = fuse
            .open_file(local.inode, OpenFlags(libc::O_RDONLY))
            .unwrap();
        let target = fuse
            .open_file(remote_inode, OpenFlags(libc::O_RDWR))
            .unwrap();
        assert_eq!(
            fuse.copy_range(local.inode, source, 0, remote_inode, target, 1, 5),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            fuse.read_file(local.inode, source, 0, 7).unwrap(),
            b"content"
        );
        fuse.close_file(local.inode, source);
        fuse.close_file(remote_inode, target);
    }

    #[test]
    fn resized_editor_replacement_survives_remount() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let make_fuse = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| panic!("no download expected")),
            )
        };
        let fuse = make_fuse();
        let (temporary, handle) = fuse
            .create_file(ROOT_INODE, "swap.tmp", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(temporary.inode, handle, 0, b"replacement")
            .unwrap();
        fuse.rename_local_file(
            ROOT_INODE,
            "swap.tmp",
            ROOT_INODE,
            "текст.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        fuse.truncate_file(temporary.inode, Some(handle), 4)
            .unwrap();
        fuse.allocate_file(temporary.inode, handle, 8, 2, 0)
            .unwrap();
        assert_eq!(
            fuse.read_file(temporary.inode, handle, 0, 10).unwrap(),
            b"repl\0\0\0\0\0\0"
        );
        fuse.files.lock().unwrap().remove(&handle);
        drop(fuse);

        let fuse = make_fuse();
        fuse.load_local_files().unwrap();
        let saved = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        let handle = fuse
            .open_file(saved.inode, OpenFlags(libc::O_RDONLY))
            .unwrap();
        assert_eq!(
            fuse.read_file(saved.inode, handle, 0, 10).unwrap(),
            b"repl\0\0\0\0\0\0"
        );
    }

    #[test]
    fn local_file_rename_replaces_local_file_and_survives_remount() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let make_fuse = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", []).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
        };
        let fuse = make_fuse();
        let (source, source_handle) = fuse
            .create_file(ROOT_INODE, "source.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(source.inode, source_handle, 0, b"new")
            .unwrap();
        let (target, target_handle) = fuse
            .create_file(ROOT_INODE, "target.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(target.inode, target_handle, 0, b"old")
            .unwrap();
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "source.txt",
                ROOT_INODE,
                "target.txt",
                fuser::RenameFlags::from_bits_truncate(libc::RENAME_NOREPLACE),
            ),
            Err(Errno::EEXIST)
        );
        fuse.rename_local_file(
            ROOT_INODE,
            "source.txt",
            ROOT_INODE,
            "target.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "source.txt").is_none());
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "target.txt").unwrap().inode,
            source.inode
        );
        assert_eq!(
            fuse.attr(&fuse.node(target.inode).unwrap()).unwrap().nlink,
            0
        );
        assert_eq!(
            fuse.read_file(target.inode, target_handle, 0, 3).unwrap(),
            b"old"
        );
        fuse.write_file(target.inode, target_handle, 3, b"!")
            .unwrap();
        assert_eq!(
            fuse.read_file(target.inode, target_handle, 0, 4).unwrap(),
            b"old!"
        );
        fuse.write_file(source.inode, source_handle, 3, b"!")
            .unwrap();
        assert_eq!(
            fuse.read_file(source.inode, source_handle, 0, 4).unwrap(),
            b"new!"
        );
        fuse.close_file(source.inode, source_handle);
        fuse.close_file(target.inode, target_handle);
        drop(fuse);
        let fuse = make_fuse();
        fuse.load_local_files().unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "source.txt").is_none());
        let node = fuse.lookup_node(ROOT_INODE, "target.txt").unwrap();
        assert_eq!(node.item_id, source.item_id);
        let drafts = DraftStore::open(&draft_root).unwrap().list().unwrap();
        assert_eq!(drafts.len(), 1);
        let draft = DraftStore::open(&draft_root)
            .unwrap()
            .recover(&drafts[0])
            .unwrap()
            .unwrap();
        let mut bytes = [0; 4];
        assert_eq!(draft.read_at(0, &mut bytes).unwrap(), 4);
        assert_eq!(&bytes, b"new!");
    }

    #[test]
    fn local_directory_replaces_only_empty_local_directory() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let make_fuse = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", []).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
        };
        let fuse = make_fuse();
        let source = fuse.create_directory(ROOT_INODE, "source").unwrap();
        let target = fuse.create_directory(ROOT_INODE, "target").unwrap();
        fuse.create_directory(target.inode, "child").unwrap();
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "source",
                ROOT_INODE,
                "target",
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::ENOTEMPTY)
        );
        fuse.remove_local_directory(target.inode, "child").unwrap();
        assert_eq!(fuse.lookup_node(target.inode, "child"), None);
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "source",
                ROOT_INODE,
                "target",
                fuser::RenameFlags::from_bits_truncate(libc::RENAME_NOREPLACE),
            ),
            Err(Errno::EEXIST)
        );
        fuse.rename_local_file(
            ROOT_INODE,
            "source",
            ROOT_INODE,
            "target",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "source").is_none());
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "target").unwrap().inode,
            source.inode
        );
        drop(fuse);
        let fuse = make_fuse();
        fuse.load_local_files().unwrap();
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "target").unwrap().item_id,
            source.item_id
        );
        assert_eq!(
            DirectoryStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn remote_rename_and_delete_remain_visible_in_overlay_after_remount() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let mut second = file_item();
        second.item_id = "second".into();
        second.name = Some("remove.txt".into());
        let make_fuse = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item(), second.clone()]).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
        };
        let fuse = make_fuse();
        fuse.rename_local_file(
            ROOT_INODE,
            "текст.txt",
            ROOT_INODE,
            "moved.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        fuse.remove_local_file(ROOT_INODE, "remove.txt").unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "текст.txt").is_none());
        assert!(fuse.lookup_node(ROOT_INODE, "moved.txt").is_some());
        assert!(fuse.lookup_node(ROOT_INODE, "remove.txt").is_none());
        assert_eq!(fuse.directory_entries(ROOT_INODE).unwrap().len(), 3);
        drop(fuse);
        let fuse = make_fuse();
        fuse.load_local_files().unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "moved.txt").is_some());
        assert!(fuse.lookup_node(ROOT_INODE, "remove.txt").is_none());
        assert_eq!(
            MutationStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn old_remote_names_cannot_be_mutated_after_queued_rename() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let mut folder = file_item();
        folder.item_id = "folder".into();
        folder.name = Some("old-folder".into());
        folder.kind = "folder".into();
        let make_fuse = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item(), folder.clone()]).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
        };
        let fuse = make_fuse();
        fuse.rename_local_file(
            ROOT_INODE,
            "текст.txt",
            ROOT_INODE,
            "new.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        fuse.rename_local_file(
            ROOT_INODE,
            "old-folder",
            ROOT_INODE,
            "new-folder",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        for fuse in [Some(fuse), None] {
            let fuse = match fuse {
                Some(fuse) => fuse,
                None => {
                    let fuse = make_fuse();
                    fuse.load_local_files().unwrap();
                    fuse
                }
            };
            assert_eq!(
                fuse.remove_local_file(ROOT_INODE, "текст.txt"),
                Err(Errno::ENOENT)
            );
            assert_eq!(
                fuse.remove_local_directory(ROOT_INODE, "old-folder"),
                Err(Errno::ENOENT)
            );
            assert_eq!(
                fuse.rename_local_file(
                    ROOT_INODE,
                    "текст.txt",
                    ROOT_INODE,
                    "another.txt",
                    fuser::RenameFlags::empty(),
                ),
                Err(Errno::ENOENT)
            );
            assert_eq!(
                fuse.remove_local_file(ROOT_INODE, "new.txt"),
                Err(Errno::EBUSY)
            );
            assert_eq!(
                fuse.remove_local_directory(ROOT_INODE, "new-folder"),
                Err(Errno::EBUSY)
            );
        }
        assert_eq!(
            MutationStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn remote_mutations_wait_for_open_writable_file_before_draft_exists() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(|_| panic!("no download expected")),
        );
        let node = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        let handle = fuse.open_file(node.inode, OpenFlags(libc::O_RDWR)).unwrap();
        assert!(
            DraftStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fuse.remove_local_file(ROOT_INODE, "текст.txt"),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "текст.txt",
                ROOT_INODE,
                "moved.txt",
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EBUSY)
        );
        assert!(
            MutationStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );

        fuse.truncate_file(node.inode, Some(handle), 0).unwrap();
        fuse.write_file(node.inode, handle, 0, b"local").unwrap();
        fuse.files.lock().unwrap().remove(&handle);
        assert_eq!(
            fuse.remove_local_file(ROOT_INODE, "текст.txt"),
            Err(Errno::EBUSY)
        );
        assert!(
            MutationStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
        let draft = DraftStore::open(&draft_root)
            .unwrap()
            .recover_item("drive", "file")
            .unwrap()
            .unwrap();
        let mut bytes = [0; 5];
        assert_eq!(draft.read_at(0, &mut bytes).unwrap(), 5);
        assert_eq!(&bytes, b"local");
    }

    #[test]
    fn remote_delete_proceeds_after_unused_write_handle_closes() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(|_| panic!("no download expected")),
        );
        let node = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        let handle = fuse
            .open_file(node.inode, OpenFlags(libc::O_WRONLY))
            .unwrap();
        assert_eq!(
            fuse.remove_local_file(ROOT_INODE, "текст.txt"),
            Err(Errno::EBUSY)
        );
        fuse.files.lock().unwrap().remove(&handle);
        fuse.remove_local_file(ROOT_INODE, "текст.txt").unwrap();
        assert_eq!(
            MutationStore::open(&draft_root).unwrap().list().unwrap()[0].kind,
            MutationKind::Delete
        );
    }

    #[test]
    fn uploaded_directory_mapping_keeps_nested_draft_visible() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        DraftStore::open(&draft_root).unwrap();
        let directories = DirectoryStore::open(&draft_root).unwrap();
        let local_dir = directories.create("drive", "root", "new folder").unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:child".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: local_dir.item_id.clone(),
                name: "inside.txt".into(),
            }),
        };
        DraftStore::open(&draft_root)
            .unwrap()
            .begin_empty(identity)
            .unwrap();
        let created = directories
            .record_created(&local_dir, "remote-folder")
            .unwrap();
        directories
            .mark_uploaded(&created, "remote-folder")
            .unwrap();
        let mut remote_folder = file_item();
        remote_folder.item_id = "remote-folder".into();
        remote_folder.name = Some("new folder".into());
        remote_folder.kind = "folder".into();
        let tree = VirtualTree::from_items("drive", "root", [remote_folder]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        fuse.load_local_files().unwrap();
        let folder = fuse.lookup_node(ROOT_INODE, "new folder").unwrap();
        assert_eq!(folder.item_id, "remote-folder");
        assert!(fuse.lookup_node(folder.inode, "inside.txt").is_some());
        assert!(DirectoryStore::open(&draft_root).unwrap().list().unwrap()[0].seen_remote);
    }

    #[test]
    fn queued_move_into_uploaded_local_directory_survives_remount() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        DraftStore::open(&draft_root).unwrap();
        let directories = DirectoryStore::open(&draft_root).unwrap();
        let local_folder = directories.create("drive", "root", "folder").unwrap();
        let created = directories
            .record_created(&local_folder, "remote-folder")
            .unwrap();
        directories
            .mark_uploaded(&created, "remote-folder")
            .unwrap();
        let make_fuse = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
        };
        let fuse = make_fuse();
        fuse.load_local_files().unwrap();
        let folder = fuse.lookup_node(ROOT_INODE, "folder").unwrap();
        fuse.rename_local_file(
            ROOT_INODE,
            "текст.txt",
            folder.inode,
            "Moved.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        drop(fuse);

        let fuse = make_fuse();
        fuse.load_local_files().unwrap();
        let folder = fuse.lookup_node(ROOT_INODE, "folder").unwrap();
        assert_eq!(folder.item_id, local_folder.item_id);
        let moved = fuse.lookup_node(folder.inode, "Moved.txt").unwrap();
        assert_eq!(moved.item_id, "file");
        assert!(
            fuse.directory_entries(folder.inode)
                .unwrap()
                .iter()
                .any(|entry| entry.2 == "Moved.txt")
        );
        assert_eq!(
            fuse.create_file(folder.inode, "moved.TXT", OpenFlags(libc::O_WRONLY)),
            Err(Errno::EEXIST)
        );
        assert_eq!(
            fuse.remove_local_file(folder.inode, "Moved.txt"),
            Err(Errno::EBUSY)
        );
        assert_eq!(
            MutationStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn queued_move_into_remote_directory_prevents_removing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let mut folder = file_item();
        folder.item_id = "folder".into();
        folder.name = Some("folder".into());
        folder.kind = "folder".into();
        let make_fuse = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item(), folder.clone()]).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
        };
        let fuse = make_fuse();
        let folder_inode = fuse.lookup_node(ROOT_INODE, "folder").unwrap().inode;
        fuse.rename_local_file(
            ROOT_INODE,
            "текст.txt",
            folder_inode,
            "moved.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        for fuse in [Some(fuse), None] {
            let fuse = match fuse {
                Some(fuse) => fuse,
                None => {
                    let fuse = make_fuse();
                    fuse.load_local_files().unwrap();
                    fuse
                }
            };
            assert!(fuse.lookup_node(folder_inode, "moved.txt").is_some());
            assert_eq!(
                fuse.remove_local_directory(ROOT_INODE, "folder"),
                Err(Errno::ENOTEMPTY)
            );
        }
        let mutations = MutationStore::open(&draft_root).unwrap().list().unwrap();
        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].item_id, "file");
    }

    #[test]
    fn remote_parent_waits_for_queued_child_removal_to_reach_catalog() {
        for delete_child in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let draft_root = dir.path().join("drafts");
            let mut folder = file_item();
            folder.item_id = "folder".into();
            folder.name = Some("folder".into());
            folder.kind = "folder".into();
            let mut child = file_item();
            child.parent_id = Some("folder".into());
            child.name = Some("child.txt".into());
            let make_fuse = || {
                VirtualFuse::new_writable(
                    VirtualTree::from_items("drive", "root", [folder.clone(), child.clone()])
                        .unwrap(),
                    dir.path().join("cache"),
                    draft_root.clone(),
                    10,
                    Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
                )
            };
            let fuse = make_fuse();
            let folder_inode = fuse.lookup_node(ROOT_INODE, "folder").unwrap().inode;
            if delete_child {
                fuse.remove_local_file(folder_inode, "child.txt").unwrap();
            } else {
                fuse.rename_local_file(
                    folder_inode,
                    "child.txt",
                    ROOT_INODE,
                    "moved.txt",
                    fuser::RenameFlags::empty(),
                )
                .unwrap();
            }
            for fuse in [Some(fuse), None] {
                let fuse = match fuse {
                    Some(fuse) => fuse,
                    None => {
                        let fuse = make_fuse();
                        fuse.load_local_files().unwrap();
                        fuse
                    }
                };
                assert_eq!(fuse.directory_entries(folder_inode).unwrap().len(), 2);
                assert_eq!(
                    fuse.remove_local_directory(ROOT_INODE, "folder"),
                    Err(Errno::EBUSY)
                );
            }
            let mutations = MutationStore::open(&draft_root).unwrap().list().unwrap();
            assert_eq!(mutations.len(), 1);
            assert_eq!(mutations[0].item_id, "file");
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
    fn empty_io_on_pending_remote_file_needs_no_download_or_draft() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_loader = Arc::clone(&calls);
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(move |_| {
                calls_for_loader.fetch_add(1, Ordering::SeqCst);
                Err(io::ErrorKind::NotConnected.into())
            }),
        );
        let inode = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode;
        let handle = fuse.open_file(inode, OpenFlags(libc::O_RDWR)).unwrap();

        assert_eq!(fuse.read_file(inode, handle, 0, 0).unwrap(), b"");
        assert_eq!(fuse.write_file(inode, handle, 0, b"").unwrap(), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            DraftStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fuse.write_file(inode, handle + 1, 0, b""),
            Err(Errno::EBADF)
        );
        assert_eq!(fuse.read_file(inode, handle + 1, 0, 0), Err(Errno::EBADF));

        assert_eq!(fuse.read_file(inode, handle, 0, 1), Err(Errno::ENETDOWN));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalid_write_offset_does_not_download_or_create_draft() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_loader = Arc::clone(&calls);
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(move |_| {
                calls_for_loader.fetch_add(1, Ordering::SeqCst);
                Err(io::ErrorKind::NotConnected.into())
            }),
        );
        let inode = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode;
        let handle = fuse.open_file(inode, OpenFlags(libc::O_WRONLY)).unwrap();

        assert_eq!(
            fuse.write_file(inode, handle, i64::MAX as u64, b"x"),
            Err(Errno::EINVAL)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            DraftStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fuse.write_file(inode, handle, 0, b"x"),
            Err(Errno::ENETDOWN)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn directory_plus_entry_attributes_follow_local_edits() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let (created, handle) = fuse
            .create_file(ROOT_INODE, "new.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(created.inode, handle, 0, b"local data")
            .unwrap();
        let remote_inode = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap().inode;
        let remote_handle = fuse
            .open_file(remote_inode, OpenFlags(libc::O_WRONLY | libc::O_TRUNC))
            .unwrap();
        fuse.write_file(remote_inode, remote_handle, 0, b"edited!")
            .unwrap();

        let store = DraftStore::open(&draft_root).unwrap();
        let entries: Vec<_> = fuse
            .directory_entries(ROOT_INODE)
            .unwrap()
            .into_iter()
            .map(|(inode, _, name)| {
                (
                    name,
                    fuse.directory_entry_attr(inode, Some(&store)).unwrap(),
                )
            })
            .collect();
        assert_eq!(
            entries
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            [".", "..", "new.txt", "текст.txt"]
        );
        assert_eq!(entries[0].1.ino, INodeNo(ROOT_INODE));
        assert_eq!(entries[0].1.kind, FileType::Directory);
        assert_eq!(entries[1].1.ino, INodeNo(ROOT_INODE));
        assert_eq!(entries[2].1.ino, INodeNo(created.inode));
        assert_eq!(entries[2].1.size, 10);
        assert_eq!(entries[2].1.kind, FileType::RegularFile);
        assert_eq!(entries[3].1.size, 7);
        assert_eq!(
            fuse.directory_entry_attr(u64::MAX, Some(&store))
                .unwrap_err(),
            Errno::ENOENT
        );
    }

    #[test]
    fn open_directory_keeps_entry_offsets_stable_until_release() {
        let dir = tempfile::tempdir().unwrap();
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let (middle, file_handle) = fuse
            .create_file(ROOT_INODE, "middle.txt", OpenFlags(libc::O_WRONLY))
            .unwrap();
        fuse.files.lock().unwrap().remove(&file_handle);
        let handle = fuse.open_directory(ROOT_INODE).unwrap();
        let snapshot = fuse
            .directory_entries_for_handle(ROOT_INODE, handle)
            .unwrap();
        assert_eq!(
            snapshot[2..]
                .iter()
                .map(|(_, _, name)| name.as_str())
                .collect::<Vec<_>>(),
            ["middle.txt", "текст.txt"]
        );

        let (_, file_handle) = fuse
            .create_file(ROOT_INODE, "alpha.txt", OpenFlags(libc::O_WRONLY))
            .unwrap();
        fuse.files.lock().unwrap().remove(&file_handle);
        assert_eq!(snapshot[2].0, middle.inode);
        assert_eq!(
            fuse.directory_entries_for_handle(ROOT_INODE, handle)
                .unwrap()[3]
                .2,
            "текст.txt"
        );
        assert_eq!(
            fuse.directory_entries(ROOT_INODE).unwrap()[2].2,
            "alpha.txt"
        );
        fuse.remove_local_file(ROOT_INODE, "middle.txt").unwrap();
        assert_eq!(snapshot[2].2, "middle.txt");
        assert_eq!(
            fuse.directory_entry_attr(middle.inode, None).unwrap_err(),
            Errno::ENOENT
        );
        assert_eq!(
            fuse.directory_entries_for_handle(middle.inode, handle)
                .unwrap_err(),
            Errno::EBADF
        );
        let remote_file = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        assert_eq!(fuse.open_directory(remote_file.inode), Err(Errno::ENOTDIR));
        assert_eq!(fuse.open_directory(u64::MAX), Err(Errno::ENOENT));

        assert_eq!(fuse.close_directory(ROOT_INODE, handle), Ok(()));
        assert_eq!(
            fuse.directory_entries_for_handle(ROOT_INODE, handle)
                .unwrap_err(),
            Errno::EBADF
        );
        let reopened = fuse.open_directory(ROOT_INODE).unwrap();
        assert_eq!(
            fuse.directory_entries_for_handle(ROOT_INODE, reopened)
                .unwrap()[2]
                .2,
            "alpha.txt"
        );
        assert_eq!(fuse.close_directory(ROOT_INODE, reopened), Ok(()));
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
    fn append_open_ignores_supplied_offsets_and_preserves_positioned_writes() {
        let dir = tempfile::tempdir().unwrap();
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", []).unwrap(),
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let (node, normal) = fuse
            .create_file(ROOT_INODE, "append.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(node.inode, normal, 0, b"first").unwrap();
        let append = fuse
            .open_file(node.inode, OpenFlags(libc::O_WRONLY | libc::O_APPEND))
            .unwrap();
        fuse.write_file(node.inode, append, 0, b" second").unwrap();
        fuse.write_file(node.inode, normal, 0, b"F").unwrap();
        fuse.write_file(node.inode, append, 1, b" third").unwrap();
        assert_eq!(
            fuse.read_file(node.inode, normal, 0, 64).unwrap(),
            b"First second third"
        );
    }

    #[test]
    fn mounted_append_survives_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", []).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let path = mountpoint.join("append.txt");
        let session = mount();
        fs::write(&path, b"first").unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b" second")
            .unwrap();
        session.umount_and_join().unwrap();
        let session = mount();
        assert_eq!(fs::read(&path).unwrap(), b"first second");
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_unlinked_file_keeps_open_descriptor_and_name_can_be_reused() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", []).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let path = mountpoint.join("note.txt");
        let session = mount();
        let mut old = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        old.write_all(b"old").unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!path.exists());
        old.write_all(b"!").unwrap();
        old.set_len(3).unwrap();
        assert_eq!(old.metadata().unwrap().len(), 3);
        fs::write(&path, b"new").unwrap();
        let mut bytes = [0; 3];
        assert_eq!(old.read_at(&mut bytes, 0).unwrap(), 3);
        assert_eq!(&bytes, b"old");
        drop(old);
        session.umount_and_join().unwrap();

        let session = mount();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(
            DraftStore::open(&draft_root).unwrap().list().unwrap().len(),
            1
        );
        session.umount_and_join().unwrap();
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
    fn mounted_editor_can_set_modified_time_after_saving_a_draft() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        fs::create_dir(&mountpoint).unwrap();
        let session = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            dir.path().join("drafts"),
            20,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        )
        .mount(&mountpoint)
        .unwrap();
        let path = mountpoint.join("текст.txt");
        fs::write(&path, b"edited locally").unwrap();
        let modified = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"edited locally");
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        session.umount_and_join().unwrap();
    }

    #[test]
    fn new_file_survives_remount_as_local_draft() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let cache_root = dir.path().join("cache");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let loader: Arc<ContentLoader> = Arc::new(|_| Err(io::ErrorKind::NotConnected.into()));
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let session = VirtualFuse::new_writable(
            tree,
            cache_root.clone(),
            draft_root.clone(),
            10,
            Arc::clone(&loader),
        )
        .mount(&mountpoint)
        .unwrap();
        let created = mountpoint.join("новый файл.txt");
        fs::write(&created, b"local content").unwrap();
        assert_eq!(fs::read(&created).unwrap(), b"local content");
        let copy_source = dir.path().join("copy-source");
        fs::write(&copy_source, b"copied bytes").unwrap();
        fs::copy(&copy_source, mountpoint.join("copied.txt")).unwrap();
        assert_eq!(
            fs::read(mountpoint.join("copied.txt")).unwrap(),
            b"copied bytes"
        );
        assert!(
            fs::read_dir(&mountpoint)
                .unwrap()
                .any(|entry| entry.unwrap().file_name() == "новый файл.txt")
        );
        session.umount_and_join().unwrap();
        let drafts = DraftStore::open(&draft_root).unwrap().list().unwrap();
        assert_eq!(drafts.len(), 2);
        assert!(drafts.iter().any(|draft| {
            draft
                .new_file
                .as_ref()
                .is_some_and(|file| file.name == "новый файл.txt")
        }));
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let session = VirtualFuse::new_writable(tree, cache_root, draft_root, 10, loader)
            .mount(&mountpoint)
            .unwrap();
        assert_eq!(fs::read(&created).unwrap(), b"local content");
        assert_eq!(
            fs::read(mountpoint.join("copied.txt")).unwrap(),
            b"copied bytes"
        );
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_new_file_rename_and_delete_survive_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let loader: Arc<ContentLoader> = Arc::new(|_| Err(io::ErrorKind::NotConnected.into()));
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::clone(&loader),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let session = mount();
        fs::write(mountpoint.join("before.txt"), b"persistent").unwrap();
        fs::rename(mountpoint.join("before.txt"), mountpoint.join("after.txt")).unwrap();
        session.umount_and_join().unwrap();
        let session = mount();
        assert!(!mountpoint.join("before.txt").exists());
        assert_eq!(
            fs::read(mountpoint.join("after.txt")).unwrap(),
            b"persistent"
        );
        fs::remove_file(mountpoint.join("after.txt")).unwrap();
        session.umount_and_join().unwrap();
        let session = mount();
        assert!(!mountpoint.join("after.txt").exists());
        assert!(
            DraftStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_nested_directories_and_files_survive_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let loader: Arc<ContentLoader> = Arc::new(|_| Err(io::ErrorKind::NotConnected.into()));
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::clone(&loader),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let session = mount();
        fs::create_dir(mountpoint.join("parent")).unwrap();
        fs::create_dir(mountpoint.join("parent/child")).unwrap();
        fs::write(mountpoint.join("parent/child/document.txt"), b"nested").unwrap();
        assert_eq!(
            fs::remove_dir(mountpoint.join("parent/child"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::DirectoryNotEmpty
        );
        session.umount_and_join().unwrap();
        let session = mount();
        assert_eq!(
            fs::read(mountpoint.join("parent/child/document.txt")).unwrap(),
            b"nested"
        );
        fs::rename(mountpoint.join("parent"), mountpoint.join("renamed")).unwrap();
        assert_eq!(
            fs::read(mountpoint.join("renamed/child/document.txt")).unwrap(),
            b"nested"
        );
        fs::remove_file(mountpoint.join("renamed/child/document.txt")).unwrap();
        fs::remove_dir(mountpoint.join("renamed/child")).unwrap();
        fs::remove_dir(mountpoint.join("renamed")).unwrap();
        session.umount_and_join().unwrap();
        let session = mount();
        assert!(
            fs::read_dir(&mountpoint)
                .unwrap()
                .all(|entry| entry.unwrap().file_name() != "renamed")
        );
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_editor_rename_replaces_remote_content_without_network() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let session = mount();
        fs::write(mountpoint.join("swap.tmp"), b"offline edit").unwrap();
        fs::rename(mountpoint.join("swap.tmp"), mountpoint.join("текст.txt")).unwrap();
        assert_eq!(
            fs::read(mountpoint.join("текст.txt")).unwrap(),
            b"offline edit"
        );
        assert_eq!(
            fs::read_dir(&mountpoint)
                .unwrap()
                .filter(|entry| entry.as_ref().unwrap().file_name() == "текст.txt")
                .count(),
            1
        );
        session.umount_and_join().unwrap();
        let session = mount();
        assert!(!mountpoint.join("swap.tmp").exists());
        assert_eq!(
            fs::read(mountpoint.join("текст.txt")).unwrap(),
            b"offline edit"
        );
        assert_eq!(
            DraftStore::open(&draft_root).unwrap().list().unwrap().len(),
            1
        );
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_editor_can_rename_open_temporary_file_over_existing_file() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let session = mount();
        let target = mountpoint.join("текст.txt");
        let temporary = mountpoint.join("текст.txt.tmp");
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .unwrap();
        use std::io::Write;
        output.write_all(b"first edit").unwrap();
        output.sync_all().unwrap();
        fs::rename(&temporary, &target).unwrap();
        output.sync_all().unwrap();
        drop(output);
        assert_eq!(fs::read(&target).unwrap(), b"first edit");
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .unwrap();
        output.write_all(b"second edit").unwrap();
        output.sync_all().unwrap();
        fs::rename(&temporary, &target).unwrap();
        output.sync_all().unwrap();
        drop(output);
        assert_eq!(fs::read(&target).unwrap(), b"second edit");
        session.umount_and_join().unwrap();
        let session = mount();
        assert_eq!(fs::read(&target).unwrap(), b"second edit");
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_local_file_replacement_survives_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", []).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let session = mount();
        fs::write(mountpoint.join("source.txt"), b"new").unwrap();
        fs::write(mountpoint.join("target.txt"), b"old").unwrap();
        let old_target = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(mountpoint.join("target.txt"))
            .unwrap();
        fs::rename(mountpoint.join("source.txt"), mountpoint.join("target.txt")).unwrap();
        assert_eq!(fs::read(mountpoint.join("target.txt")).unwrap(), b"new");
        let mut old_bytes = [0; 4];
        assert_eq!(old_target.read_at(&mut old_bytes, 0).unwrap(), 3);
        assert_eq!(&old_bytes[..3], b"old");
        old_target.write_at(b"!", 3).unwrap();
        assert_eq!(old_target.read_at(&mut old_bytes, 0).unwrap(), 4);
        assert_eq!(&old_bytes, b"old!");
        drop(old_target);
        session.umount_and_join().unwrap();
        let session = mount();
        assert!(!mountpoint.join("source.txt").exists());
        assert_eq!(fs::read(mountpoint.join("target.txt")).unwrap(), b"new");
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_empty_directory_replacement_survives_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", []).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let session = mount();
        fs::create_dir(mountpoint.join("source")).unwrap();
        fs::create_dir(mountpoint.join("target")).unwrap();
        fs::write(mountpoint.join("source/file.txt"), b"kept").unwrap();
        fs::rename(mountpoint.join("source"), mountpoint.join("target")).unwrap();
        assert_eq!(
            fs::read(mountpoint.join("target/file.txt")).unwrap(),
            b"kept"
        );
        session.umount_and_join().unwrap();
        let session = mount();
        assert!(!mountpoint.join("source").exists());
        assert_eq!(
            fs::read(mountpoint.join("target/file.txt")).unwrap(),
            b"kept"
        );
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_remote_metadata_operations_are_durable_before_graph() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let mut second = file_item();
        second.item_id = "second".into();
        second.name = Some("remove.txt".into());
        let loader: Arc<ContentLoader> = Arc::new(|_| Err(io::ErrorKind::NotConnected.into()));
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item(), second.clone()]).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::clone(&loader),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let session = mount();
        fs::rename(mountpoint.join("текст.txt"), mountpoint.join("moved.txt")).unwrap();
        fs::remove_file(mountpoint.join("remove.txt")).unwrap();
        session.umount_and_join().unwrap();
        let session = mount();
        assert!(mountpoint.join("moved.txt").exists());
        assert!(!mountpoint.join("текст.txt").exists());
        assert!(!mountpoint.join("remove.txt").exists());
        assert_eq!(
            MutationStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .len(),
            2
        );
        session.umount_and_join().unwrap();
    }

    #[test]
    fn create_file_rejects_name_collision_without_overwriting_draft() {
        let dir = tempfile::tempdir().unwrap();
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        assert_eq!(
            fuse.create_file(ROOT_INODE, "текст.txt", OpenFlags(libc::O_WRONLY))
                .unwrap_err(),
            Errno::EEXIST
        );
        let (created, handle) = fuse
            .create_file(ROOT_INODE, "new.txt", OpenFlags(libc::O_WRONLY))
            .unwrap();
        fuse.write_file(created.inode, handle, 0, b"data").unwrap();
        assert_eq!(
            fuse.create_file(ROOT_INODE, "new.txt", OpenFlags(libc::O_WRONLY))
                .unwrap_err(),
            Errno::EEXIST
        );
        assert_eq!(
            fuse.read_file(created.inode, handle, 0, 4),
            Err(Errno::EBADF)
        );
        let reader = fuse
            .open_file(created.inode, OpenFlags(libc::O_RDONLY))
            .unwrap();
        assert_eq!(
            fuse.read_file(created.inode, reader, 0, 4).unwrap(),
            b"data"
        );
    }

    #[test]
    fn virtual_creation_rejects_case_collisions_with_remote_and_local_names() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        assert_eq!(
            fuse.create_file(ROOT_INODE, "ТЕКСТ.txt", OpenFlags(libc::O_WRONLY)),
            Err(Errno::EEXIST)
        );
        assert_eq!(
            fuse.create_directory(ROOT_INODE, "ТЕКСТ.TXT"),
            Err(Errno::EEXIST)
        );
        let (file, handle) = fuse
            .create_file(ROOT_INODE, "Local.txt", OpenFlags(libc::O_WRONLY))
            .unwrap();
        fuse.files.lock().unwrap().remove(&handle);
        assert_eq!(
            fuse.create_directory(ROOT_INODE, "local.TXT"),
            Err(Errno::EEXIST)
        );
        let folder = fuse.create_directory(ROOT_INODE, "Folder").unwrap();
        assert_eq!(
            fuse.create_file(ROOT_INODE, "folder", OpenFlags(libc::O_WRONLY)),
            Err(Errno::EEXIST)
        );
        assert_eq!(
            DraftStore::open(&draft_root).unwrap().list().unwrap().len(),
            1
        );
        assert_eq!(
            DirectoryStore::open(&draft_root)
                .unwrap()
                .list()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "Local.txt").unwrap().inode,
            file.inode
        );
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "Folder").unwrap().inode,
            folder.inode
        );
    }

    #[test]
    fn virtual_rename_rejects_case_collision_but_allows_own_case_change() {
        let dir = tempfile::tempdir().unwrap();
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let (local, handle) = fuse
            .create_file(ROOT_INODE, "draft.txt", OpenFlags(libc::O_WRONLY))
            .unwrap();
        fuse.files.lock().unwrap().remove(&handle);
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "draft.txt",
                ROOT_INODE,
                "ТЕКСТ.txt",
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EEXIST)
        );
        assert!(fuse.lookup_node(ROOT_INODE, "draft.txt").is_some());
        fuse.rename_local_file(
            ROOT_INODE,
            "draft.txt",
            ROOT_INODE,
            "DRAFT.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert_eq!(
            fuse.lookup_node(ROOT_INODE, "DRAFT.txt").unwrap().inode,
            local.inode
        );
        assert_eq!(
            fuse.rename_local_file(
                ROOT_INODE,
                "текст.txt",
                ROOT_INODE,
                "draft.TXT",
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EEXIST)
        );
        fuse.rename_local_file(
            ROOT_INODE,
            "текст.txt",
            ROOT_INODE,
            "ТЕКСТ.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert!(fuse.lookup_node(ROOT_INODE, "ТЕКСТ.txt").is_some());
    }

    #[test]
    fn virtual_creation_rejects_case_collision_with_queued_remote_rename() {
        let dir = tempfile::tempdir().unwrap();
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        fuse.rename_local_file(
            ROOT_INODE,
            "текст.txt",
            ROOT_INODE,
            "Queued.txt",
            fuser::RenameFlags::empty(),
        )
        .unwrap();
        assert_eq!(
            fuse.create_file(ROOT_INODE, "queued.TXT", OpenFlags(libc::O_WRONLY)),
            Err(Errno::EEXIST)
        );
        assert_eq!(
            fuse.create_directory(ROOT_INODE, "QUEUED.txt"),
            Err(Errno::EEXIST)
        );
        assert!(fuse.lookup_node(ROOT_INODE, "Queued.txt").is_some());
    }

    #[test]
    fn concurrent_virtual_creation_and_remote_rename_cannot_claim_same_name() {
        use std::sync::Barrier;

        for directory in [false, true] {
            for _ in 0..8 {
                let dir = tempfile::tempdir().unwrap();
                let mut source = file_item();
                source.name = Some("source".into());
                if directory {
                    source.kind = "folder".into();
                }
                let fuse = Arc::new(VirtualFuse::new_writable(
                    VirtualTree::from_items("drive", "root", [source]).unwrap(),
                    dir.path().join("cache"),
                    dir.path().join("drafts"),
                    10,
                    Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
                ));
                let barrier = Arc::new(Barrier::new(3));
                let (created, renamed) = std::thread::scope(|scope| {
                    let create_fuse = Arc::clone(&fuse);
                    let create_barrier = Arc::clone(&barrier);
                    let create = scope.spawn(move || {
                        create_barrier.wait();
                        if directory {
                            create_fuse
                                .create_directory(ROOT_INODE, "Target")
                                .map(|_| ())
                        } else {
                            create_fuse
                                .create_file(ROOT_INODE, "Target", OpenFlags(libc::O_WRONLY))
                                .map(|_| ())
                        }
                    });
                    let rename_fuse = Arc::clone(&fuse);
                    let rename_barrier = Arc::clone(&barrier);
                    let rename = scope.spawn(move || {
                        rename_barrier.wait();
                        rename_fuse.rename_local_file(
                            ROOT_INODE,
                            "source",
                            ROOT_INODE,
                            "target",
                            fuser::RenameFlags::empty(),
                        )
                    });
                    barrier.wait();
                    (create.join().unwrap(), rename.join().unwrap())
                });
                assert_ne!(created.is_ok(), renamed.is_ok());
                assert_eq!(created.err().or_else(|| renamed.err()), Some(Errno::EEXIST));
                assert_eq!(
                    fuse.directory_entries(ROOT_INODE)
                        .unwrap()
                        .iter()
                        .filter(|entry| entry.2.to_lowercase() == "target")
                        .count(),
                    1
                );
            }
        }
    }

    #[test]
    fn regular_mknod_creates_durable_file_and_rejects_special_nodes() {
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
        for mode in [libc::S_IFDIR, libc::S_IFLNK, libc::S_IFIFO, libc::S_IFCHR] {
            assert_eq!(
                fuse.create_regular_node(ROOT_INODE, "special", mode | 0o600),
                Err(Errno::EOPNOTSUPP)
            );
        }
        assert!(fuse.lookup_node(ROOT_INODE, "special").is_none());
        let node = fuse
            .create_regular_node(ROOT_INODE, "created.txt", libc::S_IFREG | 0o644)
            .unwrap();
        assert!(fuse.files.lock().unwrap().is_empty());
        assert_eq!(fuse.attr(&node).unwrap().kind, FileType::RegularFile);
        assert_eq!(fuse.attr(&node).unwrap().size, 0);
        assert_eq!(
            fuse.create_regular_node(ROOT_INODE, "created.txt", libc::S_IFREG),
            Err(Errno::EEXIST)
        );
        assert_eq!(
            DraftStore::open(&draft_root).unwrap().list().unwrap().len(),
            1
        );

        let reopened = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root,
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        reopened.load_local_files().unwrap();
        assert_eq!(
            reopened
                .lookup_node(ROOT_INODE, "created.txt")
                .unwrap()
                .inode,
            node.inode
        );
    }

    #[test]
    fn mounted_regular_mknod_survives_remount() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        let draft_root = dir.path().join("drafts");
        fs::create_dir(&mountpoint).unwrap();
        let mount = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", []).unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
            .mount(&mountpoint)
            .unwrap()
        };
        let path = mountpoint.join("created.txt");
        let raw_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let session = mount();
        assert_eq!(
            unsafe { libc::mknod(raw_path.as_ptr(), libc::S_IFREG | 0o600, 0) },
            0
        );
        fs::write(&path, b"data").unwrap();
        session.umount_and_join().unwrap();
        let session = mount();
        assert_eq!(fs::read(&path).unwrap(), b"data");
        session.umount_and_join().unwrap();
    }

    #[test]
    fn remote_name_collision_keeps_local_file_visible_after_remount() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:pending".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "текст.txt".into(),
            }),
        };
        DraftStore::open(&draft_root)
            .unwrap()
            .begin_empty(identity)
            .unwrap()
            .write_at(0, b"my copy")
            .unwrap();
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            draft_root,
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        fuse.load_local_files().unwrap();
        let entries = fuse.directory_entries(ROOT_INODE).unwrap();
        let conflict = entries
            .iter()
            .find(|entry| entry.2.starts_with(".conflict-"))
            .unwrap();
        let inode = fuse.lookup_node(ROOT_INODE, &conflict.2).unwrap().inode;
        let handle = fuse.open_file(inode, OpenFlags(libc::O_RDONLY)).unwrap();
        assert_eq!(fuse.read_file(inode, handle, 0, 7).unwrap(), b"my copy");
    }

    #[test]
    fn case_collisions_after_catalog_refresh_keep_local_file_and_directory_visible() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:pending".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "ТЕКСТ.txt".into(),
            }),
        };
        DraftStore::open(&draft_root)
            .unwrap()
            .begin_empty(identity)
            .unwrap()
            .write_at(0, b"local copy")
            .unwrap();
        DirectoryStore::open(&draft_root)
            .unwrap()
            .create("drive", "root", "docs")
            .unwrap();
        let mut remote_folder = file_item();
        remote_folder.item_id = "remote-folder".into();
        remote_folder.name = Some("Docs".into());
        remote_folder.kind = "folder".into();
        let make_fuse = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items("drive", "root", [file_item(), remote_folder.clone()])
                    .unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
        };
        for _ in 0..2 {
            let fuse = make_fuse();
            fuse.load_local_files().unwrap();
            let entries = fuse.directory_entries(ROOT_INODE).unwrap();
            assert_eq!(entries.len(), 6);
            assert!(fuse.lookup_node(ROOT_INODE, "текст.txt").is_some());
            assert!(fuse.lookup_node(ROOT_INODE, "Docs").is_some());
            assert!(fuse.lookup_node(ROOT_INODE, "ТЕКСТ.txt").is_none());
            assert!(fuse.lookup_node(ROOT_INODE, "docs").is_none());
            let local_file = entries
                .iter()
                .find(|entry| entry.2.starts_with(".conflict-") && entry.2.ends_with("ТЕКСТ.txt"))
                .unwrap();
            let local_folder = entries
                .iter()
                .find(|entry| entry.2.starts_with(".conflict-") && entry.2.ends_with("docs"))
                .unwrap();
            let handle = fuse
                .open_file(local_file.0, OpenFlags(libc::O_RDONLY))
                .unwrap();
            assert_eq!(
                fuse.read_file(local_file.0, handle, 0, 10).unwrap(),
                b"local copy"
            );
            assert_eq!(fuse.node(local_folder.0).unwrap().kind, NodeKind::Directory);
            assert_eq!(
                DraftStore::open(&draft_root).unwrap().list().unwrap().len(),
                1
            );
            assert_eq!(
                DirectoryStore::open(&draft_root)
                    .unwrap()
                    .list()
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[test]
    fn recovery_names_do_not_hide_existing_remote_items() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        let local_file_id = "local:pending";
        let file_inode = crate::stable_inode("drive", local_file_id);
        DraftStore::open(&draft_root)
            .unwrap()
            .begin_empty(DraftIdentity {
                drive_id: "drive".into(),
                item_id: local_file_id.into(),
                base_etag: "local-new".into(),
                new_file: Some(NewFileLocation {
                    parent_id: "root".into(),
                    name: "ТЕКСТ.txt".into(),
                }),
            })
            .unwrap()
            .write_at(0, b"local copy")
            .unwrap();
        let local_directory = DirectoryStore::open(&draft_root)
            .unwrap()
            .create("drive", "root", "docs")
            .unwrap();
        let directory_inode = crate::stable_inode("drive", &local_directory.item_id);
        let file_alias = format!(".conflict-{file_inode}-ТЕКСТ.txt");
        let directory_alias = format!(".conflict-{directory_inode}-docs");
        let mut existing_file_alias = file_item();
        existing_file_alias.item_id = "existing-file-alias".into();
        existing_file_alias.name = Some(file_alias.clone());
        let mut remote_directory = file_item();
        remote_directory.item_id = "remote-directory".into();
        remote_directory.name = Some("Docs".into());
        remote_directory.kind = "folder".into();
        let mut existing_directory_alias = file_item();
        existing_directory_alias.item_id = "existing-directory-alias".into();
        existing_directory_alias.name = Some(directory_alias.clone());
        let make_fuse = || {
            VirtualFuse::new_writable(
                VirtualTree::from_items(
                    "drive",
                    "root",
                    [
                        file_item(),
                        existing_file_alias.clone(),
                        remote_directory.clone(),
                        existing_directory_alias.clone(),
                    ],
                )
                .unwrap(),
                dir.path().join("cache"),
                draft_root.clone(),
                10,
                Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
            )
        };
        for _ in 0..2 {
            let fuse = make_fuse();
            fuse.load_local_files().unwrap();
            assert_eq!(
                fuse.lookup_node(ROOT_INODE, &file_alias).unwrap().item_id,
                "existing-file-alias"
            );
            assert_eq!(
                fuse.lookup_node(ROOT_INODE, &directory_alias)
                    .unwrap()
                    .item_id,
                "existing-directory-alias"
            );
            let recovered_file = fuse
                .lookup_node(ROOT_INODE, &format!(".conflict-{file_inode}-2-ТЕКСТ.txt"))
                .unwrap();
            let recovered_directory = fuse
                .lookup_node(ROOT_INODE, &format!(".conflict-{directory_inode}-2-docs"))
                .unwrap();
            assert_eq!(recovered_directory.item_id, local_directory.item_id);
            let handle = fuse
                .open_file(recovered_file.inode, OpenFlags(libc::O_RDONLY))
                .unwrap();
            assert_eq!(
                fuse.read_file(recovered_file.inode, handle, 0, 10).unwrap(),
                b"local copy"
            );
            assert_eq!(fuse.directory_entries(ROOT_INODE).unwrap().len(), 8);
        }
    }

    #[test]
    fn recovery_name_stays_within_linux_component_limit() {
        let dir = tempfile::tempdir().unwrap();
        let fuse = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", []).unwrap(),
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        let parent = fuse.tree.getattr(ROOT_INODE).unwrap();
        let name = fuse.recovery_name(parent, u64::MAX, &"💾".repeat(100), false, &HashMap::new());
        assert!(name.starts_with(".conflict-"));
        assert!(name.len() <= 255);
        assert!(name.is_char_boundary(name.len()));
    }

    #[test]
    fn missing_remote_item_keeps_existing_draft_visible_until_catalog_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let draft_root = dir.path().join("drafts");
        DraftStore::open(&draft_root)
            .unwrap()
            .begin_empty(DraftIdentity {
                drive_id: "drive".into(),
                item_id: "file".into(),
                base_etag: "v1".into(),
                new_file: None,
            })
            .unwrap()
            .write_at(0, b"unsent changes")
            .unwrap();
        let missing = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", []).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        missing.load_local_files().unwrap();
        let orphan_name = format!(
            ".orphan-{}-remote-file",
            crate::stable_inode("drive", "file")
        );
        let orphan = missing.lookup_node(ROOT_INODE, &orphan_name).unwrap();
        let handle = missing
            .open_file(orphan.inode, OpenFlags(libc::O_RDONLY))
            .unwrap();
        assert_eq!(
            missing.read_file(orphan.inode, handle, 0, 14).unwrap(),
            b"unsent changes"
        );
        assert_eq!(
            missing.remove_local_file(ROOT_INODE, &orphan_name),
            Err(Errno::EBUSY)
        );
        drop(missing);

        let restored = VirtualFuse::new_writable(
            VirtualTree::from_items("drive", "root", [file_item()]).unwrap(),
            dir.path().join("cache"),
            draft_root.clone(),
            10,
            Arc::new(|_| Err(io::ErrorKind::NotConnected.into())),
        );
        restored.load_local_files().unwrap();
        assert!(restored.lookup_node(ROOT_INODE, &orphan_name).is_none());
        let file = restored.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        let handle = restored
            .open_file(file.inode, OpenFlags(libc::O_RDONLY))
            .unwrap();
        assert_eq!(
            restored.read_file(file.inode, handle, 0, 14).unwrap(),
            b"unsent changes"
        );
        assert_eq!(
            DraftStore::open(&draft_root).unwrap().list().unwrap().len(),
            1
        );
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
        assert_eq!(fuse.read_file(inode, handle, 0, 5), Err(Errno::EBADF));
        let reader = fuse.open_file(inode, OpenFlags(libc::O_RDONLY)).unwrap();
        assert_eq!(fuse.read_file(inode, reader, 0, 5).unwrap(), b"local");
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

    #[test]
    fn slow_remote_open_allows_local_open_and_sees_new_draft() {
        let dir = tempfile::tempdir().unwrap();
        let cache_root = dir.path().join("cache");
        let source = dir.path().join("source");
        fs::write(&source, b"cloud").unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let gate_for_loader = Arc::clone(&gate);
        let cache_for_loader = cache_root.clone();
        let loader: Arc<ContentLoader> = Arc::new(move |request| {
            started_tx.send(()).unwrap();
            let (lock, wake) = &*gate_for_loader;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            let mut cache = CacheStore::open(&cache_for_loader, 10).map_err(io::Error::other)?;
            cache
                .put(&request.cache_key(), &source, request.size)
                .map_err(io::Error::other)
        });
        let tree = VirtualTree::from_items("drive", "root", [file_item()]).unwrap();
        let fuse = Arc::new(VirtualFuse::new_writable(
            tree,
            cache_root,
            dir.path().join("drafts"),
            10,
            loader,
        ));
        let (local, local_handle) = fuse
            .create_file(ROOT_INODE, "local.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(local.inode, local_handle, 0, b"local")
            .unwrap();
        let remote = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        let remote_reader = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || fuse.open_file(remote.inode, OpenFlags(libc::O_RDONLY)))
        };
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (local_tx, local_rx) = mpsc::channel();
        let local_reader = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || {
                let handle = fuse.open_file(local.inode, OpenFlags(libc::O_RDONLY))?;
                local_tx
                    .send(fuse.read_file(local.inode, handle, 0, 5))
                    .unwrap();
                Ok::<(), Errno>(())
            })
        };
        let local_result = local_rx.recv_timeout(Duration::from_secs(2));
        let (replacement_tx, replacement_rx) = mpsc::channel();
        let replacement_writer = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || {
                let result = fuse
                    .open_file(remote.inode, OpenFlags(libc::O_WRONLY | libc::O_TRUNC))
                    .and_then(|handle| fuse.write_file(remote.inode, handle, 0, b"draft"));
                replacement_tx.send(result).unwrap();
            })
        };
        let replacement_result = replacement_rx.recv_timeout(Duration::from_secs(2));
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        assert_eq!(local_result.unwrap().unwrap(), b"local");
        assert_eq!(replacement_result.unwrap(), Ok(5));
        local_reader.join().unwrap().unwrap();
        replacement_writer.join().unwrap();
        let remote_handle = remote_reader.join().unwrap().unwrap();
        assert_eq!(
            fuse.read_file(remote.inode, remote_handle, 0, 5).unwrap(),
            b"draft"
        );
    }

    #[test]
    fn slow_download_does_not_block_cached_file_open() {
        let dir = tempfile::tempdir().unwrap();
        let cache_root = dir.path().join("cache");
        let source = dir.path().join("source");
        fs::write(&source, b"ready").unwrap();
        let mut cached_item = file_item();
        cached_item.item_id = "cached".into();
        cached_item.name = Some("cached.txt".into());
        let tree = VirtualTree::from_items("drive", "root", [file_item(), cached_item]).unwrap();
        let cached_request = tree.request_for_item("cached").unwrap();
        CacheStore::open(&cache_root, 10)
            .unwrap()
            .put(&cached_request.cache_key(), &source, 5)
            .unwrap();
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
        let fuse = Arc::new(VirtualFuse::new(tree, cache_root, 10, loader));
        let uncached = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        let remote_reader = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || fuse.open_file(uncached.inode, OpenFlags(libc::O_RDONLY)))
        };
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let cached = fuse.lookup_node(ROOT_INODE, "cached.txt").unwrap();
        let (cached_tx, cached_rx) = mpsc::channel();
        let cached_reader = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || {
                let result = fuse
                    .open_file(cached.inode, OpenFlags(libc::O_RDONLY))
                    .and_then(|handle| fuse.read_file(cached.inode, handle, 0, 5));
                cached_tx.send(result).unwrap();
            })
        };
        let cached_result = cached_rx.recv_timeout(Duration::from_secs(2));
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        assert_eq!(cached_result.unwrap().unwrap(), b"ready");
        cached_reader.join().unwrap();
        assert_eq!(remote_reader.join().unwrap(), Err(Errno::ENETDOWN));
    }

    #[test]
    fn mounted_slow_download_keeps_cached_file_responsive() {
        if std::env::var_os("ONEDRIVE_FUSE_SMOKE").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mount");
        fs::create_dir(&mountpoint).unwrap();
        let cache_root = dir.path().join("cache");
        let source = dir.path().join("source");
        fs::write(&source, b"ready").unwrap();
        let mut cached_item = file_item();
        cached_item.item_id = "cached".into();
        cached_item.name = Some("cached.txt".into());
        let tree = VirtualTree::from_items("drive", "root", [file_item(), cached_item]).unwrap();
        let cached_request = tree.request_for_item("cached").unwrap();
        CacheStore::open(&cache_root, 10)
            .unwrap()
            .put(&cached_request.cache_key(), &source, 5)
            .unwrap();
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
        let session = VirtualFuse::new(tree, cache_root, 10, loader)
            .mount(&mountpoint)
            .unwrap();
        let uncached_path = mountpoint.join("текст.txt");
        let remote_reader = std::thread::spawn(move || fs::read(uncached_path));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let cached_path = mountpoint.join("cached.txt");
        let (cached_tx, cached_rx) = mpsc::channel();
        let cached_reader = std::thread::spawn(move || {
            cached_tx.send(fs::read(cached_path)).unwrap();
        });
        let cached_result = cached_rx.recv_timeout(Duration::from_secs(2));
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        assert_eq!(cached_result.unwrap().unwrap(), b"ready");
        cached_reader.join().unwrap();
        assert_eq!(
            remote_reader.join().unwrap().unwrap_err().raw_os_error(),
            Some(libc::ENETDOWN)
        );
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_slow_remote_open_keeps_local_open_responsive() {
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
        let session = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            loader,
        )
        .mount(&mountpoint)
        .unwrap();
        fs::write(mountpoint.join("local.txt"), b"local").unwrap();
        let remote_path = mountpoint.join("текст.txt");
        let remote_reader = std::thread::spawn(move || fs::read(remote_path));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (local_tx, local_rx) = mpsc::channel();
        let local_path = mountpoint.join("local.txt");
        let local_reader = std::thread::spawn(move || {
            local_tx.send(fs::read(local_path)).unwrap();
        });
        let local_result = local_rx.recv_timeout(Duration::from_secs(2));
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        assert_eq!(local_result.unwrap().unwrap(), b"local");
        local_reader.join().unwrap();
        assert_eq!(
            remote_reader.join().unwrap().unwrap_err().raw_os_error(),
            Some(libc::ENETDOWN)
        );
        session.umount_and_join().unwrap();
    }

    #[test]
    fn pending_remote_read_does_not_block_new_local_open() {
        let dir = tempfile::tempdir().unwrap();
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
        let fuse = Arc::new(VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            loader,
        ));
        let (local, local_handle) = fuse
            .create_file(ROOT_INODE, "local.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(local.inode, local_handle, 0, b"local")
            .unwrap();
        let remote = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        let remote_handle = fuse
            .open_file(remote.inode, OpenFlags(libc::O_RDWR))
            .unwrap();
        let remote_reader = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || fuse.read_file(remote.inode, remote_handle, 0, 5))
        };
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (local_tx, local_rx) = mpsc::channel();
        let local_reader = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || {
                let handle = fuse.open_file(local.inode, OpenFlags(libc::O_RDONLY));
                local_tx
                    .send(handle.and_then(|handle| fuse.read_file(local.inode, handle, 0, 5)))
                    .unwrap();
            })
        };
        let local_result = local_rx.recv_timeout(Duration::from_secs(2));
        let (replacement_tx, replacement_rx) = mpsc::channel();
        let replacement_writer = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || {
                let result = fuse
                    .open_file(remote.inode, OpenFlags(libc::O_WRONLY | libc::O_TRUNC))
                    .and_then(|handle| fuse.write_file(remote.inode, handle, 0, b"draft"));
                replacement_tx.send(result).unwrap();
            })
        };
        let replacement_result = replacement_rx.recv_timeout(Duration::from_secs(2));
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        assert_eq!(local_result.unwrap().unwrap(), b"local");
        assert_eq!(replacement_result.unwrap(), Ok(5));
        local_reader.join().unwrap();
        replacement_writer.join().unwrap();
        assert_eq!(remote_reader.join().unwrap(), Ok(b"draft".to_vec()));
    }

    #[test]
    fn pending_remote_write_does_not_block_new_local_open() {
        let dir = tempfile::tempdir().unwrap();
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
        let fuse = Arc::new(VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            loader,
        ));
        let (local, local_handle) = fuse
            .create_file(ROOT_INODE, "local.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(local.inode, local_handle, 0, b"local")
            .unwrap();
        let remote = fuse.lookup_node(ROOT_INODE, "текст.txt").unwrap();
        let remote_handle = fuse
            .open_file(remote.inode, OpenFlags(libc::O_RDWR))
            .unwrap();
        let remote_writer = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || fuse.write_file(remote.inode, remote_handle, 1, b"X"))
        };
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (local_tx, local_rx) = mpsc::channel();
        let local_reader = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || {
                let handle = fuse.open_file(local.inode, OpenFlags(libc::O_RDONLY));
                local_tx
                    .send(handle.and_then(|handle| fuse.read_file(local.inode, handle, 0, 5)))
                    .unwrap();
            })
        };
        let local_result = local_rx.recv_timeout(Duration::from_secs(2));
        let (replacement_tx, replacement_rx) = mpsc::channel();
        let replacement_writer = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || {
                let result = fuse
                    .open_file(remote.inode, OpenFlags(libc::O_WRONLY | libc::O_TRUNC))
                    .and_then(|handle| fuse.write_file(remote.inode, handle, 0, b"draft"));
                replacement_tx.send(result).unwrap();
            })
        };
        let replacement_result = replacement_rx.recv_timeout(Duration::from_secs(2));
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        assert_eq!(local_result.unwrap().unwrap(), b"local");
        assert_eq!(replacement_result.unwrap(), Ok(5));
        local_reader.join().unwrap();
        replacement_writer.join().unwrap();
        assert_eq!(remote_writer.join().unwrap(), Ok(1));
        assert_eq!(
            fuse.read_file(remote.inode, remote_handle, 0, 5).unwrap(),
            b"dXaft"
        );
    }

    #[test]
    fn blocked_local_write_does_not_block_other_open_file() {
        let dir = tempfile::tempdir().unwrap();
        let drafts = dir.path().join("drafts");
        let tree = VirtualTree::from_items("drive", "root", []).unwrap();
        let fuse = Arc::new(VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            drafts.clone(),
            10,
            Arc::new(|_| unreachable!()),
        ));
        let (blocked, blocked_handle) = fuse
            .create_file(ROOT_INODE, "blocked.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        let (other, other_handle) = fuse
            .create_file(ROOT_INODE, "other.txt", OpenFlags(libc::O_RDWR))
            .unwrap();
        fuse.write_file(blocked.inode, blocked_handle, 0, b"blocked")
            .unwrap();
        fuse.write_file(other.inode, other_handle, 0, b"ready")
            .unwrap();
        let blocked_path = fs::read_dir(&drafts)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "data")
                    && fs::read(path).unwrap() == b"blocked"
            })
            .unwrap();
        let lock_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(blocked_path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) },
            0
        );
        let (started_tx, started_rx) = mpsc::channel();
        let (write_tx, write_rx) = mpsc::channel();
        let writer = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                write_tx
                    .send(fuse.write_file(blocked.inode, blocked_handle, 0, b"updated"))
                    .unwrap();
            })
        };
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(write_rx.recv_timeout(Duration::from_millis(100)).is_err());
        let (read_tx, read_rx) = mpsc::channel();
        let reader = {
            let fuse = Arc::clone(&fuse);
            std::thread::spawn(move || {
                read_tx
                    .send(fuse.read_file(other.inode, other_handle, 0, 5))
                    .unwrap();
            })
        };
        let read_result = read_rx.recv_timeout(Duration::from_secs(2));
        assert_eq!(
            unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) },
            0
        );
        assert_eq!(read_result.unwrap().unwrap(), b"ready");
        assert_eq!(
            write_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(7)
        );
        writer.join().unwrap();
        reader.join().unwrap();
    }

    #[test]
    fn mounted_pending_write_keeps_local_open_responsive() {
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
        let session = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            loader,
        )
        .mount(&mountpoint)
        .unwrap();
        fs::write(mountpoint.join("local.txt"), b"local").unwrap();
        let remote = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(mountpoint.join("текст.txt"))
            .unwrap();
        let remote_writer = std::thread::spawn(move || remote.write_at(b"X", 1));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (local_tx, local_rx) = mpsc::channel();
        let local_path = mountpoint.join("local.txt");
        let local_reader = std::thread::spawn(move || {
            local_tx.send(fs::read(local_path)).unwrap();
        });
        let local_result = local_rx.recv_timeout(Duration::from_secs(2));
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        assert_eq!(local_result.unwrap().unwrap(), b"local");
        local_reader.join().unwrap();
        assert_eq!(
            remote_writer.join().unwrap().unwrap_err().raw_os_error(),
            Some(libc::ENETDOWN)
        );
        session.umount_and_join().unwrap();
    }

    #[test]
    fn mounted_pending_read_keeps_local_open_responsive() {
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
        let session = VirtualFuse::new_writable(
            tree,
            dir.path().join("cache"),
            dir.path().join("drafts"),
            10,
            loader,
        )
        .mount(&mountpoint)
        .unwrap();
        fs::write(mountpoint.join("local.txt"), b"local").unwrap();
        let remote_path = mountpoint.join("текст.txt");
        let remote_reader = std::thread::spawn(move || {
            let mut remote = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(remote_path)
                .unwrap();
            let mut bytes = [0_u8; 5];
            remote.read(&mut bytes)
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (local_tx, local_rx) = mpsc::channel();
        let local_path = mountpoint.join("local.txt");
        let local_reader = std::thread::spawn(move || {
            local_tx.send(fs::read(local_path)).unwrap();
        });
        let local_result = local_rx.recv_timeout(Duration::from_secs(2));
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        assert_eq!(local_result.unwrap().unwrap(), b"local");
        local_reader.join().unwrap();
        assert_eq!(
            remote_reader.join().unwrap().unwrap_err().raw_os_error(),
            Some(libc::ENETDOWN)
        );
        session.umount_and_join().unwrap();
    }
}
