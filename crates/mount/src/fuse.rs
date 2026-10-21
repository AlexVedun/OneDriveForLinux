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
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLseek, ReplyOpen,
    ReplyWrite, Request,
};
use onedrive_cache::CacheStore;
use std::collections::{HashMap, HashSet};
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
    operations: Arc<Mutex<PublicationOperations>>,
}

impl Drop for PublicationLease {
    fn drop(&mut self) {
        self.operations.lock().unwrap().active.remove(&self.inode);
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
        if new_file
            && self
                .local_files
                .lock()
                .unwrap()
                .get(&inode)
                .is_none_or(|node| node.item_id != item_id || node.kind != NodeKind::File)
        {
            return Err(io::ErrorKind::NotFound.into());
        }
        let files = self.files.lock().unwrap();
        let mut operations = self.operations.lock().unwrap();
        if files.values().any(|open| open.inode == inode)
            || operations.opening.contains_key(&inode)
            || !operations.active.insert(inode)
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(PublicationLease {
            inode,
            operations: Arc::clone(&self.operations),
        })
    }

    /// Make a verified upload visible to an already mounted filesystem.
    pub fn publish_existing(&self, item_id: &str, etag: String, size: u64) {
        let inode = crate::stable_inode(&self.drive_id, item_id);
        self.versions.lock().unwrap().insert(inode, (etag, size));
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
    writable: bool,
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
    local_files: Arc<Mutex<HashMap<u64, Node>>>,
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
            local_files: Arc::new(Mutex::new(HashMap::new())),
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
        let directory = node.kind == NodeKind::Directory;
        let draft_attributes = if directory {
            None
        } else {
            self.draft_for_node(node)?
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

    fn node(&self, inode: u64) -> Option<Node> {
        let local = self.local_files.lock().unwrap().get(&inode).cloned();
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
            .ok_or(Errno::EINVAL)
    }

    fn remote_node(&self, node: &Node) -> Option<Node> {
        let mutation = self
            .remote_changes
            .lock()
            .unwrap()
            .get(&node.inode)
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

    fn remote_target_taken(&self, parent_id: &str, name: &str) -> bool {
        self.remote_changes
            .lock()
            .unwrap()
            .values()
            .any(|mutation| {
                matches!(&mutation.kind, MutationKind::Rename { target_parent_id, target_name }
                if target_parent_id == parent_id && target_name == name)
            })
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
                && self
                    .node(parent)
                    .is_some_and(|node| node.item_id == *target_parent_id)
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
                    let name = if self.tree.lookup(parent.inode, &directory.name).is_some()
                        || local.values().any(|node| {
                            node.parent_inode == parent.inode && node.name == directory.name
                        }) {
                        format!(".conflict-{inode}-{}", directory.name)
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
                    local.insert(
                        inode,
                        Node {
                            inode,
                            parent_inode: crate::ROOT_INODE,
                            item_id: directory.item_id,
                            name: format!(".orphan-{inode}-{}", directory.name),
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
            let Some(location) = &identity.new_file else {
                continue;
            };
            if identity.drive_id != self.tree.drive_id() {
                continue;
            }
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
                || self.tree.lookup(parent.inode, &visible_name).is_some()
                || local
                    .values()
                    .any(|node| node.parent_inode == parent.inode && node.name == visible_name)
            {
                let short_id = identity
                    .item_id
                    .strip_prefix("local:")
                    .unwrap_or(&identity.item_id);
                let prefix = if orphaned { "orphan" } else { "conflict" };
                visible_name = format!(
                    ".{prefix}-{short_id}-{}",
                    location.name.chars().take(120).collect::<String>()
                );
            }
            let inode = crate::stable_inode(&identity.drive_id, &identity.item_id);
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
        let parent_node = self.node(parent).ok_or(Errno::ENOENT)?;
        if parent_node.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        let mut local = self.local_files.lock().unwrap();
        if self.tree.lookup(parent, name).is_some()
            || self.remote_target_taken(&parent_node.item_id, name)
            || local
                .values()
                .any(|node| node.parent_inode == parent && node.name == name)
        {
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
        let handle = self.insert_open(
            inode,
            OpenHandle::Draft(Arc::new(draft)),
            flags.acc_mode() != OpenAccMode::O_RDONLY,
        );
        Ok((node, handle))
    }

    fn queue_remote_change(&self, node: &Node, kind: MutationKind) -> Result<(), Errno> {
        let Some(root) = &self.draft_root else {
            return Err(Errno::EROFS);
        };
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
        if changes.contains_key(&node.inode) {
            return Err(Errno::EBUSY);
        }
        DraftStore::open(root).map_err(map_draft_error)?;
        MutationStore::open(root)
            .and_then(|store| store.add(&mutation))
            .map_err(map_draft_error)?;
        changes.insert(node.inode, mutation);
        Ok(())
    }

    fn remove_local_file(&self, parent: u64, name: &str) -> Result<(), Errno> {
        let _serialized = self.open_lock.lock().unwrap();
        let Some(root) = &self.draft_root else {
            return Err(Errno::EROFS);
        };
        let mut local = self.local_files.lock().unwrap();
        let node = local
            .values()
            .filter_map(|node| self.remote_node(node))
            .find(|node| node.parent_inode == parent && node.name == name)
            .or_else(|| self.tree.lookup(parent, name).cloned())
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
        if self
            .files
            .lock()
            .unwrap()
            .values()
            .any(|open| open.inode == node.inode)
        {
            return Err(Errno::EBUSY);
        }
        DraftStore::open(root)
            .and_then(|store| store.remove_new(self.tree.drive_id(), &node.item_id))
            .map_err(map_draft_error)?;
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
        let mut local = self.local_files.lock().unwrap();
        let parent_node = local
            .get(&parent)
            .or_else(|| self.tree.getattr(parent))
            .ok_or(Errno::ENOENT)?;
        if parent_node.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        if self.tree.lookup(parent, name).is_some()
            || self.remote_target_taken(&parent_node.item_id, name)
            || local
                .values()
                .any(|node| node.parent_inode == parent && node.name == name)
        {
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
        let mut local = self.local_files.lock().unwrap();
        let node = local
            .values()
            .filter_map(|node| self.remote_node(node))
            .find(|node| node.parent_inode == parent && node.name == name)
            .or_else(|| self.tree.lookup(parent, name).cloned())
            .ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        if self.is_publishing(node.inode) {
            return Err(Errno::EBUSY);
        }
        if local.values().any(|child| child.parent_inode == node.inode)
            || self.tree.readdir(node.inode).is_some_and(|children| {
                children.into_iter().any(|child| {
                    self.remote_node(child)
                        .is_some_and(|visible| visible.parent_inode == node.inode)
                })
            })
        {
            return Err(Errno::ENOTEMPTY);
        }
        if !node.item_id.starts_with("local-dir:") {
            drop(local);
            return self.queue_remote_change(&node, MutationKind::Delete);
        }
        let store = DirectoryStore::open(root).map_err(map_draft_error)?;
        let directory = store
            .list()
            .map_err(map_draft_error)?
            .into_iter()
            .find(|directory| directory.item_id == node.item_id)
            .ok_or(Errno::EIO)?;
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
        let node = local
            .values()
            .find(|node| node.parent_inode == parent && node.name == name)
            .cloned()
            .or_else(|| self.tree.lookup(parent, name).cloned())
            .ok_or(Errno::ENOENT)?;
        if parent == new_parent && name == new_name {
            return Ok(());
        }
        let target = local
            .values()
            .filter_map(|entry| self.remote_node(entry))
            .find(|entry| entry.parent_inode == new_parent && entry.name == new_name)
            .or_else(|| {
                self.tree
                    .lookup(new_parent, new_name)
                    .and_then(|entry| self.remote_node(entry))
            });
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
                .remote_changes
                .lock()
                .unwrap()
                .contains_key(&target.inode)
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
            if self
                .files
                .lock()
                .unwrap()
                .values()
                .any(|open| open.inode == node.inode || open.inode == target.inode)
            {
                return Err(Errno::EBUSY);
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
            if source.remote_id.is_some() || destination.remote_id.is_some() {
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
        if self.tree.lookup(new_parent, new_name).is_some()
            || self.remote_target_taken(&parent_node.item_id, new_name)
            || local
                .values()
                .any(|node| node.parent_inode == new_parent && node.name == new_name)
        {
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

    fn directory_entries(&self, inode: u64) -> Option<Vec<(u64, FileType, String)>> {
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
                && directory.item_id == *target_parent_id
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
            return Ok(self.insert_open(inode, OpenHandle::Draft(Arc::new(draft)), writable));
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
            return Ok(self.insert_open(inode, OpenHandle::Draft(Arc::new(draft)), true));
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
                true,
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
            return Ok(self.insert_open(inode, OpenHandle::Draft(Arc::new(draft)), false));
        }
        Ok(self.insert_open(inode, OpenHandle::Cached(Arc::new(opened)), false))
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
        let opened = self
            .files
            .lock()
            .unwrap()
            .get(&handle)
            .filter(|open| open.inode == inode)
            .ok_or(Errno::EBADF)?
            .handle
            .clone();
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
        drop(files);
        let written = draft.write_at(offset, data).map_err(map_draft_error)?;
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
        let files = self.files.lock().unwrap();
        if files.get(&handle).is_none_or(|open| open.inode != inode) {
            return Err(Errno::EBADF);
        }
        let node = self.node(inode).ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::File {
            return Err(Errno::EISDIR);
        }
        if let Some(draft) = self.draft_for_node(&node)? {
            return draft.seek_data_hole(offset, whence).map_err(|error| {
                match error.raw_os_error() {
                    Some(libc::ENXIO) => Errno::ENXIO,
                    Some(libc::EINVAL) => Errno::EINVAL,
                    _ => Errno::EIO,
                }
            });
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
                .is_none_or(|open| open.inode != source_inode)
                || files
                    .get(&target_handle)
                    .is_none_or(|open| open.inode != target_inode || !open.writable)
            {
                return Err(Errno::EBADF);
            }
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
            let draft = self.draft_for_node(&node)?.ok_or(Errno::EROFS)?;
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
        assert_eq!(
            fuse.remove_local_file(ROOT_INODE, "second.txt"),
            Err(Errno::EBUSY)
        );
        fuse.files.lock().unwrap().remove(&handle);
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
    fn editor_style_rename_replaces_remote_file_with_durable_draft() {
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
        fuse.files.lock().unwrap().remove(&handle);
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
        let mut bytes = [0; 11];
        assert_eq!(draft.read_at(0, &mut bytes).unwrap(), 11);
        assert_eq!(&bytes, b"replacement");
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
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EBUSY)
        );
        fuse.files.lock().unwrap().clear();
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
        let mut bytes = [0; 3];
        assert_eq!(draft.read_at(0, &mut bytes).unwrap(), 3);
        assert_eq!(&bytes, b"new");
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
        fs::rename(mountpoint.join("source.txt"), mountpoint.join("target.txt")).unwrap();
        assert_eq!(fs::read(mountpoint.join("target.txt")).unwrap(), b"new");
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
            fuse.read_file(created.inode, handle, 0, 4).unwrap(),
            b"data"
        );
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
