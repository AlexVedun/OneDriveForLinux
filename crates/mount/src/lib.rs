//! Read-only metadata view for a future FUSE adapter. No network calls or mounts happen here.

pub mod directory;
pub mod draft;
pub mod fuse;
pub mod preflight;

use onedrive_cache::{CacheError, CacheKey, CacheStore};
use onedrive_catalog::{Catalog, CatalogError, CatalogItem};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

pub const ROOT_INODE: u64 = 1;
pub const MAX_READ_BYTES: u32 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadRequest {
    pub drive_id: String,
    pub item_id: String,
    pub etag: String,
    pub size: u64,
}

impl DownloadRequest {
    pub fn cache_key(&self) -> CacheKey {
        CacheKey::new(&self.drive_id, &self.item_id, &self.etag)
            .expect("validated download request")
    }
}

#[derive(Debug)]
pub enum OpenError {
    MissingFile,
    IsDirectory,
    MissingVersion,
    NotCached(DownloadRequest),
    Cache(CacheError),
    SizeMismatch,
    Io(io::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingFile => write!(f, "file is not in the virtual tree"),
            Self::IsDirectory => write!(f, "cannot open a directory as a file"),
            Self::MissingVersion => write!(f, "file has no eTag in the catalog"),
            Self::NotCached(_) => write!(f, "file version is not cached"),
            Self::Cache(error) => write!(f, "cache error: {error}"),
            Self::SizeMismatch => write!(f, "cached file size differs from catalog"),
            Self::Io(error) => write!(f, "cached file I/O error: {error}"),
        }
    }
}

impl std::error::Error for OpenError {}

pub struct FileHandle {
    file: File,
    size: u64,
}

impl FileHandle {
    pub fn source_file(&self) -> &File {
        &self.file
    }

    pub fn read(&self, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        if size > MAX_READ_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read exceeds limit",
            ));
        }
        let count = usize::try_from(self.size.saturating_sub(offset).min(u64::from(size)))
            .expect("bounded by MAX_READ_BYTES");
        let mut bytes = vec![0; count];
        let mut position = 0;
        while position < count {
            let read = self
                .file
                .read_at(&mut bytes[position..], offset + position as u64)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "cached file was truncated",
                ));
            }
            position += read;
        }
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Directory,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub inode: u64,
    pub parent_inode: u64,
    pub item_id: String,
    pub name: String,
    pub kind: NodeKind,
    pub size: u64,
    pub etag: Option<String>,
    pub ctag: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TreeError {
    EmptyIdentity,
    WrongDrive(String),
    DuplicateItem(String),
    InvalidName(String),
    InvalidKind(String),
    InvalidSize(String),
    InvalidParent(String),
    DuplicateName(String),
    Unreachable(String),
    InodeCollision(String),
}

impl std::fmt::Display for TreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid virtual tree: {self:?}")
    }
}

impl std::error::Error for TreeError {}

#[derive(Debug)]
pub enum SnapshotError {
    Catalog(CatalogError),
    NoActiveDrive,
    IncompleteCatalog,
    NoRoot,
    InvalidTree(TreeError),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Catalog(error) => write!(f, "cannot read catalog: {error}"),
            Self::NoActiveDrive => write!(f, "no active drive is selected"),
            Self::IncompleteCatalog => write!(f, "catalog enumeration is incomplete"),
            Self::NoRoot => write!(f, "active drive has no root item"),
            Self::InvalidTree(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<CatalogError> for SnapshotError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(error)
    }
}

pub struct VirtualTree {
    drive_id: String,
    nodes: HashMap<u64, Node>,
    children: HashMap<u64, BTreeMap<String, u64>>,
}

impl VirtualTree {
    pub fn drive_id(&self) -> &str {
        &self.drive_id
    }

    pub fn request_for_item(&self, item_id: &str) -> Result<DownloadRequest, OpenError> {
        let inode = if self.nodes[&ROOT_INODE].item_id == item_id {
            ROOT_INODE
        } else {
            stable_inode(&self.drive_id, item_id)
        };
        if self
            .getattr(inode)
            .is_none_or(|node| node.item_id != item_id)
        {
            return Err(OpenError::MissingFile);
        }
        self.request_for_inode(inode)
    }

    fn request_for_inode(&self, inode: u64) -> Result<DownloadRequest, OpenError> {
        let node = self.getattr(inode).ok_or(OpenError::MissingFile)?;
        if node.kind != NodeKind::File {
            return Err(OpenError::IsDirectory);
        }
        let etag = node
            .etag
            .as_deref()
            .filter(|etag| !etag.is_empty())
            .ok_or(OpenError::MissingVersion)?;
        Ok(DownloadRequest {
            drive_id: self.drive_id.clone(),
            item_id: node.item_id.clone(),
            etag: etag.to_owned(),
            size: node.size,
        })
    }

