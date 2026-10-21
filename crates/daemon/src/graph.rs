//! Graph v1.0 transport. URLs from delta are checked before attaching a
//! bearer token; preauthenticated transfer URLs never receive one.

use onedrive_catalog::{DeltaPage, FetchError, validate_graph_url};
use onedrive_mount::DownloadRequest;
use onedrive_sync::QueuedOperation;
use onedrive_sync::upload::{UploadError, UploadStep, UploadTransport};
use reqwest::{
    Client, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, IF_MATCH, RETRY_AFTER},
};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

use crate::transfer::AtomicDownload;

const DRIVE_URL: &str = "https://graph.microsoft.com/v1.0/me/drive";
const ROOT_URL: &str = "https://graph.microsoft.com/v1.0/me/drive/root?$select=id";

#[derive(Clone)]
pub struct GraphClient {
    http: Client,
}

#[derive(Deserialize)]
struct Drive {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    upload_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionStatus {
    next_expected_ranges: Vec<String>,
}

#[derive(Deserialize)]
struct UploadedItem {
    id: String,
    #[serde(rename = "eTag")]
    etag: Option<String>,
}

#[derive(Deserialize)]
struct CreatedVirtualFile {
    id: String,
    name: String,
    size: u64,
    #[serde(rename = "eTag")]
    etag: Option<String>,
    file: Option<serde_json::Value>,
    #[serde(rename = "parentReference")]
    parent_reference: Option<FolderParent>,
}

pub struct CreatedVirtualExpectation<'a> {
    pub item_id: &'a str,
    pub parent_id: &'a str,
    pub name: &'a str,
    pub etag: &'a str,
    pub size: u64,
}

fn created_virtual_file_matches(
    item: &CreatedVirtualFile,
    id: &str,
    parent: &str,
    name: &str,
    etag: &str,
    size: u64,
) -> bool {
    item.id == id
        && item.name == name
        && item.size == size
        && item.etag.as_deref() == Some(etag)
        && item.file.is_some()
        && item
            .parent_reference
            .as_ref()
            .and_then(|value| value.id.as_deref())
            == Some(parent)
}

#[derive(Deserialize)]
struct UploadedEmptyItem {
    id: String,
    #[serde(rename = "eTag")]
    etag: Option<String>,
    size: Option<u64>,
    file: Option<serde_json::Value>,
}

fn empty_upload_matches(item: &UploadedEmptyItem, expected_id: &str) -> bool {
    item.id == expected_id
        && item.etag.as_ref().is_some_and(|etag| !etag.is_empty())
        && item.size == Some(0)
        && item.file.is_some()
}

#[derive(Deserialize)]
struct DownloadItemVersion {
    id: String,
    #[serde(rename = "eTag")]
    etag: Option<String>,
    size: Option<u64>,
    file: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct CreatedFolder {
    id: String,
    name: String,
    folder: Option<serde_json::Value>,
    #[serde(rename = "parentReference")]
    parent_reference: Option<FolderParent>,
}

#[derive(Deserialize)]
struct FolderParent {
    id: Option<String>,
}

#[derive(Deserialize)]
struct MovedFile {
    id: String,
    name: String,
    #[serde(rename = "eTag")]
    etag: Option<String>,
    file: Option<serde_json::Value>,
    folder: Option<serde_json::Value>,
    #[serde(rename = "parentReference")]
    parent_reference: Option<FolderParent>,
}

fn moved_item_matches(
    item: &MovedFile,
    id: &str,
    parent_id: &str,
    name: &str,
    directory: bool,
) -> bool {
    item.id == id
        && item.name == name
        && if directory {
            item.folder.is_some()
        } else {
            item.file.is_some()
        }
        && item.etag.as_deref().is_some_and(|etag| !etag.is_empty())
        && item
            .parent_reference
            .as_ref()
            .and_then(|parent| parent.id.as_deref())
            == Some(parent_id)
}

fn folder_page_is_empty(page: &serde_json::Value) -> Option<bool> {
    let children = page.get("value")?.as_array()?;
    Some(children.is_empty() && page.get("@odata.nextLink").is_none())
}

fn created_folder_matches(folder: &CreatedFolder, name: &str, parent_id: &str) -> bool {
    !folder.id.is_empty()
        && folder.name == name
        && folder.folder.is_some()
        && folder
            .parent_reference
            .as_ref()
            .and_then(|parent| parent.id.as_deref())
            .is_none_or(|id| id == parent_id)
}

#[derive(Debug)]
pub enum FolderCreateError {
    Conflict,
    Retry,
    NeedsReconcile,
}

#[derive(Debug)]
pub enum RemoteRenameError {
    Conflict,
    Retry,
    NeedsReconcile,
}

#[derive(Debug)]
pub enum RemoteDeleteError {
    Conflict,
    Retry,
    NeedsReconcile,
}

pub struct RemoteRenameInput<'a> {
    pub item_id: &'a str,
    pub source_parent_id: &'a str,
    pub source_name: &'a str,
    pub target_parent_id: &'a str,
    pub target_name: &'a str,
    pub etag: &'a str,
}

pub struct RemoteDeleteInput<'a> {
    pub item_id: &'a str,
    pub parent_id: &'a str,
    pub name: &'a str,
    pub etag: &'a str,
}

#[derive(Debug)]
pub enum QueuedDownloadError {
    Changed,
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum VirtualDownloadError {
    Changed,
    Offline,
    Failed,
}

impl From<FetchError> for VirtualDownloadError {
    fn from(error: FetchError) -> Self {
        match error {
            FetchError::Offline => Self::Offline,
            _ => Self::Failed,
        }
    }
}

fn transport_fetch_error(error: reqwest::Error) -> FetchError {
    if error.is_connect() || error.is_timeout() || error.is_body() {
        FetchError::Offline
    } else {
        FetchError::Failed
    }
}

pub struct QueuedDownloadTarget<'a> {
    pub root: &'a Path,
    pub path: &'a Path,
    pub expected_local: Option<&'a str>,
}

pub struct GraphUpload<'a> {
    pub graph: &'a GraphClient,
    pub drive_id: &'a str,
    pub access_token: &'a str,
    http: Client,
}

