//! Read-only metadata view for a future FUSE adapter. No network calls or mounts happen here.

use onedrive_catalog::{Catalog, CatalogError, CatalogItem};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};

pub const ROOT_INODE: u64 = 1;

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
    nodes: HashMap<u64, Node>,
    children: HashMap<u64, BTreeMap<String, u64>>,
}

impl VirtualTree {
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
}
