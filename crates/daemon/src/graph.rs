//! Minimal Graph v1.0 read-only transport. URLs from delta are checked before
//! attaching a bearer token.

use onedrive_catalog::{DeltaPage, FetchError, validate_graph_url};
use reqwest::{Client, StatusCode, header::RETRY_AFTER};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

use crate::transfer::AtomicDownload;

const DRIVE_URL: &str = "https://graph.microsoft.com/v1.0/me/drive";

#[derive(Clone)]
pub struct GraphClient {
    http: Client,
}

#[derive(Deserialize)]
struct Drive {
    id: String,
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
        let mut url = reqwest::Url::parse("https://graph.microsoft.com/v1.0/")
            .map_err(|_| FetchError::Failed)?;
        url.path_segments_mut()
            .map_err(|_| FetchError::Failed)?
            .extend(["drives", drive_id, "items", item_id, "content"]);
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