impl<'a> GraphUpload<'a> {
    pub fn new(
        graph: &'a GraphClient,
        drive_id: &'a str,
        access_token: &'a str,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            graph,
            drive_id,
            access_token,
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(30))
                .timeout(Duration::from_secs(300))
                .build()?,
        })
    }

    fn create_session_request(
        &self,
        operation: &QueuedOperation,
        size: u64,
    ) -> Result<reqwest::Request, UploadError> {
        let mut url = reqwest::Url::parse("https://graph.microsoft.com/v1.0/")
            .map_err(|_| UploadError::InvalidResponse)?;
        match operation.kind.as_str() {
            "UploadChanged" => {
                let id = operation
                    .remote_id
                    .as_deref()
                    .ok_or(UploadError::InvalidResponse)?;
                let etag = operation
                    .expected_etag
                    .as_deref()
                    .ok_or(UploadError::InvalidResponse)?;
                url.path_segments_mut()
                    .map_err(|_| UploadError::InvalidResponse)?
                    .pop_if_empty()
                    .extend(["drives", self.drive_id, "items", id, "createUploadSession"]);
                self.graph
                    .http
                    .post(url)
                    .bearer_auth(self.access_token)
                    .header(IF_MATCH, etag)
                    .json(&serde_json::json!({"item": {
                        "@microsoft.graph.conflictBehavior": "replace"
                    }}))
                    .build()
                    .map_err(|_| UploadError::InvalidResponse)
            }
            "UploadNew" => {
                let parts: Vec<_> = operation.path.split('/').collect();
                let name = parts.last().ok_or(UploadError::InvalidResponse)?;
                if operation.remote_id.is_some()
                    || operation.expected_etag.is_some()
                    || parts.iter().any(|part| !valid_new_file_name(part))
                    || size == 0
                {
                    return Err(UploadError::InvalidResponse);
                }
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| UploadError::InvalidResponse)?;
                segments
                    .pop_if_empty()
                    .extend(["drives", self.drive_id, "root:"]);
                segments.extend(parts[..parts.len() - 1].iter().copied());
                segments.push(&format!("{name}:"));
                segments.push("createUploadSession");
                drop(segments);
                self.graph
                    .http
                    .post(url)
                    .bearer_auth(self.access_token)
                    .json(&serde_json::json!({
                        "item": {
                            "@microsoft.graph.conflictBehavior": "fail",
                            "name": name,
                            "fileSize": size,
                        }
                    }))
                    .build()
                    .map_err(|_| UploadError::InvalidResponse)
            }
            "UploadNewByParent" => {
                let parent_id = operation
                    .remote_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .ok_or(UploadError::InvalidResponse)?;
                let name = operation.path.as_str();
                if operation.expected_etag.is_some() || !valid_new_file_name(name) || size == 0 {
                    return Err(UploadError::InvalidResponse);
                }
                url.path_segments_mut()
                    .map_err(|_| UploadError::InvalidResponse)?
                    .pop_if_empty()
                    .extend([
                        "drives",
                        self.drive_id,
                        "items",
                        &format!("{parent_id}:"),
                        &format!("{name}:"),
                        "createUploadSession",
                    ]);
                self.graph
                    .http
                    .post(url)
                    .bearer_auth(self.access_token)
                    .json(&serde_json::json!({"item": {
                        "@microsoft.graph.conflictBehavior": "fail",
                        "name": name,
                        "fileSize": size,
                    }}))
                    .build()
                    .map_err(|_| UploadError::InvalidResponse)
            }
            _ => Err(UploadError::InvalidResponse),
        }
    }

    fn empty_upload_request(
        &self,
        operation: &QueuedOperation,
    ) -> Result<reqwest::Request, UploadError> {
        if operation.kind != "UploadChanged" {
            return Err(UploadError::InvalidResponse);
        }
        let item_id = operation
            .remote_id
            .as_deref()
            .filter(|item_id| !item_id.is_empty())
            .ok_or(UploadError::InvalidResponse)?;
        let etag = operation
            .expected_etag
            .as_deref()
            .filter(|etag| !etag.is_empty())
            .ok_or(UploadError::InvalidResponse)?;
        let url =
            item_content_url(self.drive_id, item_id).map_err(|_| UploadError::InvalidResponse)?;
        self.graph
            .http
            .put(url)
            .bearer_auth(self.access_token)
            .header(IF_MATCH, etag)
            .header(CONTENT_TYPE, "application/octet-stream")
            .header(CONTENT_LENGTH, "0")
            .body(Vec::<u8>::new())
            .build()
            .map_err(|_| UploadError::InvalidResponse)
    }

    fn empty_new_file_request(
        &self,
        operation: &QueuedOperation,
    ) -> Result<reqwest::Request, UploadError> {
        let parent_id = operation
            .remote_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or(UploadError::InvalidResponse)?;
        if operation.kind != "UploadNewByParent"
            || operation.expected_etag.is_some()
            || !valid_new_file_name(&operation.path)
        {
            return Err(UploadError::InvalidResponse);
        }
        let mut url = reqwest::Url::parse("https://graph.microsoft.com/v1.0/")
            .map_err(|_| UploadError::InvalidResponse)?;
        url.path_segments_mut()
            .map_err(|_| UploadError::InvalidResponse)?
            .pop_if_empty()
            .extend(["drives", self.drive_id, "items", parent_id, "children"]);
        self.graph
            .http
            .post(url)
            .bearer_auth(self.access_token)
            .json(&serde_json::json!({
                "name": operation.path,
                "file": {},
                "@microsoft.graph.conflictBehavior": "fail",
            }))
            .build()
            .map_err(|_| UploadError::InvalidResponse)
    }

    fn range_request(
        &self,
        upload_url: &str,
        start: u64,
        total: u64,
        bytes: Vec<u8>,
    ) -> Result<reqwest::Request, UploadError> {
        if bytes.is_empty()
            || start
                .checked_add(bytes.len() as u64)
                .is_none_or(|end| end > total)
        {
            return Err(UploadError::InvalidResponse);
        }
        let end = start + bytes.len() as u64 - 1;
        self.http
            .put(session_url(upload_url)?)
            .header(CONTENT_RANGE, format!("bytes {start}-{end}/{total}"))
            .body(bytes)
            .build()
            .map_err(|_| UploadError::InvalidResponse)
    }
}

fn valid_new_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with([' ', '.'])
        && !name
            .chars()
            .any(|character| "\"*:<>?/\\|".contains(character) || character.is_control())
}

fn session_url(value: &str) -> Result<reqwest::Url, UploadError> {
    let url = reqwest::Url::parse(value).map_err(|_| UploadError::InvalidResponse)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(UploadError::InvalidResponse);
    }
    Ok(url)
}

fn next_offset(ranges: &[String]) -> Result<u64, UploadError> {
    ranges
        .iter()
        .filter_map(|range| {
            range
                .split_once('-')
                .and_then(|(start, _)| start.parse::<u64>().ok())
        })
        .min()
        .ok_or(UploadError::InvalidResponse)
}

