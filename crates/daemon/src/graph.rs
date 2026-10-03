//! Graph v1.0 transport. URLs from delta are checked before attaching a
//! bearer token; preauthenticated transfer URLs never receive one.

use onedrive_catalog::{DeltaPage, FetchError, validate_graph_url};
use onedrive_sync::QueuedOperation;
use onedrive_sync::upload::{UploadError, UploadStep, UploadTransport};
use reqwest::{
    Client, StatusCode,
    header::{CONTENT_RANGE, IF_MATCH, RETRY_AFTER},
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

pub struct ExistingUpload<'a> {
    pub graph: &'a GraphClient,
    pub drive_id: &'a str,
    pub access_token: &'a str,
    http: Client,
}

impl<'a> ExistingUpload<'a> {
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
    ) -> Result<reqwest::Request, UploadError> {
        if operation.kind != "UploadChanged" {
            return Err(UploadError::InvalidResponse);
        }
        let id = operation
            .remote_id
            .as_deref()
            .ok_or(UploadError::InvalidResponse)?;
        let etag = operation
            .expected_etag
            .as_deref()
            .ok_or(UploadError::InvalidResponse)?;
        let mut url = reqwest::Url::parse("https://graph.microsoft.com/v1.0/")
            .map_err(|_| UploadError::InvalidResponse)?;
        url.path_segments_mut()
            .map_err(|_| UploadError::InvalidResponse)?
            .pop_if_empty()
            .extend(["drives", self.drive_id, "items", id, "createUploadSession"]);
        self.graph
            .http
            .post(url)
            .bearer_auth(self.access_token)
            .header(IF_MATCH, etag)
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
        _ => UploadError::InvalidResponse,
    }
}

impl UploadTransport for ExistingUpload<'_> {
    async fn create_session(
        &self,
        operation: &QueuedOperation,
        _: u64,
    ) -> Result<String, UploadError> {
        let response = self
            .graph
            .http
            .execute(self.create_session_request(operation)?)
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
            .map_err(|_| FetchError::Failed)?;
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
                .map_err(|_| FetchError::Failed)?;
        }
        if !response.status().is_success() {
            return Err(FetchError::Failed);
        }
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
                .map_err(|_| FetchError::Failed)?;
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
    fn session_fragments_never_carry_bearer_token() {
        let graph = GraphClient::new().unwrap();
        let upload = ExistingUpload::new(&graph, "drive", "test-token").unwrap();
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
        let create = upload.create_session_request(&operation).unwrap();
        assert_eq!(create.headers().get(IF_MATCH).unwrap(), "v1");
        assert!(create.headers().contains_key(AUTHORIZATION));
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
    }
}
