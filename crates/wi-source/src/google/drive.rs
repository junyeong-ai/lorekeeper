use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use wi_core::config::SourceType;
use wi_core::event::RawItem;

use super::{GoogleAuth, check_response};
use crate::{ExtractContext, Source, SourceError};

const BASE: &str = "https://www.googleapis.com/drive/v3";

pub struct DriveSource {
    http: reqwest::Client,
    auth: Arc<GoogleAuth>,
}

#[derive(Debug, Deserialize)]
struct DriveParams {
    folder: String,
    file_pattern: String,
}

#[derive(Deserialize)]
struct FileList {
    files: Option<Vec<FileMeta>>,
}

#[derive(Deserialize)]
struct FileMeta {
    id: String,
    name: String,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(rename = "modifiedTime")]
    modified_time: Option<String>,
}

impl DriveSource {
    pub fn new(http: reqwest::Client, auth: Arc<GoogleAuth>) -> Self {
        Self { http, auth }
    }

    async fn find_folder_id(&self, token: &str, path: &str) -> Result<String, SourceError> {
        let mut parent_id = "root".to_string();
        for segment in path.split('/') {
            if segment.is_empty() {
                continue;
            }
            let q = format!(
                "name = '{}' and '{}' in parents and mimeType = 'application/vnd.google-apps.folder' and trashed = false",
                segment, parent_id
            );
            let resp = check_response(
                self.http
                    .get(format!("{BASE}/files"))
                    .bearer_auth(token)
                    .query(&[("q", &q), ("fields", &"files(id,name)".to_string())])
                    .send()
                    .await?,
            )
            .await?;

            let list: FileList = resp.json().await?;
            parent_id = list
                .files
                .and_then(|f| f.into_iter().next())
                .map(|f| f.id)
                .ok_or_else(|| {
                    SourceError::Parse(format!("folder not found: {segment} in {path}"))
                })?;
        }
        Ok(parent_id)
    }
}

#[async_trait]
impl Source for DriveSource {
    fn source_type(&self) -> SourceType {
        SourceType::GoogleDrive
    }

    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: DriveParams = serde_json::from_value(params.clone())
            .map_err(|e| SourceError::InvalidParams(e.to_string()))?;

        let token = self.auth.access_token().await?;
        let folder_id = self.find_folder_id(&token, &p.folder).await?;

        let name_fragment = p
            .file_pattern
            .replace("{date}", &ctx.target_date.to_string());

        let q = format!(
            "name contains '{}' and '{}' in parents and trashed = false",
            name_fragment, folder_id
        );

        let resp = check_response(
            self.http
                .get(format!("{BASE}/files"))
                .bearer_auth(&token)
                .query(&[
                    ("q", q.as_str()),
                    ("fields", "files(id,name,mimeType,modifiedTime)"),
                    ("pageSize", "10"),
                ])
                .send()
                .await?,
        )
        .await?;

        let list: FileList = resp.json().await?;
        let files = list.files.unwrap_or_default();

        tracing::info!(count = files.len(), folder = %p.folder, "drive: files found");

        let mut items = Vec::new();
        for file in files {
            let content_resp = check_response(
                self.http
                    .get(format!("{BASE}/files/{}", file.id))
                    .bearer_auth(&token)
                    .query(&[("alt", "media")])
                    .send()
                    .await?,
            )
            .await?;
            let content = content_resp.text().await?;

            let ts = file
                .modified_time
                .as_deref()
                .and_then(|s| s.parse::<jiff::Timestamp>().ok())
                .unwrap_or_else(jiff::Timestamp::now);

            items.push(RawItem {
                external_id: Some(file.id.clone()),
                title: file.name.clone(),
                body: content,
                url: Some(format!("https://drive.google.com/file/d/{}/view", file.id)),
                author: None,
                timestamp: ts,
                metadata: serde_json::json!({
                    "mime_type": file.mime_type,
                }),
            });
        }

        Ok(items)
    }
}