fn item_content_url(drive_id: &str, item_id: &str) -> Result<reqwest::Url, FetchError> {
    let mut url =
        reqwest::Url::parse("https://graph.microsoft.com/v1.0/").map_err(|_| FetchError::Failed)?;
    url.path_segments_mut()
        .map_err(|_| FetchError::Failed)?
        .pop_if_empty()
        .extend(["drives", drive_id, "items", item_id, "content"]);
    Ok(url)
}

fn item_metadata_url(drive_id: &str, item_id: &str) -> Result<reqwest::Url, FetchError> {
    let mut url = item_content_url(drive_id, item_id)?;
    url.path_segments_mut()
        .map_err(|_| FetchError::Failed)?
        .pop();
    url.query_pairs_mut()
        .append_pair("$select", "id,name,parentReference,eTag,size,file");
    Ok(url)
}

fn directory_metadata_url(drive_id: &str, item_id: &str) -> Result<reqwest::Url, FetchError> {
    let mut url = item_metadata_url(drive_id, item_id)?;
    url.set_query(None);
    url.query_pairs_mut()
        .append_pair("$select", "id,name,parentReference,eTag,folder");
    Ok(url)
}

fn version_matches(item: &DownloadItemVersion, id: &str, etag: &str, size: u64) -> bool {
    item.id == id
        && item.etag.as_deref() == Some(etag)
        && item.size == Some(size)
        && item.file.is_some()
}

async fn upload_error(response: reqwest::Response) -> UploadError {
    match response.status() {
        StatusCode::NOT_FOUND => UploadError::Expired,
        StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => UploadError::Conflict,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
            let seconds = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(1)
                .min(60);
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            UploadError::Transient
        }
        status if status.is_server_error() || status == StatusCode::RANGE_NOT_SATISFIABLE => {
            UploadError::Transient
        }
        status => UploadError::HttpStatus(status.as_u16()),
    }
}

impl UploadTransport for GraphUpload<'_> {
    async fn create_session(
        &self,
        operation: &QueuedOperation,
        size: u64,
    ) -> Result<String, UploadError> {
        let response = self
            .graph
            .http
            .execute(self.create_session_request(operation, size)?)
            .await
            .map_err(|_| UploadError::Transient)?;
        if !response.status().is_success() {
            return Err(upload_error(response).await);
        }
        let session: Session = response
            .json()
            .await
            .map_err(|_| UploadError::InvalidResponse)?;
        session_url(&session.upload_url)?;
        Ok(session.upload_url)
    }

    async fn session_offset(&self, upload_url: &str) -> Result<u64, UploadError> {
        let url = session_url(upload_url)?;
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|_| UploadError::Transient)?;
        if !response.status().is_success() {
            return Err(upload_error(response).await);
        }
        let status: SessionStatus = response
            .json()
            .await
            .map_err(|_| UploadError::InvalidResponse)?;
        next_offset(&status.next_expected_ranges)
    }

    async fn send_range(
        &self,
        upload_url: &str,
        start: u64,
        total: u64,
        bytes: Vec<u8>,
    ) -> Result<UploadStep, UploadError> {
        let request = self.range_request(upload_url, start, total, bytes)?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|_| UploadError::Transient)?;
        match response.status() {
            StatusCode::ACCEPTED => {
                let status: SessionStatus = response
                    .json()
                    .await
                    .map_err(|_| UploadError::InvalidResponse)?;
                Ok(UploadStep::More(next_offset(&status.next_expected_ranges)?))
            }
            StatusCode::OK | StatusCode::CREATED => {
                let item: UploadedItem = response
                    .json()
                    .await
                    .map_err(|_| UploadError::InvalidResponse)?;
                if item.id.is_empty() {
                    return Err(UploadError::InvalidResponse);
                }
                Ok(UploadStep::Complete {
                    remote_id: item.id,
                    etag: item.etag,
                })
            }
            _ => Err(upload_error(response).await),
        }
    }

    async fn upload_empty(&self, operation: &QueuedOperation) -> Result<UploadStep, UploadError> {
        if operation.kind == "UploadNewByParent" {
            let parent_id = operation
                .remote_id
                .as_deref()
                .ok_or(UploadError::InvalidResponse)?;
            let response = self
                .graph
                .http
                .execute(self.empty_new_file_request(operation)?)
                .await
                .map_err(|_| UploadError::Transient)?;
            if response.status() != StatusCode::CREATED {
                return Err(upload_error(response).await);
            }
            let item: CreatedVirtualFile = response
                .json()
                .await
                .map_err(|_| UploadError::InvalidResponse)?;
            let etag = item.etag.as_deref().ok_or(UploadError::InvalidResponse)?;
            if item.id.is_empty()
                || etag.is_empty()
                || !created_virtual_file_matches(
                    &item,
                    &item.id,
                    parent_id,
                    &operation.path,
                    etag,
                    0,
                )
            {
                return Err(UploadError::InvalidResponse);
            }
            return Ok(UploadStep::Complete {
                remote_id: item.id,
                etag: item.etag,
            });
        }
        let item_id = operation
            .remote_id
            .as_deref()
            .ok_or(UploadError::InvalidResponse)?;
        let response = self
            .graph
            .http
            .execute(self.empty_upload_request(operation)?)
            .await
            .map_err(|_| UploadError::Transient)?;
        if !matches!(response.status(), StatusCode::OK | StatusCode::CREATED) {
            return Err(upload_error(response).await);
        }
        let item: UploadedEmptyItem = response
            .json()
            .await
            .map_err(|_| UploadError::InvalidResponse)?;
        if !empty_upload_matches(&item, item_id) {
            return Err(UploadError::InvalidResponse);
        }
        Ok(UploadStep::Complete {
            remote_id: item.id,
            etag: item.etag,
        })
    }
}

