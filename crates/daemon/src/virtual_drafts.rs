use onedrive_catalog::CatalogItem;
use onedrive_mount::draft::{DraftIdentity, DraftUploadIssue};
use serde::Serialize;

#[derive(Serialize)]
pub struct VirtualDraftStatus {
    pub drive_id: String,
    pub item_id: String,
    pub name: Option<String>,
    pub status: &'static str,
    pub last_upload_issue: Option<DraftUploadIssue>,
}

pub fn next_ready_item_id<'a>(
    statuses: &'a [VirtualDraftStatus],
    after: Option<&str>,
) -> Option<&'a str> {
    statuses
        .iter()
        .find(|draft| {
            draft.status == "ready" && after.is_some_and(|id| draft.item_id.as_str() > id)
        })
        .or_else(|| statuses.iter().find(|draft| draft.status == "ready"))
        .map(|draft| draft.item_id.as_str())
}

pub fn classify(
    draft: &DraftIdentity,
    active_drive: Option<&str>,
    catalog_complete: bool,
    item: Option<&CatalogItem>,
) -> VirtualDraftStatus {
    let status = if active_drive != Some(draft.drive_id.as_str()) {
        "other-drive"
    } else if !catalog_complete {
        "catalog-incomplete"
    } else {
        match item {
            None | Some(CatalogItem { deleted: true, .. }) => "remote-missing",
            Some(item) if item.kind != "file" => "remote-not-file",
            Some(item) if item.etag.as_deref() != Some(draft.base_etag.as_str()) => {
                "remote-changed"
            }
            Some(_) => "ready",
        }
    };
    VirtualDraftStatus {
        drive_id: draft.drive_id.clone(),
        item_id: draft.item_id.clone(),
        name: item.and_then(|item| item.name.clone()),
        status,
        last_upload_issue: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> DraftIdentity {
        DraftIdentity {
            drive_id: "drive".into(),
            item_id: "item".into(),
            base_etag: "old".into(),
            new_file: None,
        }
    }

    fn item() -> CatalogItem {
        CatalogItem {
            drive_id: "drive".into(),
            item_id: "item".into(),
            parent_id: None,
            name: Some("note.txt".into()),
            kind: "file".into(),
            size: Some(4),
            etag: Some("old".into()),
            ctag: None,
            modified: None,
            deleted: false,
        }
    }

    #[test]
    fn only_complete_matching_file_version_is_ready() {
        let draft = draft();
        let mut item = item();
        assert_eq!(
            classify(&draft, Some("drive"), true, Some(&item)).status,
            "ready"
        );
        assert_eq!(
            classify(&draft, None, true, Some(&item)).status,
            "other-drive"
        );
        assert_eq!(
            classify(&draft, Some("other"), true, Some(&item)).status,
            "other-drive"
        );
        assert_eq!(
            classify(&draft, Some("drive"), false, Some(&item)).status,
            "catalog-incomplete"
        );
        assert_eq!(
            classify(&draft, Some("drive"), true, None).status,
            "remote-missing"
        );
        item.deleted = true;
        assert_eq!(
            classify(&draft, Some("drive"), true, Some(&item)).status,
            "remote-missing"
        );
        item.deleted = false;
        item.kind = "folder".into();
        assert_eq!(
            classify(&draft, Some("drive"), true, Some(&item)).status,
            "remote-not-file"
        );
        item.kind = "file".into();
        item.etag = None;
        assert_eq!(
            classify(&draft, Some("drive"), true, Some(&item)).status,
            "remote-changed"
        );
        item.etag = Some("new".into());
        assert_eq!(
            classify(&draft, Some("drive"), true, Some(&item)).status,
            "remote-changed"
        );
    }

    #[test]
    fn automatic_candidate_skips_conflicts() {
        let draft = draft();
        let mut changed = item();
        changed.etag = Some("new".into());
        let statuses = [
            classify(&draft, Some("drive"), true, Some(&changed)),
            classify(&draft, Some("drive"), true, Some(&item())),
        ];
        assert_eq!(next_ready_item_id(&statuses, None), Some("item"));
        assert_eq!(next_ready_item_id(&statuses[..1], None), None);
        let mut other = item();
        other.item_id = "second".into();
        let statuses = [
            classify(&draft, Some("drive"), true, Some(&item())),
            classify(
                &DraftIdentity {
                    item_id: "second".into(),
                    ..draft
                },
                Some("drive"),
                true,
                Some(&other),
            ),
        ];
        assert_eq!(next_ready_item_id(&statuses, Some("item")), Some("second"));
        assert_eq!(next_ready_item_id(&statuses, Some("second")), Some("item"));
    }
}
