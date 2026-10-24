use onedrive_catalog::CatalogItem;
use onedrive_mount::draft::{DraftIdentity, DraftUploadCheckpoint, DraftUploadIssue};
use serde::Serialize;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const RETRY_DELAY: Duration = Duration::from_secs(300);

#[derive(Default)]
pub struct RetrySchedule {
    next_attempt: HashMap<(String, String), Instant>,
}

impl RetrySchedule {
    pub fn allows(&self, draft: &VirtualDraftStatus, now: Instant) -> bool {
        self.next_attempt
            .get(&(draft.drive_id.clone(), draft.item_id.clone()))
            .is_none_or(|deadline| now >= *deadline)
    }

    pub fn defer(&mut self, draft: &DraftIdentity, now: Instant) {
        self.next_attempt.insert(
            (draft.drive_id.clone(), draft.item_id.clone()),
            now + RETRY_DELAY,
        );
    }

    pub fn forget(&mut self, draft: &DraftIdentity) {
        self.next_attempt
            .remove(&(draft.drive_id.clone(), draft.item_id.clone()));
    }

    pub fn prune(&mut self, now: Instant) {
        self.next_attempt.retain(|_, deadline| now < *deadline);
    }
}

#[derive(Clone, Serialize)]
pub struct VirtualDraftStatus {
    pub drive_id: String,
    pub item_id: String,
    pub name: Option<String>,
    pub status: &'static str,
    pub last_upload_issue: Option<DraftUploadIssue>,
    pub last_upload_http_status: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoDraftAction {
    Upload,
    Reconcile,
}

/// A lost final reply can survive a daemon crash without a failure record.
pub fn mark_uncertain_upload(
    status: &mut VirtualDraftStatus,
    draft: &DraftIdentity,
    checkpoint: Option<&DraftUploadCheckpoint>,
) {
    let Some(checkpoint) = checkpoint.filter(|checkpoint| checkpoint.final_chunk_attempted) else {
        return;
    };
    if draft.new_file.is_none() && status.status == "remote-changed" {
        status.last_upload_issue = Some(DraftUploadIssue::TransferFailed);
    } else if draft.new_file.is_some() && status.status == "name-conflict" {
        status.status = "uncertain-new";
        status.last_upload_issue = Some(DraftUploadIssue::TransferFailed);
    } else if checkpoint.size == 0 {
        match (draft.new_file.is_some(), status.status) {
            (false, "ready") => status.status = "uncertain-empty-existing",
            (true, "ready-new") => status.status = "uncertain-empty-new",
            _ => return,
        }
        status.last_upload_issue = Some(DraftUploadIssue::TransferFailed);
    } else if checkpoint.next_offset == checkpoint.size {
        match (draft.new_file.is_some(), status.status) {
            (false, "ready") => status.status = "uncertain-existing",
            (true, "ready-new") => status.status = "uncertain-new",
            _ => return,
        }
        status.last_upload_issue = Some(DraftUploadIssue::TransferFailed);
    }
}

pub fn next_auto_candidate<'a>(
    statuses: &'a [VirtualDraftStatus],
    after: Option<&str>,
) -> Option<(&'a VirtualDraftStatus, AutoDraftAction)> {
    statuses
        .iter()
        .filter_map(|draft| {
            let action = match draft.status {
                "ready" | "ready-new" => AutoDraftAction::Upload,
                "remote-changed" | "uncertain-empty-existing" | "uncertain-existing"
                    if draft.last_upload_issue == Some(DraftUploadIssue::TransferFailed) =>
                {
                    AutoDraftAction::Reconcile
                }
                _ => return None,
            };
            Some((draft, action))
        })
        .min_by_key(|(draft, _)| {
            (
                after.is_some_and(|id| draft.item_id.as_str() <= id),
                draft.item_id.as_str(),
            )
        })
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
        last_upload_http_status: None,
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
        assert_eq!(
            next_auto_candidate(&statuses, None)
                .map(|(draft, action)| (draft.item_id.as_str(), action)),
            Some(("item", AutoDraftAction::Upload))
        );
        assert!(next_auto_candidate(&statuses[..1], None).is_none());
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
        assert_eq!(
            next_auto_candidate(&statuses, Some("item")).map(|(draft, _)| draft.item_id.as_str()),
            Some("second")
        );
        assert_eq!(
            next_auto_candidate(&statuses, Some("second")).map(|(draft, _)| draft.item_id.as_str()),
            Some("item")
        );
        let mut new = statuses[1].clone();
        new.status = "ready-new";
        assert_eq!(
            next_auto_candidate(&[new], None).map(|(_, action)| action),
            Some(AutoDraftAction::Upload)
        );
    }

    #[test]
    fn automatic_reconcile_only_follows_uncertain_transfer() {
        let base = classify(&draft(), Some("drive"), true, Some(&item()));
        let mut changed = base.clone();
        changed.item_id = "changed".into();
        changed.status = "remote-changed";
        assert!(next_auto_candidate(&[changed.clone()], None).is_none());
        changed.last_upload_issue = Some(DraftUploadIssue::RemoteChanged);
        assert!(next_auto_candidate(&[changed.clone()], None).is_none());
        changed.last_upload_issue = Some(DraftUploadIssue::TransferFailed);
        assert_eq!(
            next_auto_candidate(&[changed.clone()], None).map(|(_, action)| action),
            Some(AutoDraftAction::Reconcile)
        );
        let statuses = [base, changed];
        assert_eq!(
            next_auto_candidate(&statuses, None)
                .map(|(draft, action)| (draft.item_id.as_str(), action)),
            Some(("changed", AutoDraftAction::Reconcile))
        );
        assert_eq!(
            next_auto_candidate(&statuses, Some("changed"))
                .map(|(draft, action)| (draft.item_id.as_str(), action)),
            Some(("item", AutoDraftAction::Upload))
        );
        assert_eq!(
            next_auto_candidate(&statuses, Some("item"))
                .map(|(draft, action)| (draft.item_id.as_str(), action)),
            Some(("changed", AutoDraftAction::Reconcile))
        );
    }

    #[test]
    fn failed_draft_waits_without_blocking_other_uploads() {
        let now = Instant::now();
        let failed = draft();
        let mut other = failed.clone();
        other.item_id = "other".into();
        let mut other_item = item();
        other_item.item_id = other.item_id.clone();
        let statuses = [
            classify(&failed, Some("drive"), true, Some(&item())),
            classify(&other, Some("drive"), true, Some(&other_item)),
        ];
        let mut schedule = RetrySchedule::default();
        schedule.defer(&failed, now);
        let available: Vec<_> = statuses
            .iter()
            .filter(|status| schedule.allows(status, now + Duration::from_secs(30)))
            .cloned()
            .collect();
        assert_eq!(
            next_auto_candidate(&available, None).map(|(draft, _)| draft.item_id.as_str()),
            Some("other")
        );
        assert!(!schedule.allows(&statuses[0], now + Duration::from_secs(299)));
        assert!(schedule.allows(&statuses[0], now + RETRY_DELAY));
        schedule.prune(now + RETRY_DELAY);
        assert!(schedule.next_attempt.is_empty());

        schedule.defer(&failed, now);
        schedule.forget(&failed);
        assert!(schedule.allows(&statuses[0], now));
    }

    #[test]
    fn final_chunk_marker_recovers_missing_failure_record() {
        let draft = draft();
        let mut remote = item();
        remote.etag = Some("new".into());
        let mut status = classify(&draft, Some("drive"), true, Some(&remote));
        let checkpoint = DraftUploadCheckpoint {
            identity: draft.clone(),
            sha256: "a".repeat(64),
            size: 4,
            upload_url: "https://upload.example/session".into(),
            next_offset: 0,
            final_chunk_attempted: true,
        };
        mark_uncertain_upload(&mut status, &draft, Some(&checkpoint));
        assert_eq!(
            status.last_upload_issue,
            Some(DraftUploadIssue::TransferFailed)
        );
        assert_eq!(
            next_auto_candidate(&[status.clone()], None).map(|(_, action)| action),
            Some(AutoDraftAction::Reconcile)
        );

        status.last_upload_issue = None;
        let mut unfinished = checkpoint.clone();
        unfinished.final_chunk_attempted = false;
        mark_uncertain_upload(&mut status, &draft, Some(&unfinished));
        assert!(status.last_upload_issue.is_none());

        let mut local_new = draft;
        local_new.item_id = "local:new".into();
        local_new.base_etag = "local-new".into();
        local_new.new_file = Some(onedrive_mount::draft::NewFileLocation {
            parent_id: "root".into(),
            name: "new.txt".into(),
        });
        mark_uncertain_upload(&mut status, &local_new, Some(&checkpoint));
        assert!(status.last_upload_issue.is_none());
    }

    #[test]
    fn empty_attempt_uses_reconciliation_for_existing_and_blocks_new_retry() {
        let draft = draft();
        let checkpoint = DraftUploadCheckpoint {
            identity: draft.clone(),
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            size: 0,
            upload_url: String::new(),
            next_offset: 0,
            final_chunk_attempted: true,
        };
        let mut existing = classify(&draft, Some("drive"), true, Some(&item()));
        mark_uncertain_upload(&mut existing, &draft, Some(&checkpoint));
        assert_eq!(existing.status, "uncertain-empty-existing");
        assert_eq!(
            next_auto_candidate(&[existing], None).map(|(_, action)| action),
            Some(AutoDraftAction::Reconcile)
        );

        let mut local_new = draft;
        local_new.item_id = "local:new".into();
        local_new.new_file = Some(onedrive_mount::draft::NewFileLocation {
            parent_id: "root".into(),
            name: "new.txt".into(),
        });
        let new_checkpoint = DraftUploadCheckpoint {
            identity: local_new.clone(),
            ..checkpoint
        };
        let mut new_status = VirtualDraftStatus {
            drive_id: "drive".into(),
            item_id: local_new.item_id.clone(),
            name: Some("new.txt".into()),
            status: "ready-new",
            last_upload_issue: None,
            last_upload_http_status: None,
        };
        mark_uncertain_upload(&mut new_status, &local_new, Some(&new_checkpoint));
        assert_eq!(new_status.status, "uncertain-empty-new");
        assert!(next_auto_candidate(&[new_status], None).is_none());
    }

    #[test]
    fn unavailable_final_session_routes_existing_to_reconcile_and_new_to_review() {
        let existing_identity = draft();
        let mut checkpoint = DraftUploadCheckpoint {
            identity: existing_identity.clone(),
            sha256: "a".repeat(64),
            size: 4,
            upload_url: "https://upload.example/session".into(),
            next_offset: 0,
            final_chunk_attempted: true,
        };
        let ready = classify(&existing_identity, Some("drive"), true, Some(&item()));
        let mut resumable = ready.clone();
        mark_uncertain_upload(&mut resumable, &existing_identity, Some(&checkpoint));
        assert_eq!(resumable.status, "ready");
        assert_eq!(
            next_auto_candidate(&[resumable], None).map(|(_, action)| action),
            Some(AutoDraftAction::Upload)
        );

        checkpoint.next_offset = checkpoint.size;
        let mut uncertain = ready;
        mark_uncertain_upload(&mut uncertain, &existing_identity, Some(&checkpoint));
        assert_eq!(uncertain.status, "uncertain-existing");
        assert_eq!(
            next_auto_candidate(&[uncertain], None).map(|(_, action)| action),
            Some(AutoDraftAction::Reconcile)
        );

        let mut new_identity = existing_identity;
        new_identity.item_id = "local:new".into();
        new_identity.base_etag = "local-new".into();
        new_identity.new_file = Some(onedrive_mount::draft::NewFileLocation {
            parent_id: "root".into(),
            name: "new.txt".into(),
        });
        checkpoint.identity = new_identity.clone();
        let mut new_status = VirtualDraftStatus {
            drive_id: "drive".into(),
            item_id: new_identity.item_id.clone(),
            name: Some("new.txt".into()),
            status: "ready-new",
            last_upload_issue: None,
            last_upload_http_status: None,
        };
        mark_uncertain_upload(&mut new_status, &new_identity, Some(&checkpoint));
        assert_eq!(new_status.status, "uncertain-new");
        assert!(next_auto_candidate(&[new_status], None).is_none());
    }

    #[test]
    fn new_file_name_conflict_with_final_attempt_requires_manual_review() {
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:new".into(),
            base_etag: "local-new".into(),
            new_file: Some(onedrive_mount::draft::NewFileLocation {
                parent_id: "root".into(),
                name: "new.txt".into(),
            }),
        };
        let checkpoint = DraftUploadCheckpoint {
            identity: identity.clone(),
            sha256: "a".repeat(64),
            size: 1,
            upload_url: "https://upload.example/session".into(),
            next_offset: 0,
            final_chunk_attempted: true,
        };
        let mut status = VirtualDraftStatus {
            drive_id: "drive".into(),
            item_id: identity.item_id.clone(),
            name: Some("new.txt".into()),
            status: "name-conflict",
            last_upload_issue: None,
            last_upload_http_status: None,
        };
        mark_uncertain_upload(&mut status, &identity, None);
        assert_eq!(status.status, "name-conflict");
        assert!(next_auto_candidate(&[status.clone()], None).is_none());

        mark_uncertain_upload(&mut status, &identity, Some(&checkpoint));
        assert_eq!(status.status, "uncertain-new");
        assert_eq!(
            status.last_upload_issue,
            Some(DraftUploadIssue::TransferFailed)
        );
        assert!(next_auto_candidate(&[status], None).is_none());
    }
}