impl GraphClient {
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()?,
        })
    }

    fn remote_rename_request(
        &self,
        drive_id: &str,
        item_id: &str,
        parent_id: &str,
        name: &str,
        etag: &str,
        access_token: &str,
    ) -> Result<reqwest::Request, RemoteRenameError> {
        if drive_id.is_empty()
            || item_id.is_empty()
            || parent_id.is_empty()
            || etag.is_empty()
            || !valid_new_file_name(name)
        {
            return Err(RemoteRenameError::Conflict);
        }
        let mut url = reqwest::Url::parse("https://graph.microsoft.com/v1.0/")
            .map_err(|_| RemoteRenameError::Retry)?;
        url.path_segments_mut()
            .map_err(|_| RemoteRenameError::Retry)?
            .pop_if_empty()
            .extend(["drives", drive_id, "items", item_id]);
        self.http
            .patch(url)
            .bearer_auth(access_token)
            .header(IF_MATCH, etag)
            .json(&serde_json::json!({
                "name": name,
                "parentReference": { "id": parent_id },
            }))
            .build()
            .map_err(|_| RemoteRenameError::Retry)
    }

    fn remote_rename_source_request(
        &self,
        drive_id: &str,
        item_id: &str,
        access_token: &str,
    ) -> Result<reqwest::Request, RemoteRenameError> {
        if drive_id.is_empty() || item_id.is_empty() {
            return Err(RemoteRenameError::Conflict);
        }
        let mut url = reqwest::Url::parse("https://graph.microsoft.com/v1.0/")
            .map_err(|_| RemoteRenameError::Retry)?;
        url.path_segments_mut()
            .map_err(|_| RemoteRenameError::Retry)?
            .pop_if_empty()
            .extend(["drives", drive_id, "items", item_id]);
        url.query_pairs_mut()
            .append_pair("$select", "id,name,eTag,file,folder,parentReference");
        self.http
            .get(url)
            .bearer_auth(access_token)
            .build()
            .map_err(|_| RemoteRenameError::Retry)
    }

    pub async fn rename_remote_file(
        &self,
        drive_id: &str,
        input: &RemoteRenameInput<'_>,
        access_token: &str,
    ) -> Result<String, RemoteRenameError> {
        self.rename_remote_item(drive_id, input, false, access_token)
            .await
    }

    pub async fn rename_remote_item(
        &self,
        drive_id: &str,
        input: &RemoteRenameInput<'_>,
        directory: bool,
        access_token: &str,
    ) -> Result<String, RemoteRenameError> {
        let source_request =
            self.remote_rename_source_request(drive_id, input.item_id, access_token)?;
        let source_response = self
            .http
            .execute(source_request)
            .await
            .map_err(|_| RemoteRenameError::Retry)?;
        if source_response.status() == StatusCode::NOT_FOUND {
            return Err(RemoteRenameError::Conflict);
        }
        if !source_response.status().is_success() {
            return Err(RemoteRenameError::Retry);
        }
        let source: MovedFile = source_response
            .json()
            .await
            .map_err(|_| RemoteRenameError::Retry)?;
        if !moved_item_matches(
            &source,
            input.item_id,
            input.source_parent_id,
            input.source_name,
            directory,
        ) || source.etag.as_deref() != Some(input.etag)
        {
            return Err(RemoteRenameError::Conflict);
        }
        let request = self.remote_rename_request(
            drive_id,
            input.item_id,
            input.target_parent_id,
            input.target_name,
            input.etag,
            access_token,
        )?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|_| RemoteRenameError::Retry)?;
        match response.status() {
            StatusCode::OK => {
                let item: MovedFile = response
                    .json()
                    .await
                    .map_err(|_| RemoteRenameError::NeedsReconcile)?;
                if !moved_item_matches(
                    &item,
                    input.item_id,
                    input.target_parent_id,
                    input.target_name,
                    directory,
                ) {
                    return Err(RemoteRenameError::NeedsReconcile);
                }
                Ok(item.etag.expect("validated eTag"))
            }
            StatusCode::CONFLICT
            | StatusCode::PRECONDITION_FAILED
            | StatusCode::NOT_FOUND
            | StatusCode::BAD_REQUEST => Err(RemoteRenameError::Conflict),
            _ => Err(RemoteRenameError::Retry),
        }
    }

    fn remote_delete_request(
        &self,
        drive_id: &str,
        item_id: &str,
        etag: &str,
        access_token: &str,
    ) -> Result<reqwest::Request, RemoteDeleteError> {
        if drive_id.is_empty() || item_id.is_empty() || etag.is_empty() {
            return Err(RemoteDeleteError::Conflict);
        }
        let mut url = reqwest::Url::parse("https://graph.microsoft.com/v1.0/")
            .map_err(|_| RemoteDeleteError::Retry)?;
        url.path_segments_mut()
            .map_err(|_| RemoteDeleteError::Retry)?
            .pop_if_empty()
            .extend(["drives", drive_id, "items", item_id]);
        self.http
            .delete(url)
            .bearer_auth(access_token)
            .header(IF_MATCH, etag)
            .build()
            .map_err(|_| RemoteDeleteError::Retry)
    }

    pub async fn delete_remote_file(
        &self,
        drive_id: &str,
        item_id: &str,
        parent_id: &str,
        name: &str,
        etag: &str,
        access_token: &str,
    ) -> Result<(), RemoteDeleteError> {
        self.delete_remote_item(
            drive_id,
            &RemoteDeleteInput {
                item_id,
                parent_id,
                name,
                etag,
            },
            false,
            access_token,
        )
        .await
    }

    pub async fn delete_remote_item(
        &self,
        drive_id: &str,
        input: &RemoteDeleteInput<'_>,
        directory: bool,
        access_token: &str,
    ) -> Result<(), RemoteDeleteError> {
        let source_request = self
            .remote_rename_source_request(drive_id, input.item_id, access_token)
            .map_err(|_| RemoteDeleteError::Retry)?;
        let source_response = self
            .http
            .execute(source_request)
            .await
            .map_err(|_| RemoteDeleteError::Retry)?;
        if source_response.status() == StatusCode::NOT_FOUND {
            return Err(RemoteDeleteError::NeedsReconcile);
        }
        if !source_response.status().is_success() {
            return Err(RemoteDeleteError::Retry);
        }
        let source: MovedFile = source_response
            .json()
            .await
            .map_err(|_| RemoteDeleteError::Retry)?;
        if !moved_item_matches(
            &source,
            input.item_id,
            input.parent_id,
            input.name,
            directory,
        ) || source.etag.as_deref() != Some(input.etag)
        {
            return Err(RemoteDeleteError::Conflict);
        }
        let request =
            self.remote_delete_request(drive_id, input.item_id, input.etag, access_token)?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|_| RemoteDeleteError::Retry)?;
        match response.status() {
            StatusCode::NO_CONTENT => Ok(()),
            StatusCode::NOT_FOUND => Err(RemoteDeleteError::NeedsReconcile),
            StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED | StatusCode::BAD_REQUEST => {
                Err(RemoteDeleteError::Conflict)
            }
            _ => Err(RemoteDeleteError::Retry),
        }
    }

    pub async fn remote_folder_is_empty(
        &self,
        drive_id: &str,
        item_id: &str,
        access_token: &str,
    ) -> Result<bool, RemoteDeleteError> {
        let mut url = reqwest::Url::parse("https://graph.microsoft.com/v1.0/")
            .map_err(|_| RemoteDeleteError::Retry)?;
        url.path_segments_mut()
            .map_err(|_| RemoteDeleteError::Retry)?
            .pop_if_empty()
            .extend(["drives", drive_id, "items", item_id, "children"]);
        url.query_pairs_mut()
            .append_pair("$select", "id")
            .append_pair("$top", "1");
        let response = self
            .http
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|_| RemoteDeleteError::Retry)?;
        if !response.status().is_success() {
            return Err(RemoteDeleteError::Retry);
        }
        let page: serde_json::Value = response
            .json()
            .await
            .map_err(|_| RemoteDeleteError::Retry)?;
        folder_page_is_empty(&page).ok_or(RemoteDeleteError::Retry)
    }

    fn create_folder_request(
        &self,
        drive_id: &str,
        parent_id: &str,
        name: &str,
        access_token: &str,
    ) -> Result<reqwest::Request, FolderCreateError> {
        if drive_id.is_empty() || parent_id.is_empty() || !valid_new_file_name(name) {
            return Err(FolderCreateError::Conflict);
        }
        let mut url = reqwest::Url::parse("https://graph.microsoft.com/v1.0/")
            .map_err(|_| FolderCreateError::Retry)?;
        url.path_segments_mut()
            .map_err(|_| FolderCreateError::Retry)?
            .pop_if_empty()
            .extend(["drives", drive_id, "items", parent_id, "children"]);
        self.http
            .post(url)
            .bearer_auth(access_token)
            .json(&serde_json::json!({
                "name": name,
                "folder": {},
                "@microsoft.graph.conflictBehavior": "fail",
            }))
            .build()
            .map_err(|_| FolderCreateError::Retry)
    }

    pub async fn create_remote_folder(
        &self,
        drive_id: &str,
        parent_id: &str,
        name: &str,
        access_token: &str,
    ) -> Result<String, FolderCreateError> {
        let request = self.create_folder_request(drive_id, parent_id, name, access_token)?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|_| FolderCreateError::Retry)?;
        match response.status() {
            StatusCode::CREATED => {
                let folder: CreatedFolder = response
                    .json()
                    .await
                    .map_err(|_| FolderCreateError::NeedsReconcile)?;
                if !created_folder_matches(&folder, name, parent_id) {
                    return Err(FolderCreateError::NeedsReconcile);
                }
                Ok(folder.id)
            }
            StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED | StatusCode::BAD_REQUEST => {
                Err(FolderCreateError::Conflict)
            }
            _ => Err(FolderCreateError::Retry),
        }
    }

    pub async fn created_virtual_directory_etag(
        &self,
        drive_id: &str,
        item_id: &str,
        parent_id: &str,
        name: &str,
        access_token: &str,
    ) -> Result<Option<String>, FetchError> {
        let url = directory_metadata_url(drive_id, item_id)?;
        let item: MovedFile = self
            .get_with_retry(url.as_str(), access_token)
            .await?
            .json()
            .await
            .map_err(transport_fetch_error)?;
        Ok(moved_item_matches(&item, item_id, parent_id, name, true)
            .then_some(item.etag)
            .flatten())
    }

    pub async fn drive_id(&self, access_token: &str) -> Result<String, FetchError> {
        let response = self.get_with_retry(DRIVE_URL, access_token).await?;
        let drive: Drive = response.json().await.map_err(|_| FetchError::Failed)?;
        if drive.id.is_empty() {
            return Err(FetchError::Failed);
        }
        Ok(drive.id)
    }

    pub async fn root_id(&self, access_token: &str) -> Result<String, FetchError> {
        let response = self.get_with_retry(ROOT_URL, access_token).await?;
        let root: Drive = response.json().await.map_err(|_| FetchError::Failed)?;
        if root.id.is_empty() {
            return Err(FetchError::Failed);
        }
        Ok(root.id)
    }

    pub async fn delta_page(&self, url: &str, access_token: &str) -> Result<DeltaPage, FetchError> {
        validate_graph_url(url).map_err(|_| FetchError::Failed)?;
        let response = self.get_with_retry(url, access_token).await?;
        response.json().await.map_err(|_| FetchError::Failed)
    }

    pub async fn download_item(
        &self,
        drive_id: &str,
        item_id: &str,
        access_token: &str,
        target: &Path,
        expected_size: Option<u64>,
    ) -> Result<(), FetchError> {
        let mut response = self
            .content_response(drive_id, item_id, access_token)
            .await?;
        let mut transfer = AtomicDownload::new(target).map_err(|_| FetchError::Failed)?;
        while let Some(chunk) = response.chunk().await.map_err(|_| FetchError::Failed)? {
            transfer
                .write_chunk(&chunk)
                .await
                .map_err(|_| FetchError::Failed)?;
        }
        transfer
            .finish(target, expected_size)
            .await
            .map_err(|_| FetchError::Failed)
    }

    pub async fn download_queued(
        &self,
        drive_id: &str,
        item_id: &str,
        access_token: &str,
        target: QueuedDownloadTarget<'_>,
        expected_etag: &str,
        expected_size: u64,
    ) -> Result<String, QueuedDownloadError> {
        let before = self
            .item_version(drive_id, item_id, access_token)
            .await
            .map_err(|_| QueuedDownloadError::Failed)?;
        if !version_matches(&before, item_id, expected_etag, expected_size) {
            return Err(QueuedDownloadError::Changed);
        }
        let mut transfer =
            AtomicDownload::new_queued(target.root, target.path, target.expected_local.is_some())
                .map_err(|_| QueuedDownloadError::Changed)?;
        let mut response = self
            .content_response(drive_id, item_id, access_token)
            .await
            .map_err(|_| QueuedDownloadError::Failed)?;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| QueuedDownloadError::Failed)?
        {
            transfer
                .write_chunk(&chunk)
                .await
                .map_err(|_| QueuedDownloadError::Failed)?;
        }
        let after = self
            .item_version(drive_id, item_id, access_token)
            .await
            .map_err(|_| QueuedDownloadError::Failed)?;
        if !version_matches(&after, item_id, expected_etag, expected_size) {
            return Err(QueuedDownloadError::Changed);
        }
        transfer
            .finish_queued(
                target.root,
                target.path,
                expected_size,
                target.expected_local,
            )
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    QueuedDownloadError::Changed
                } else {
                    QueuedDownloadError::Failed
                }
            })
    }

    /// Fetch a whole file for the virtual drive, refusing to publish changed content.
    pub async fn download_cache_item(
        &self,
        request: &DownloadRequest,
        access_token: &str,
        target: &Path,
    ) -> Result<(), VirtualDownloadError> {
        let before = self
            .item_version(&request.drive_id, &request.item_id, access_token)
            .await
            .map_err(VirtualDownloadError::from)?;
        if !version_matches(&before, &request.item_id, &request.etag, request.size) {
            return Err(VirtualDownloadError::Changed);
        }
        let mut transfer = AtomicDownload::new(target).map_err(|_| VirtualDownloadError::Failed)?;
        let mut response = self
            .content_response(&request.drive_id, &request.item_id, access_token)
            .await
            .map_err(VirtualDownloadError::from)?;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| VirtualDownloadError::from(transport_fetch_error(error)))?
        {
            transfer
                .write_chunk(&chunk)
                .await
                .map_err(|_| VirtualDownloadError::Failed)?;
        }
        let after = self
            .item_version(&request.drive_id, &request.item_id, access_token)
            .await
            .map_err(VirtualDownloadError::from)?;
        if !version_matches(&after, &request.item_id, &request.etag, request.size) {
            return Err(VirtualDownloadError::Changed);
        }
        transfer
            .finish(target, Some(request.size))
            .await
            .map_err(|_| VirtualDownloadError::Failed)
    }

    async fn item_version(
        &self,
        drive_id: &str,
        item_id: &str,
        access_token: &str,
    ) -> Result<DownloadItemVersion, FetchError> {
        let url = item_metadata_url(drive_id, item_id)?;
        self.get_with_retry(url.as_str(), access_token)
            .await?
            .json()
            .await
            .map_err(transport_fetch_error)
    }

    pub async fn matches_remote_file_version(
        &self,
        drive_id: &str,
        item_id: &str,
        expected_etag: &str,
        access_token: &str,
    ) -> Result<bool, FetchError> {
        let item = self.item_version(drive_id, item_id, access_token).await?;
        Ok(
            item.id == item_id
                && item.etag.as_deref() == Some(expected_etag)
                && item.file.is_some(),
        )
    }

    pub async fn matches_created_virtual_file(
        &self,
        drive_id: &str,
        expected: &CreatedVirtualExpectation<'_>,
        access_token: &str,
    ) -> Result<bool, FetchError> {
        let url = item_metadata_url(drive_id, expected.item_id)?;
        let item: CreatedVirtualFile = self
            .get_with_retry(url.as_str(), access_token)
            .await?
            .json()
            .await
            .map_err(transport_fetch_error)?;
        Ok(created_virtual_file_matches(
            &item,
            expected.item_id,
            expected.parent_id,
            expected.name,
            expected.etag,
            expected.size,
        ))
    }

    async fn content_response(
        &self,
        drive_id: &str,
        item_id: &str,
        access_token: &str,
    ) -> Result<reqwest::Response, FetchError> {
        let url = item_content_url(drive_id, item_id)?;
        let download_http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| FetchError::Failed)?;
        let mut response = download_http
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(transport_fetch_error)?;
        for _ in 0..4 {
            if !response.status().is_redirection() {
                break;
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or(FetchError::Failed)?;
            let url = reqwest::Url::parse(location).map_err(|_| FetchError::Failed)?;
            if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
                return Err(FetchError::Failed);
            }
            response = download_http
                .get(url)
                .send()
                .await
                .map_err(transport_fetch_error)?;
        }
        if !response.status().is_success() {
            return Err(FetchError::Failed);
        }
        Ok(response)
    }

    async fn get_with_retry(
        &self,
        url: &str,
        access_token: &str,
    ) -> Result<reqwest::Response, FetchError> {
        for attempt in 0..4 {
            let response = self
                .http
                .get(url)
                .bearer_auth(access_token)
                .send()
                .await
                .map_err(transport_fetch_error)?;
            if response.status() == StatusCode::GONE {
                return Err(FetchError::Gone);
            }
            if response.status() == StatusCode::TOO_MANY_REQUESTS
                || response.status() == StatusCode::SERVICE_UNAVAILABLE
            {
                if attempt == 3 {
                    return Err(FetchError::Failed);
                }
                let wait = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(|seconds| seconds.min(60))
                    .unwrap_or(1 << attempt);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            return response.error_for_status().map_err(|_| FetchError::Failed);
        }
        Err(FetchError::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::AUTHORIZATION;

    #[test]
    fn virtual_download_preserves_offline_and_version_failures() {
        assert_eq!(
            VirtualDownloadError::from(FetchError::Offline),
            VirtualDownloadError::Offline
        );
        assert_eq!(
            VirtualDownloadError::from(FetchError::Gone),
            VirtualDownloadError::Failed
        );
    }

    #[test]
    fn remote_rename_uses_id_parent_and_if_match() {
        let graph = GraphClient::new().unwrap();
        let source = graph
            .remote_rename_source_request("drive", "item", "token")
            .unwrap();
        assert_eq!(source.method(), reqwest::Method::GET);
        assert_eq!(source.url().path(), "/v1.0/drives/drive/items/item");
        assert_eq!(
            source.url().query(),
            Some("%24select=id%2Cname%2CeTag%2Cfile%2Cfolder%2CparentReference")
        );
        let request = graph
            .remote_rename_request("drive", "item", "parent", "new.txt", "v1", "token")
            .unwrap();
        assert_eq!(request.method(), reqwest::Method::PATCH);
        assert_eq!(request.url().path(), "/v1.0/drives/drive/items/item");
        assert_eq!(request.headers()[IF_MATCH], "v1");
        assert!(request.headers().contains_key(AUTHORIZATION));
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["name"], "new.txt");
        assert_eq!(body["parentReference"]["id"], "parent");
        assert!(
            graph
                .remote_rename_request("drive", "item", "parent", "bad:name", "v1", "token")
                .is_err()
        );
    }

    #[test]
    fn remote_rename_response_requires_exact_identity_and_version() {
        let item: MovedFile = serde_json::from_value(serde_json::json!({
            "id": "item", "name": "new.txt", "eTag": "v2", "file": {},
            "parentReference": { "id": "parent" },
        }))
        .unwrap();
        assert!(moved_item_matches(
            &item, "item", "parent", "new.txt", false
        ));
        assert!(!moved_item_matches(
            &item, "other", "parent", "new.txt", false
        ));
        assert!(!moved_item_matches(
            &item, "item", "other", "new.txt", false
        ));
        assert!(!moved_item_matches(
            &item,
            "item",
            "parent",
            "other.txt",
            false
        ));
        let no_etag: MovedFile = serde_json::from_value(serde_json::json!({
            "id": "item", "name": "new.txt", "file": {},
            "parentReference": { "id": "parent" },
        }))
        .unwrap();
        assert!(!moved_item_matches(
            &no_etag, "item", "parent", "new.txt", false
        ));
        let folder: MovedFile = serde_json::from_value(serde_json::json!({
            "id": "folder", "name": "renamed", "eTag": "v2", "folder": {},
            "parentReference": { "id": "parent" },
        }))
        .unwrap();
        assert!(moved_item_matches(
            &folder, "folder", "parent", "renamed", true
        ));
        assert!(!moved_item_matches(
            &folder, "folder", "parent", "renamed", false
        ));
    }

    #[test]
    fn remote_delete_uses_item_id_and_if_match() {
        let graph = GraphClient::new().unwrap();
        let request = graph
            .remote_delete_request("drive", "item", "v1", "token")
            .unwrap();
        assert_eq!(request.method(), reqwest::Method::DELETE);
        assert_eq!(request.url().path(), "/v1.0/drives/drive/items/item");
        assert_eq!(request.headers()[IF_MATCH], "v1");
        assert!(request.headers().contains_key(AUTHORIZATION));
        assert!(request.body().is_none());
        assert!(
            graph
                .remote_delete_request("drive", "item", "", "token")
                .is_err()
        );
    }

    #[test]
    fn remote_folder_is_empty_only_after_complete_empty_page() {
        assert_eq!(
            folder_page_is_empty(&serde_json::json!({"value": []})),
            Some(true)
        );
        assert_eq!(
            folder_page_is_empty(&serde_json::json!({"value": [{"id":"child"}]})),
            Some(false)
        );
        assert_eq!(
            folder_page_is_empty(&serde_json::json!({"value": [], "@odata.nextLink":"more"})),
            Some(false)
        );
        assert_eq!(folder_page_is_empty(&serde_json::json!({})), None);
    }

    #[test]
    fn folder_creation_uses_parent_id_and_fails_on_name_conflict() {
        let graph = GraphClient::new().unwrap();
        let request = graph
            .create_folder_request("drive", "parent", "My Notes", "token")
            .unwrap();
        assert_eq!(
            request.url().path(),
            "/v1.0/drives/drive/items/parent/children"
        );
        assert!(request.headers().contains_key(AUTHORIZATION));
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["name"], "My Notes");
        assert_eq!(body["folder"], serde_json::json!({}));
        assert_eq!(body["@microsoft.graph.conflictBehavior"], "fail");
        assert!(
            graph
                .create_folder_request("drive", "parent", "bad:name", "token")
                .is_err()
        );
        let created: CreatedFolder = serde_json::from_value(serde_json::json!({
            "id": "new-id", "name": "My Notes", "folder": {},
            "parentReference": { "id": "parent" }
        }))
        .unwrap();
        assert!(created_folder_matches(&created, "My Notes", "parent"));
        assert!(!created_folder_matches(&created, "other", "parent"));
        assert!(!created_folder_matches(
            &created,
            "My Notes",
            "other-parent"
        ));
    }

    #[test]
    fn created_directory_metadata_requires_exact_identity_and_etag() {
        let url = directory_metadata_url("drive", "folder").unwrap();
        assert_eq!(url.path(), "/v1.0/drives/drive/items/folder");
        assert_eq!(
            url.query(),
            Some("%24select=id%2Cname%2CparentReference%2CeTag%2Cfolder")
        );
        let item: MovedFile = serde_json::from_value(serde_json::json!({
            "id": "folder", "name": "My Notes", "eTag": "v1", "folder": {},
            "parentReference": { "id": "parent" }
        }))
        .unwrap();
        assert!(moved_item_matches(
            &item, "folder", "parent", "My Notes", true
        ));
        assert!(!moved_item_matches(
            &item, "folder", "wrong", "My Notes", true
        ));
        let missing_etag: MovedFile = serde_json::from_value(serde_json::json!({
            "id": "folder", "name": "My Notes", "folder": {},
            "parentReference": { "id": "parent" }
        }))
        .unwrap();
        assert!(!moved_item_matches(
            &missing_etag,
            "folder",
            "parent",
            "My Notes",
            true
        ));
    }

    #[test]
    fn queued_download_requires_exact_remote_version() {
        let item = DownloadItemVersion {
            id: "item".into(),
            etag: Some("v2".into()),
            size: Some(4),
            file: Some(serde_json::json!({})),
        };
        assert!(version_matches(&item, "item", "v2", 4));
        assert!(!version_matches(&item, "other", "v2", 4));
        assert!(!version_matches(&item, "item", "v1", 4));
        assert!(!version_matches(&item, "item", "v2", 3));
    }

    #[test]
    fn session_fragments_never_carry_bearer_token() {
        let graph = GraphClient::new().unwrap();
        let upload = GraphUpload::new(&graph, "drive", "test-token").unwrap();
        let operation = QueuedOperation {
            id: 1,
            kind: "UploadChanged".into(),
            path: "file.txt".into(),
            from_path: None,
            remote_id: Some("item".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: Some("hash".into()),
            attempts: 1,
        };
        let create = upload.create_session_request(&operation, 10).unwrap();
        assert_eq!(create.headers().get(IF_MATCH).unwrap(), "v1");
        assert!(create.headers().contains_key(AUTHORIZATION));
        let body: serde_json::Value =
            serde_json::from_slice(create.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["item"]["@microsoft.graph.conflictBehavior"], "replace");
        assert_eq!(
            create.url().path(),
            "/v1.0/drives/drive/items/item/createUploadSession"
        );
        let range = upload
            .range_request(
                "https://upload.example/session?secret=hidden",
                0,
                10,
                vec![1, 2],
            )
            .unwrap();
        assert!(!range.headers().contains_key(AUTHORIZATION));
        assert_eq!(range.headers().get(CONTENT_RANGE).unwrap(), "bytes 0-1/10");
        assert!(session_url("http://upload.example/session").is_err());
        assert_eq!(
            next_offset(&["327680-".into(), "655360-".into()]).unwrap(),
            327680
        );
        assert_eq!(
            item_content_url("drive", "item").unwrap().path(),
            "/v1.0/drives/drive/items/item/content"
        );
        assert_eq!(
            item_metadata_url("drive", "item").unwrap().path(),
            "/v1.0/drives/drive/items/item"
        );
    }

    #[test]
    fn empty_upload_is_conditional_and_requires_confirmed_empty_file() {
        let graph = GraphClient::new().unwrap();
        let upload = GraphUpload::new(&graph, "drive", "test-token").unwrap();
        let operation = QueuedOperation {
            id: 0,
            kind: "UploadChanged".into(),
            path: String::new(),
            from_path: None,
            remote_id: Some("item".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: None,
            attempts: 0,
        };
        let request = upload.empty_upload_request(&operation).unwrap();
        assert_eq!(request.method(), reqwest::Method::PUT);
        assert_eq!(
            request.url().path(),
            "/v1.0/drives/drive/items/item/content"
        );
        assert_eq!(request.headers()[IF_MATCH], "v1");
        assert_eq!(request.headers()[CONTENT_LENGTH], "0");
        assert!(request.headers().contains_key(AUTHORIZATION));
        assert_eq!(request.body().unwrap().as_bytes().unwrap(), b"");
        let item: UploadedEmptyItem = serde_json::from_value(serde_json::json!({
            "id": "item", "eTag": "v2", "size": 0, "file": {}
        }))
        .unwrap();
        assert!(empty_upload_matches(&item, "item"));
        assert!(!empty_upload_matches(&item, "other"));
        let missing_etag: UploadedEmptyItem = serde_json::from_value(serde_json::json!({
            "id": "item", "size": 0, "file": {}
        }))
        .unwrap();
        assert!(!empty_upload_matches(&missing_etag, "item"));
    }

    #[test]
    fn empty_new_file_create_fails_on_conflict_and_requires_file_identity() {
        let graph = GraphClient::new().unwrap();
        let upload = GraphUpload::new(&graph, "drive", "test-token").unwrap();
        let mut operation = QueuedOperation {
            id: 0,
            kind: "UploadNewByParent".into(),
            path: "new file.txt".into(),
            from_path: None,
            remote_id: Some("parent".into()),
            expected_etag: None,
            local_fingerprint: None,
            attempts: 0,
        };
        let request = upload.empty_new_file_request(&operation).unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request.url().path(),
            "/v1.0/drives/drive/items/parent/children"
        );
        assert!(request.headers().contains_key(AUTHORIZATION));
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["name"], "new file.txt");
        assert_eq!(body["file"], serde_json::json!({}));
        assert_eq!(body["@microsoft.graph.conflictBehavior"], "fail");
        let item: CreatedVirtualFile = serde_json::from_value(serde_json::json!({
            "id": "created", "name": "new file.txt", "eTag": "v1", "size": 0,
            "file": {}, "parentReference": {"id": "parent"}
        }))
        .unwrap();
        assert!(created_virtual_file_matches(
            &item,
            "created",
            "parent",
            "new file.txt",
            "v1",
            0
        ));
        assert!(!created_virtual_file_matches(
            &item,
            "created",
            "other",
            "new file.txt",
            "v1",
            0
        ));
        operation.path = "../unsafe".into();
        assert!(upload.empty_new_file_request(&operation).is_err());
        operation.path = "new.txt".into();
        operation.expected_etag = Some("v1".into());
        assert!(upload.empty_new_file_request(&operation).is_err());
    }

    #[test]
    fn new_upload_fails_on_name_conflict_and_escapes_name() {
        let graph = GraphClient::new().unwrap();
        let upload = GraphUpload::new(&graph, "drive", "test-token").unwrap();
        let mut operation = QueuedOperation {
            id: 1,
            kind: "UploadNew".into(),
            path: "new file.txt".into(),
            from_path: None,
            remote_id: None,
            expected_etag: None,
            local_fingerprint: Some("hash".into()),
            attempts: 1,
        };
        let request = upload.create_session_request(&operation, 42).unwrap();
        assert_eq!(
            request.url().path(),
            "/v1.0/drives/drive/root:/new%20file.txt:/createUploadSession"
        );
        assert!(request.headers().contains_key(AUTHORIZATION));
        assert!(!request.headers().contains_key(IF_MATCH));
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["item"]["@microsoft.graph.conflictBehavior"], "fail");
        assert_eq!(body["item"]["name"], "new file.txt");
        assert_eq!(body["item"]["fileSize"], 42);
        operation.path = "Docs/Work/new file.txt".into();
        let nested = upload.create_session_request(&operation, 42).unwrap();
        assert_eq!(
            nested.url().path(),
            "/v1.0/drives/drive/root:/Docs/Work/new%20file.txt:/createUploadSession"
        );
        operation.path = "../secret".into();
        assert!(upload.create_session_request(&operation, 42).is_err());
        operation.path = "Docs//secret".into();
        assert!(upload.create_session_request(&operation, 42).is_err());
        operation.path = "new.txt".into();
        operation.remote_id = Some("existing".into());
        assert!(upload.create_session_request(&operation, 42).is_err());
    }

    #[test]
    fn virtual_new_upload_uses_parent_id_and_fails_on_conflict() {
        let graph = GraphClient::new().unwrap();
        let upload = GraphUpload::new(&graph, "drive", "test-token").unwrap();
        let mut operation = QueuedOperation {
            id: 0,
            kind: "UploadNewByParent".into(),
            path: "new file.txt".into(),
            from_path: None,
            remote_id: Some("parent-id".into()),
            expected_etag: None,
            local_fingerprint: None,
            attempts: 0,
        };
        let request = upload.create_session_request(&operation, 42).unwrap();
        assert_eq!(
            request.url().path(),
            "/v1.0/drives/drive/items/parent-id:/new%20file.txt:/createUploadSession"
        );
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["item"]["@microsoft.graph.conflictBehavior"], "fail");
        operation.path = "invalid:name".into();
        assert!(upload.create_session_request(&operation, 42).is_err());
    }

    #[test]
    fn uploaded_virtual_file_must_match_parent_name_and_version() {
        let item: CreatedVirtualFile = serde_json::from_value(serde_json::json!({
            "id": "new-id", "name": "new.txt", "size": 3, "eTag": "v1",
            "file": {}, "parentReference": {"id": "parent"}
        }))
        .unwrap();
        assert!(created_virtual_file_matches(
            &item, "new-id", "parent", "new.txt", "v1", 3
        ));
        assert!(!created_virtual_file_matches(
            &item, "new-id", "other", "new.txt", "v1", 3
        ));
        assert!(!created_virtual_file_matches(
            &item, "new-id", "parent", "new.txt", "v2", 3
        ));
    }
}
