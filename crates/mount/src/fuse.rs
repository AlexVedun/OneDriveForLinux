//! FUSE adapter around a validated catalog snapshot, versioned cache and local drafts.

use crate::directory::DirectoryStore;
use crate::draft::{DraftFile, DraftIdentity, DraftStore, NewFileLocation};
use crate::mutation::{Mutation, MutationKind, MutationStatus, MutationStore};
use crate::{
    DownloadRequest, FileHandle as CachedHandle, MAX_READ_BYTES, Node, NodeKind, OpenError,
    VirtualTree,
};
use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
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
    local_files: Mutex<HashMap<u64, Node>>,
    remote_changes: Mutex<HashMap<u64, Mutation>>,
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
            local_files: Mutex::new(HashMap::new()),
            remote_changes: Mutex::new(HashMap::new()),
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
        self.local_files
            .lock()
            .unwrap()
            .get(&inode)
            .cloned()
            .or_else(|| {
                self.tree
                    .getattr(inode)
                    .and_then(|node| self.remote_node(node))
            })
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
                        crate::stable_inode(self.tree.drive_id(), &target_parent_id)
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
        if let Some(node) = self
            .local_files
            .lock()
            .unwrap()
            .values()
            .find(|node| node.parent_inode == parent && node.name == name)
            .cloned()
        {
            return Some(node);
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
                    .and_then(|node| self.remote_node(node));
            }
        }
        self.tree
            .lookup(parent, name)
            .and_then(|node| self.remote_node(node))
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
            OpenHandle::Draft(draft),
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
        let parent = self.tree.getattr(node.parent_inode).ok_or(Errno::EIO)?;
        let mutation = Mutation {
            drive_id: self.tree.drive_id().to_owned(),
            item_id: node.item_id.clone(),
            parent_id: parent.item_id.clone(),
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
        let Some(root) = &self.draft_root else {
            return Err(Errno::EROFS);
        };
        let mut local = self.local_files.lock().unwrap();
        let node = local
            .values()
            .find(|node| node.parent_inode == parent && node.name == name)
            .cloned()
            .or_else(|| self.tree.lookup(parent, name).cloned())
            .ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::File {
            return Err(Errno::EISDIR);
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
        let Some(root) = &self.draft_root else {
            return Err(Errno::EROFS);
        };
        let mut local = self.local_files.lock().unwrap();
        let node = local
            .values()
            .find(|node| node.parent_inode == parent && node.name == name)
            .cloned()
            .or_else(|| self.tree.lookup(parent, name).cloned())
            .ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::Directory {
            return Err(Errno::ENOTDIR);
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
            .find(|entry| entry.parent_inode == new_parent && entry.name == new_name)
            .cloned()
            .or_else(|| {
                self.tree
                    .lookup(new_parent, new_name)
                    .and_then(|entry| self.remote_node(entry))
            });
        if let Some(target) = target
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
                .any(|open| open.inode == node.inode)
            {
                return Err(Errno::EBUSY);
            }
            if self.draft_for_node(&target)?.is_some()
                || self
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
            let request = self
                .tree
                .request_for_item(&target.item_id)
                .map_err(map_open_error)?;
            store
                .begin_from_file(
                    DraftIdentity {
                        drive_id: request.drive_id,
                        item_id: request.item_id,
                        base_etag: request.etag,
                        new_file: None,
                    },
                    snapshot.file.as_file(),
                )
                .map_err(map_draft_error)?;
            store
                .remove_new(self.tree.drive_id(), &node.item_id)
                .map_err(map_draft_error)?;
            let replacement = local.get_mut(&node.inode).ok_or(Errno::EIO)?;
            replacement.parent_inode = new_parent;
            replacement.name = new_name.to_owned();
            replacement.item_id = target.item_id;
            replacement.size = snapshot.size;
            replacement.etag = target.etag;
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
            if parent_node.item_id.starts_with("local-dir:") {
                return Err(Errno::EINVAL);
            }
            drop(local);
            return self.queue_remote_change(
                &node,
                MutationKind::Rename {
                    target_parent_id: parent_node.item_id,
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
        let local_entries: Vec<_> = self
            .local_files
            .lock()
            .unwrap()
            .values()
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
        let node = self.node(inode).ok_or(Errno::ENOENT)?;
        if node.kind != NodeKind::File {
            return Err(Errno::EISDIR);
        }
        if let Some(draft) = self.draft_for_node(&node)? {
            if writable && flags.0 & libc::O_TRUNC != 0 {
                draft.set_len(0).map_err(map_draft_error)?;
            }
            return Ok(self.insert_open(inode, OpenHandle::Draft(draft), writable));
        }
        if node.item_id.starts_with("local:") {
            return Err(Errno::EIO);
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
                    new_file: None,
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
                    new_file: None,
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
        if self.draft_root.is_some()
            && let Some(node) = self.node(inode)
            && let Some(draft) = self.draft_for_node(&node)?
        {
            return read_draft(&draft, offset, size);
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
        } else if mode.is_some() && self.draft_root.is_some() {
            // OneDrive has no Unix mode metadata; accept the copy tool's chmod.
            Ok(())
        } else {
            Err(Errno::EROFS)
        };
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
                fuser::RenameFlags::empty(),
            ),
            Err(Errno::EBUSY)
        );
        fuse.files.lock().unwrap().remove(&handle);
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
        directories
            .mark_uploaded(&local_dir, "remote-folder")
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
}