    pub fn open_cached(&self, inode: u64, cache: &mut CacheStore) -> Result<FileHandle, OpenError> {
        let request = self.request_for_inode(inode)?;
        let size = request.size;
        let key = request.cache_key();
        let file = cache
            .get(&key)
            .map_err(OpenError::Cache)?
            .ok_or(OpenError::NotCached(request))?;
        if file.metadata().map_err(OpenError::Io)?.len() != size {
            return Err(OpenError::SizeMismatch);
        }
        Ok(FileHandle { file, size })
    }

    /// Takes one validated snapshot. The caller must rebuild it after catalog changes.
    pub fn from_catalog(catalog: &Catalog) -> Result<(String, Self), SnapshotError> {
        let drive_id = catalog
            .active_drive_id()?
            .ok_or(SnapshotError::NoActiveDrive)?;
        if !catalog.is_complete(&drive_id)? {
            return Err(SnapshotError::IncompleteCatalog);
        }
        let root_id = catalog.root_id(&drive_id)?.ok_or(SnapshotError::NoRoot)?;
        let items = catalog.items(&drive_id)?;
        let tree =
            Self::from_items(&drive_id, &root_id, items).map_err(SnapshotError::InvalidTree)?;
        Ok((drive_id, tree))
    }

    pub fn from_items(
        drive_id: &str,
        root_id: &str,
        items: impl IntoIterator<Item = CatalogItem>,
    ) -> Result<Self, TreeError> {
        if drive_id.is_empty() || root_id.is_empty() {
            return Err(TreeError::EmptyIdentity);
        }

        let mut active = HashMap::new();
        for item in items {
            if item.drive_id != drive_id {
                return Err(TreeError::WrongDrive(item.item_id));
            }
            if item.deleted {
                continue;
            }
            if item.item_id.is_empty() {
                return Err(TreeError::EmptyIdentity);
            }
            let item_id = item.item_id.clone();
            if active.insert(item_id.clone(), item).is_some() {
                return Err(TreeError::DuplicateItem(item_id));
            }
        }
        // The catalog may omit the root item; root_id comes from drive_root.
        active.remove(root_id);

        let mut descendants: HashMap<String, Vec<String>> = HashMap::new();
        for item in active.values() {
            let parent = item
                .parent_id
                .as_ref()
                .ok_or_else(|| TreeError::InvalidParent(item.item_id.clone()))?;
            descendants
                .entry(parent.clone())
                .or_default()
                .push(item.item_id.clone());
        }
        let root = Node {
            inode: ROOT_INODE,
            parent_inode: ROOT_INODE,
            item_id: root_id.to_owned(),
            name: String::new(),
            kind: NodeKind::Directory,
            size: 0,
            etag: None,
            ctag: None,
        };
        let mut tree = Self {
            drive_id: drive_id.to_owned(),
            nodes: HashMap::from([(ROOT_INODE, root)]),
            children: HashMap::new(),
        };
        let mut id_to_inode = HashMap::from([(root_id.to_owned(), ROOT_INODE)]);
        let mut queue = VecDeque::from([root_id.to_owned()]);

        while let Some(parent_id) = queue.pop_front() {
            let parent_inode = id_to_inode[&parent_id];
            let mut child_ids = descendants.remove(&parent_id).unwrap_or_default();
            child_ids.sort();
            if !child_ids.is_empty() && tree.nodes[&parent_inode].kind != NodeKind::Directory {
                return Err(TreeError::InvalidParent(parent_id));
            }
            for child_id in child_ids {
                let item = &active[&child_id];
                let name = item
                    .name
                    .as_ref()
                    .ok_or_else(|| TreeError::InvalidName(child_id.clone()))?;
                if name.is_empty()
                    || name == "."
                    || name == ".."
                    || name.contains('/')
                    || name.contains('\0')
                {
                    return Err(TreeError::InvalidName(child_id));
                }
                let kind = match item.kind.as_str() {
                    "folder" => NodeKind::Directory,
                    "file" => NodeKind::File,
                    _ => return Err(TreeError::InvalidKind(child_id)),
                };
                let size = match kind {
                    NodeKind::Directory => 0,
                    NodeKind::File => item
                        .size
                        .and_then(|size| u64::try_from(size).ok())
                        .ok_or_else(|| TreeError::InvalidSize(child_id.clone()))?,
                };
                let siblings = tree.children.entry(parent_inode).or_default();
                if siblings.contains_key(name) {
                    return Err(TreeError::DuplicateName(name.clone()));
                }
                let inode = stable_inode(drive_id, &child_id);
                if tree.nodes.contains_key(&inode) {
                    return Err(TreeError::InodeCollision(child_id));
                }
                let node = Node {
                    inode,
                    parent_inode,
                    item_id: child_id.clone(),
                    name: name.clone(),
                    kind,
                    size,
                    etag: item.etag.clone(),
                    ctag: item.ctag.clone(),
                };
                siblings.insert(name.clone(), inode);
                tree.nodes.insert(inode, node);
                id_to_inode.insert(child_id.clone(), inode);
                queue.push_back(child_id);
            }
        }
        if id_to_inode.len() != active.len() + 1 {
            let mut missing: Vec<_> = active
                .keys()
                .filter(|id| !id_to_inode.contains_key(*id))
                .collect();
            missing.sort();
            return Err(TreeError::Unreachable((*missing[0]).clone()));
        }
        Ok(tree)
    }

