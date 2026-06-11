use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use super::{GoogleAuth, check_response};
use crate::{ExtractContext, Source, SourceError};

const BASE: &str = "https://www.googleapis.com/drive/v3";

pub struct GoogleDriveSource {
    http: reqwest::Client,
    auth: Arc<GoogleAuth>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoogleDriveParams {
    folder: String,
    file_pattern: String,
    #[serde(default = "default_max_files")]
    max_files: usize,
}

/// Validate this source's params at config-load time, before any network work.
pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    let p: GoogleDriveParams = crate::parse_params(params)?;
    if p.folder.trim().is_empty() {
        return Err(SourceError::InvalidParams(
            "drive `folder` must not be empty (use the Drive folder name or ID)".into(),
        ));
    }
    if p.file_pattern.trim().is_empty() {
        return Err(SourceError::InvalidParams(
            "drive `file_pattern` must not be empty".into(),
        ));
    }
    if p.max_files == 0 {
        return Err(SourceError::InvalidParams(
            "drive `max_files` must be > 0".into(),
        ));
    }
    Ok(())
}

fn default_max_files() -> usize {
    200
}

/// Escape a value for inclusion inside a single-quoted Drive query string literal.
/// Per the Drive API, `\` and `'` must be backslash-escaped; otherwise a folder or
/// filename containing a quote (e.g. `Team's Docs`) produces a malformed query.
fn escape_drive_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

#[derive(Deserialize)]
struct FileList {
    files: Option<Vec<FileMeta>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
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

impl GoogleDriveSource {
    pub fn new(http: reqwest::Client, auth: Arc<GoogleAuth>) -> Self {
        Self { http, auth }
    }

    async fn resolve_folder_id(&self, token: &str, path: &str) -> Result<String, SourceError> {
        let mut parent_id = "root".to_string();
        for segment in path.split('/') {
            if segment.is_empty() {
                continue;
            }
            let q = format!(
                "name = '{}' and '{}' in parents and mimeType = 'application/vnd.google-apps.folder' and trashed = false",
                escape_drive_literal(segment),
                parent_id
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
impl Source for GoogleDriveSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        let p: GoogleDriveParams = crate::parse_params(params)?;

        let token = self.auth.access_token().await?;
        let folder_id = self.resolve_folder_id(&token, &p.folder).await?;

        let name_fragment = p
            .file_pattern
            .replace("{date}", &ctx.target_date.to_string());

        let q = format!(
            "name contains '{}' and '{}' in parents and trashed = false",
            escape_drive_literal(&name_fragment),
            folder_id
        );

        // Paginate to the end of the listing (complete-refetch contract: the daily page is
        // re-rendered from this fetch, so a file beyond one page is silently lost
        // knowledge). `fields` must request `nextPageToken` explicitly — a `files(...)`
        // selector alone strips it from the response and pagination would silently stop.
        // Requested page size; the server may return fewer (or zero) per page.
        // Termination is `paging::page_step` — the rule all listing adapters share.
        const PAGE_SIZE: usize = 100;
        let page_size = PAGE_SIZE.to_string();

        let mut files: Vec<FileMeta> = Vec::new();
        let mut page_token: Option<String> = None;
        let mut pages_fetched = 0usize;

        loop {
            let mut req = self
                .http
                .get(format!("{BASE}/files"))
                .bearer_auth(&token)
                .query(&[
                    ("q", q.as_str()),
                    (
                        "fields",
                        "nextPageToken, files(id,name,mimeType,modifiedTime)",
                    ),
                    ("pageSize", page_size.as_str()),
                ]);
            if let Some(ref pt) = page_token {
                req = req.query(&[("pageToken", pt.as_str())]);
            }

            let resp = check_response(req.send().await?).await?;
            let list: FileList = resp.json().await?;

            files.extend(list.files.unwrap_or_default());
            pages_fetched += 1;

            match crate::paging::page_step(
                files.len(),
                p.max_files,
                list.next_page_token.is_some(),
                pages_fetched,
            ) {
                crate::paging::PageStep::Continue => page_token = list.next_page_token,
                crate::paging::PageStep::Stop { dropped } => {
                    files.truncate(p.max_files);
                    if dropped {
                        tracing::warn!(
                            max = p.max_files,
                            "drive: file cap hit, some files may have been dropped; raise max_files"
                        );
                    }
                    break;
                }
                crate::paging::PageStep::Exhausted => {
                    tracing::warn!(
                        pages = crate::paging::MAX_PAGES,
                        "drive: page budget exhausted before the listing completed; results may be incomplete"
                    );
                    break;
                }
            }
        }

        tracing::info!(count = files.len(), folder = %p.folder, "drive: files found");

        let mut items = Vec::new();
        for file in files {
            let download = async {
                let content_resp = check_response(
                    self.http
                        .get(format!("{BASE}/files/{}", file.id))
                        .bearer_auth(&token)
                        .query(&[("alt", "media")])
                        .send()
                        .await?,
                )
                .await?;
                content_resp.text().await.map_err(SourceError::Http)
            };

            let content = match download.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        file = %file.name,
                        error = %e,
                        "drive: skipping file (download failed)"
                    );
                    continue;
                }
            };

            let Some(ts) = file
                .modified_time
                .as_deref()
                .and_then(|s| s.parse::<jiff::Timestamp>().ok())
            else {
                tracing::warn!(file_id = %file.id, "drive: skipping file with unparseable timestamp");
                continue;
            };

            items.push(RawItem {
                external_id: Some(file.id.clone()),
                title: file.name.clone(),
                body: content,
                url: Some(format!("https://drive.google.com/file/d/{}/view", file.id)),
                author: None,
                timestamp: ts,
                is_self: false,
                metadata: serde_json::json!({
                    "mime_type": file.mime_type,
                }),
            });
        }

        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_quotes_and_backslashes_in_drive_literals() {
        assert_eq!(escape_drive_literal("Team's Docs"), r"Team\'s Docs");
        assert_eq!(escape_drive_literal(r"a\b"), r"a\\b");
        assert_eq!(escape_drive_literal("plain"), "plain");
    }

    #[test]
    fn max_files_defaults_and_rejects_zero() {
        let params: GoogleDriveParams =
            serde_json::from_value(serde_json::json!({"folder": "f", "file_pattern": "p"}))
                .unwrap();
        assert_eq!(params.max_files, 200);
        // A zero cap would silently fetch nothing; validation must refuse it up front.
        let zero = serde_json::json!({"folder": "f", "file_pattern": "p", "max_files": 0});
        assert!(validate_params(&zero).is_err());
        let one = serde_json::json!({"folder": "f", "file_pattern": "p", "max_files": 1});
        assert!(validate_params(&one).is_ok());
    }
}