    pub fn getattr(&self, inode: u64) -> Option<&Node> {
        self.nodes.get(&inode)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<&Node> {
        let directory = self.getattr(parent)?;
        if directory.kind != NodeKind::Directory {
            return None;
        }
        let inode = match name {
            "." => parent,
            ".." => directory.parent_inode,
            _ => *self.children.get(&parent)?.get(name)?,
        };
        self.getattr(inode)
    }

    pub fn readdir(&self, parent: u64) -> Option<Vec<&Node>> {
        if self.getattr(parent)?.kind != NodeKind::Directory {
            return None;
        }
        Some(
            self.children
                .get(&parent)
                .into_iter()
                .flat_map(BTreeMap::values)
                .map(|inode| &self.nodes[inode])
                .collect(),
        )
    }
}

fn stable_inode(drive_id: &str, item_id: &str) -> u64 {
    let mut hash = Sha256::new();
    for part in [drive_id, item_id] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    let digest = hash.finalize();
    u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 has eight bytes")).max(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_catalog::DeltaPage;
    use std::fs;

    fn item(id: &str, parent: &str, name: &str, kind: &str) -> CatalogItem {
        CatalogItem {
            drive_id: "drive".into(),
            item_id: id.into(),
            parent_id: Some(parent.into()),
            name: Some(name.into()),
            kind: kind.into(),
            size: Some(5),
            etag: Some("v1".into()),
            ctag: None,
            modified: None,
            deleted: false,
        }
    }

    #[test]
    fn nested_lookup_and_sorted_directory_listing() {
        let tree = VirtualTree::from_items(
            "drive",
            "root",
            [
                item("file", "folder", "текст.txt", "file"),
                item("folder", "root", "Каталог", "folder"),
                item("alpha", "root", "Alpha", "file"),
            ],
        )
        .unwrap();
        let root_names: Vec<_> = tree
            .readdir(ROOT_INODE)
            .unwrap()
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(root_names, ["Alpha", "Каталог"]);
        let folder = tree.lookup(ROOT_INODE, "Каталог").unwrap();
        assert_eq!(folder.size, 0);
        assert_eq!(tree.lookup(folder.inode, "текст.txt").unwrap().size, 5);
        assert_eq!(tree.lookup(folder.inode, "..").unwrap().inode, ROOT_INODE);
        assert_eq!(tree.lookup(ROOT_INODE, "..").unwrap().inode, ROOT_INODE);
        assert!(
            tree.readdir(tree.lookup(ROOT_INODE, "Alpha").unwrap().inode)
                .is_none()
        );
    }

    #[test]
    fn inode_survives_rename_version_change_and_input_order() {
        let first =
            VirtualTree::from_items("drive", "root", [item("a", "root", "old", "file")]).unwrap();
        let mut renamed = item("a", "root", "new", "file");
        renamed.etag = Some("v2".into());
        let second = VirtualTree::from_items("drive", "root", [renamed]).unwrap();
        assert_eq!(
            first.lookup(ROOT_INODE, "old").unwrap().inode,
            second.lookup(ROOT_INODE, "new").unwrap().inode
        );
        assert_eq!(
            second.lookup(ROOT_INODE, "new").unwrap().etag.as_deref(),
            Some("v2")
        );
    }

    #[test]
    fn rejects_ambiguous_and_broken_hierarchies() {
        assert_eq!(
            VirtualTree::from_items(
                "drive",
                "root",
                [
                    item("a", "root", "same", "file"),
                    item("b", "root", "same", "file")
                ]
            )
            .err(),
            Some(TreeError::DuplicateName("same".into()))
        );
        assert_eq!(
            VirtualTree::from_items("drive", "root", [item("a", "missing", "a", "file")]).err(),
            Some(TreeError::Unreachable("a".into()))
        );
        assert_eq!(
            VirtualTree::from_items(
                "drive",
                "root",
                [item("a", "b", "a", "folder"), item("b", "a", "b", "folder")]
            )
            .err(),
            Some(TreeError::Unreachable("a".into()))
        );
        assert_eq!(
            VirtualTree::from_items(
                "drive",
                "root",
                [item("a", "root", "a", "file"), item("b", "a", "b", "file")]
            )
            .err(),
            Some(TreeError::InvalidParent("a".into()))
        );
    }

    #[test]
    fn rejects_invalid_metadata_and_ignores_tombstones() {
        let mut deleted = item("gone", "root", "gone", "file");
        deleted.deleted = true;
        assert!(
            VirtualTree::from_items("drive", "root", [deleted])
                .unwrap()
                .readdir(ROOT_INODE)
                .unwrap()
                .is_empty()
        );
        let mut invalid = item("bad", "root", "../bad", "file");
        assert_eq!(
            VirtualTree::from_items("drive", "root", [invalid.clone()]).err(),
            Some(TreeError::InvalidName("bad".into()))
        );
        invalid.name = Some("bad".into());
        invalid.size = Some(-1);
        assert_eq!(
            VirtualTree::from_items("drive", "root", [invalid]).err(),
            Some(TreeError::InvalidSize("bad".into()))
        );
    }

    #[test]
    fn rejects_duplicate_ids_and_wrong_drive() {
        let duplicated = item("same", "root", "one", "file");
        assert_eq!(
            VirtualTree::from_items("drive", "root", [duplicated.clone(), duplicated]).err(),
            Some(TreeError::DuplicateItem("same".into()))
        );
        let mut other_drive = item("foreign", "root", "name", "file");
        other_drive.drive_id = "other".into();
        assert_eq!(
            VirtualTree::from_items("drive", "root", [other_drive]).err(),
            Some(TreeError::WrongDrive("foreign".into()))
        );
    }

    #[test]
    fn snapshot_requires_complete_catalog_and_root() {
        let mut catalog = Catalog::in_memory().unwrap();
        assert!(matches!(
            VirtualTree::from_catalog(&catalog),
            Err(SnapshotError::NoActiveDrive)
        ));
        catalog.set_active_drive_id("drive").unwrap();
        assert!(matches!(
            VirtualTree::from_catalog(&catalog),
            Err(SnapshotError::IncompleteCatalog)
        ));
        let page: DeltaPage = serde_json::from_str(
            r#"{"value":[{"id":"file","parentReference":{"id":"root"},"name":"file.txt","file":{},"size":5,"eTag":"v1"}],"@odata.deltaLink":"https://graph.microsoft.com/v1.0/me/drive/root/delta?token=done"}"#,
        ).unwrap();
        catalog.apply_page("drive", &page).unwrap();
        assert!(matches!(
            VirtualTree::from_catalog(&catalog),
            Err(SnapshotError::NoRoot)
        ));
        catalog.set_root_id("drive", "root").unwrap();
        let (drive, tree) = VirtualTree::from_catalog(&catalog).unwrap();
        assert_eq!(drive, "drive");
        assert_eq!(tree.lookup(ROOT_INODE, "file.txt").unwrap().size, 5);
        catalog.reset_expired_cursor("drive").unwrap();
        assert!(matches!(
            VirtualTree::from_catalog(&catalog),
            Err(SnapshotError::IncompleteCatalog)
        ));
    }

    #[test]
    fn cached_file_reads_at_offsets_and_requires_matching_version() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, b"hello").unwrap();
        let mut cache = CacheStore::open(&directory.path().join("cache"), 20).unwrap();
        let key = CacheKey::new("drive", "file", "v1").unwrap();
        cache.put(&key, &source, 5).unwrap();
        let tree =
            VirtualTree::from_items("drive", "root", [item("file", "root", "hello", "file")])
                .unwrap();
        let inode = tree.lookup(ROOT_INODE, "hello").unwrap().inode;
        let handle = tree.open_cached(inode, &mut cache).unwrap();
        assert_eq!(handle.read(1, 3).unwrap(), b"ell");
        assert_eq!(handle.read(4, 20).unwrap(), b"o");
        assert!(handle.read(5, 1).unwrap().is_empty());
        assert_eq!(
            handle.read(0, MAX_READ_BYTES + 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let mut changed = item("file", "root", "hello", "file");
        changed.etag = Some("v2".into());
        let tree = VirtualTree::from_items("drive", "root", [changed]).unwrap();
        match tree.open_cached(inode, &mut cache) {
            Err(OpenError::NotCached(request)) => {
                assert_eq!(request.item_id, "file");
                assert_eq!(request.etag, "v2");
                assert_eq!(request.size, 5);
            }
            _ => panic!("changed version must miss the cache"),
        }
        assert!(matches!(
            tree.open_cached(ROOT_INODE, &mut cache),
            Err(OpenError::IsDirectory)
        ));
        assert_eq!(tree.request_for_item("file").unwrap().etag, "v2");
        assert!(matches!(
            tree.request_for_item("missing"),
            Err(OpenError::MissingFile)
        ));
    }
}
