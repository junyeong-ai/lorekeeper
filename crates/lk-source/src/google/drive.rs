use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use lk_core::event::RawItem;

use super::text::ContentFormat;
use super::{GoogleAuth, check_response};
use crate::{ExtractContext, Source, SourceError};

const BASE: &str = "https://www.googleapis.com/drive/v3/";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const FILE_FIELDS: &str =
    "id,name,mimeType,modifiedTime,createdTime,version,parents,trashed,headRevisionId";
pub(super) const MAX_CONTENT_BYTES: usize = 16 * 1024 * 1024;

pub struct GoogleDriveSource {
    http: reqwest::Client,
    auth: Arc<GoogleAuth>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoogleDriveParams {
    folder_id: String,
    name_prefix: Option<String>,
    #[serde(default = "default_lookback_days")]
    lookback_days: u32,
    #[serde(default = "default_max_files")]
    max_files: usize,
}

impl GoogleDriveParams {
    fn lookback_hours(&self) -> Result<u32, SourceError> {
        self.lookback_days.checked_mul(24).ok_or_else(|| {
            SourceError::InvalidParams(
                "drive `lookback_days` exceeds the supported day-window range".into(),
            )
        })
    }
}

pub fn validate_params(params: &serde_json::Value) -> Result<(), SourceError> {
    crate::parse_validated::<GoogleDriveParams>(params).map(|_| ())
}

impl crate::ValidatedParams for GoogleDriveParams {
    fn validate(&self) -> Result<(), SourceError> {
        if self.folder_id.trim().is_empty() || self.folder_id != self.folder_id.trim() {
            return Err(SourceError::InvalidParams(
                "drive `folder_id` must be a nonblank Drive folder ID (or root), without surrounding whitespace".into(),
            ));
        }
        if self
            .name_prefix
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(SourceError::InvalidParams(
                "drive `name_prefix` must not be blank when supplied".into(),
            ));
        }
        if self.max_files == 0 {
            return Err(SourceError::InvalidParams(
                "drive `max_files` must be > 0".into(),
            ));
        }
        self.lookback_hours()?;
        Ok(())
    }
}

fn default_lookback_days() -> u32 {
    1
}

fn default_max_files() -> usize {
    200
}

fn escape_drive_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileList {
    #[serde(default)]
    files: Vec<FileMeta>,
    next_page_token: Option<String>,
    #[serde(default)]
    incomplete_search: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct FileMeta {
    id: String,
    name: String,
    mime_type: Option<String>,
    modified_time: Option<String>,
    created_time: Option<String>,
    version: Option<String>,
    #[serde(default)]
    parents: Vec<String>,
    #[serde(default)]
    trashed: bool,
    head_revision_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderMeta {
    id: String,
    mime_type: String,
    drive_id: Option<String>,
    #[serde(default)]
    trashed: bool,
}

struct DriveApi<'a> {
    http: &'a reqwest::Client,
    token: &'a str,
    base: url::Url,
}

impl DriveApi<'_> {
    fn url(&self, segments: &[&str]) -> url::Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("HTTP Drive API base has path segments")
            .pop_if_empty()
            .extend(segments);
        url
    }

    async fn get(
        &self,
        segments: &[&str],
        params: &[(String, String)],
    ) -> Result<reqwest::Response, SourceError> {
        let url = self.url(segments);
        check_response(
            crate::retry::send_with_retry(|| {
                self.http
                    .get(url.clone())
                    .bearer_auth(self.token)
                    .query(params)
                    .send()
            })
            .await?,
        )
        .await
    }

    async fn metadata(&self, id: &str) -> Result<FileMeta, SourceError> {
        Ok(self
            .get(
                &["files", id],
                &[
                    ("fields".into(), FILE_FIELDS.into()),
                    ("supportsAllDrives".into(), "true".into()),
                ],
            )
            .await?
            .json()
            .await?)
    }

    async fn observe(
        &self,
        params: &GoogleDriveParams,
        ctx: &ExtractContext,
    ) -> Result<crate::SourceObservation, SourceError> {
        let folder: FolderMeta = self
            .get(
                &["files", &params.folder_id],
                &[
                    ("fields".into(), "id,mimeType,driveId,trashed".into()),
                    ("supportsAllDrives".into(), "true".into()),
                ],
            )
            .await?
            .json()
            .await?;
        if folder.mime_type != FOLDER_MIME || folder.trashed || folder.id.is_empty() {
            return Err(SourceError::InvalidParams(format!(
                "drive folder_id {:?} does not identify an available folder",
                params.folder_id
            )));
        }
        let (start, end) = ctx.day_window(params.lookback_hours()?, 0)?;
        let query = file_query(params, &folder.id, start, end);
        let mut list_params = vec![
            ("q".into(), query),
            (
                "fields".into(),
                format!("nextPageToken,incompleteSearch,files({FILE_FIELDS})"),
            ),
            ("pageSize".into(), "100".into()),
            ("orderBy".into(), "modifiedTime,name".into()),
            ("spaces".into(), "drive".into()),
            ("supportsAllDrives".into(), "true".into()),
            ("includeItemsFromAllDrives".into(), "true".into()),
        ];
        if let Some(drive_id) = &folder.drive_id {
            list_params.push(("corpora".into(), "drive".into()));
            list_params.push(("driveId".into(), drive_id.clone()));
        } else {
            list_params.push(("corpora".into(), "user".into()));
        }

        let mut files = BTreeMap::new();
        let mut partial = false;
        let mut page_token: Option<String> = None;
        for pages_fetched in 1..=crate::paging::MAX_PAGES {
            let mut request_params = list_params.clone();
            if let Some(token) = &page_token {
                request_params.push(("pageToken".into(), token.clone()));
            }
            let list: FileList = self.get(&["files"], &request_params).await?.json().await?;
            if list.incomplete_search {
                tracing::warn!("drive: provider reported incomplete search results");
                partial = true;
            }
            for file in list.files {
                if let Some(previous) = files.get(&file.id) {
                    if previous != &file {
                        return Err(SourceError::Parse(format!(
                            "drive file {:?} changed across listing pages; rerun ingestion",
                            file.id
                        )));
                    }
                } else {
                    files.insert(file.id.clone(), file);
                }
            }
            match crate::paging::page_step(
                files.len(),
                params.max_files,
                list.next_page_token.is_some(),
                pages_fetched,
            ) {
                crate::paging::PageStep::Continue => page_token = list.next_page_token,
                crate::paging::PageStep::Stop { dropped } => {
                    if dropped {
                        partial = true;
                        tracing::warn!(
                            max = params.max_files,
                            "drive: file cap left part of the folder query unobserved; raise max_files"
                        );
                    }
                    break;
                }
                crate::paging::PageStep::Exhausted => {
                    partial = true;
                    tracing::warn!("drive: page budget exhausted before listing completed");
                    break;
                }
            }
        }

        let listed = files.len().min(params.max_files);
        let mut items = Vec::new();
        for file in files.into_values().take(params.max_files) {
            match self.capture(&file, &folder.id, start, end).await {
                Ok(item) => items.push(item),
                Err(error) => {
                    partial = true;
                    tracing::warn!(file_id = %file.id, file = %file.name, %error, "drive: file not captured");
                }
            }
        }
        crate::require_any_observation("listed Drive file", items.len(), listed)?;
        Ok(crate::SourceObservation {
            items,
            scope: crate::ObservationScope::Window { start, end },
            outcome: if partial {
                crate::ObservationOutcome::Partial
            } else {
                crate::ObservationOutcome::Observed
            },
        })
    }

    async fn capture(
        &self,
        file: &FileMeta,
        folder_id: &str,
        start: jiff::Timestamp,
        end: jiff::Timestamp,
    ) -> Result<RawItem, SourceError> {
        let projection = ContentProjection::for_file(file)?;
        let modified = file
            .modified_time
            .as_deref()
            .and_then(|value| value.parse::<jiff::Timestamp>().ok())
            .ok_or_else(|| SourceError::Parse("Drive file has no valid modifiedTime".into()))?;
        if modified < start || modified >= end {
            return Err(SourceError::Parse(
                "Drive file modifiedTime is outside the requested window".into(),
            ));
        }
        if file.id.is_empty()
            || file.name.trim().is_empty()
            || file.version.as_deref().is_none_or(str::is_empty)
            || file.trashed
            || !file.parents.iter().any(|parent| parent == folder_id)
        {
            return Err(SourceError::Parse(
                "Drive file lacks a stable ID, title, version, or selected folder membership"
                    .into(),
            ));
        }
        let params = if let Some(mime_type) = projection.export_mime {
            vec![("mimeType".into(), mime_type.into())]
        } else {
            vec![
                ("alt".into(), "media".into()),
                ("supportsAllDrives".into(), "true".into()),
            ]
        };
        let mut segments = vec!["files", file.id.as_str()];
        if projection.export_mime.is_some() {
            segments.push("export");
        }
        let response = self.get(&segments, &params).await?;
        let response_content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let (content, digest) = read_content(response).await?;
        let after = self.metadata(&file.id).await?;
        if *file != after {
            return Err(SourceError::Parse(
                "Drive file metadata/version changed during capture; rerun ingestion".into(),
            ));
        }
        let body = projection.format.markdown(&content)?;
        let identity = lk_core::source_identity::SourceIdentity::Provider {
            provider: lk_core::config::SourceType::GoogleDrive,
            namespace: "files".into(),
            document: file.id.clone(),
        };
        Ok(RawItem {
            labels: Vec::new(),
            external_id: Some(file.id.clone()),
            title: file.name.clone(),
            body,
            url: Some(format!("https://drive.google.com/file/d/{}/view", file.id)),
            author: None,
            timestamp: modified,
            is_self: false,
            work: None,
            metadata: serde_json::json!({
                "source_identity": identity,
                "source_record": {
                    "provider": "google-drive",
                    "id": file.id,
                    "version": file.version,
                    "mime_type": file.mime_type,
                    "modified_time": file.modified_time,
                    "created_time": file.created_time,
                    "head_revision_id": file.head_revision_id,
                    "folder_id": folder_id,
                    "observation": {"kind": "modified-time-window", "start": start, "end": end},
                    "extraction": {
                        "method": if projection.export_mime.is_some() { "export" } else { "media" },
                        "mime_type": projection.export_mime.or(file.mime_type.as_deref()),
                        "response_content_type": response_content_type,
                        "scope": "document-text",
                        "omissions": ["embedded-media", "comments", "revision-history"],
                    },
                    "content_digest": {"scheme": "blake3", "value": digest},
                },
            }),
        })
    }
}

fn file_query(
    params: &GoogleDriveParams,
    folder_id: &str,
    start: jiff::Timestamp,
    end: jiff::Timestamp,
) -> String {
    let mut query = format!(
        "'{}' in parents and trashed = false and mimeType != '{FOLDER_MIME}' and modifiedTime >= '{start}' and modifiedTime < '{end}'",
        escape_drive_literal(folder_id),
    );
    if let Some(name) = &params.name_prefix {
        query.push_str(&format!(
            " and name contains '{}'",
            escape_drive_literal(name)
        ));
    }
    query
}

struct ContentProjection {
    export_mime: Option<&'static str>,
    format: ContentFormat,
}

impl ContentProjection {
    fn for_file(file: &FileMeta) -> Result<Self, SourceError> {
        let (export_mime, format) = match file.mime_type.as_deref() {
            Some("application/vnd.google-apps.document") => {
                (Some("text/markdown"), ContentFormat::Markdown)
            }
            Some("application/vnd.google-apps.presentation") => {
                (Some("text/plain"), ContentFormat::Plain)
            }
            Some(
                mime_type @ ("text/plain" | "text/markdown" | "text/x-markdown" | "text/html"),
            ) => (None, ContentFormat::from_mime(mime_type)?),
            mime_type => {
                return Err(SourceError::Parse(format!(
                    "unsupported Drive MIME type {mime_type:?}; use an explicit supported text export for binary files, spreadsheets, shortcuts, or media"
                )));
            }
        };
        Ok(Self {
            export_mime,
            format,
        })
    }
}

pub(super) async fn read_content(
    mut response: reqwest::Response,
) -> Result<(String, String), SourceError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CONTENT_BYTES as u64)
    {
        return Err(SourceError::Parse(format!(
            "Drive text exceeds {MAX_CONTENT_BYTES} bytes"
        )));
    }
    let charset = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .and_then(|mime| {
            mime.get_param(mime::CHARSET)
                .map(|charset| charset.as_str().to_owned())
        });
    let encoding = if let Some(charset) = &charset {
        encoding_rs::Encoding::for_label(charset.as_bytes()).ok_or_else(|| {
            SourceError::Parse(format!("unsupported Drive text charset {charset:?}"))
        })?
    } else {
        encoding_rs::UTF_8
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > MAX_CONTENT_BYTES - bytes.len() {
            return Err(SourceError::Parse(format!(
                "Drive text exceeds {MAX_CONTENT_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    let (text, _, malformed) = encoding.decode(&bytes);
    if malformed {
        return Err(SourceError::Parse(
            "Drive content cannot be decoded without replacement characters".into(),
        ));
    }
    Ok((text.into_owned(), blake3::hash(&bytes).to_hex().to_string()))
}

impl GoogleDriveSource {
    pub fn new(http: reqwest::Client, auth: Arc<GoogleAuth>) -> Self {
        Self { http, auth }
    }
}

#[async_trait]
impl Source for GoogleDriveSource {
    async fn extract(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<Vec<RawItem>, SourceError> {
        Ok(self.observe(params, ctx).await?.items)
    }

    async fn observe(
        &self,
        params: &serde_json::Value,
        ctx: &ExtractContext,
    ) -> Result<crate::SourceObservation, SourceError> {
        let params = crate::parse_validated::<GoogleDriveParams>(params)?;
        let token = self.auth.access_token().await?;
        DriveApi {
            http: &self.http,
            token: &token,
            base: url::Url::parse(BASE).expect("valid Drive API URL"),
        }
        .observe(&params, ctx)
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn params_use_explicit_folder_identity_and_literal_name_selection() {
        let valid = serde_json::json!({"folder_id": "root"});
        let params: GoogleDriveParams = crate::parse_validated(&valid).unwrap();
        assert_eq!(params.max_files, 200);
        assert_eq!(params.lookback_days, 1);
        for invalid in [
            serde_json::json!({"folder": "Docs", "file_pattern": "briefing-{date}.md"}),
            serde_json::json!({"folder_id": ""}),
            serde_json::json!({"folder_id": " root"}),
            serde_json::json!({"folder_id": "root", "name_prefix": " "}),
            serde_json::json!({"folder_id": "root", "max_files": 0}),
            serde_json::json!({"folder_id": "root", "lookback_days": -1}),
            serde_json::json!({"folder_id": "root", "lookback_days": "1"}),
            serde_json::json!({"folder_id": "root", "lookback_days": u32::MAX}),
        ] {
            assert!(validate_params(&invalid).is_err());
        }
        let params: GoogleDriveParams = crate::parse_validated(&serde_json::json!({
            "folder_id": "root", "name_prefix": "Team's \\ {date}", "max_files": 1,
        }))
        .unwrap();
        let query = file_query(
            &params,
            "selected-id",
            "2026-10-18T15:00:00Z".parse().unwrap(),
            "2026-10-19T15:00:00Z".parse().unwrap(),
        );
        assert!(query.contains("'selected-id' in parents"));
        assert!(query.contains(r"name contains 'Team\'s \\ {date}'"));
        assert!(query.contains("modifiedTime >= '2026-10-18T15:00:00Z'"));
        assert!(query.contains("modifiedTime < '2026-10-19T15:00:00Z'"));
    }

    struct TestServer {
        base: url::Url,
        requests: Arc<Mutex<Vec<url::Url>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn server(respond: impl Fn(&url::Url) -> Vec<u8> + Send + Sync + 'static) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base: url::Url = format!("http://{}/drive/v3/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let server_base = base.clone();
        let task = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read(&mut byte).await.unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                    assert!(request.len() < 16 * 1024);
                }
                let request = std::str::from_utf8(&request).unwrap();
                let target = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let url = server_base.join(target).unwrap();
                captured.lock().unwrap().push(url.clone());
                let response = respond(&url);
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            }
        });
        TestServer {
            base,
            requests,
            task,
        }
    }

    fn response(content_type: &str, body: impl AsRef<[u8]>) -> Vec<u8> {
        let body = body.as_ref();
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn json_response(value: serde_json::Value) -> Vec<u8> {
        response("application/json", serde_json::to_vec(&value).unwrap())
    }

    fn context() -> ExtractContext {
        ExtractContext {
            target_date: jiff::civil::date(2026, 10, 19),
            timezone: jiff::tz::TimeZone::get("Asia/Seoul").unwrap(),
            locale: lk_core::i18n::Locale::En,
            identity: lk_core::config::Identity {
                name: "Test".into(),
                email: "test@example.com".into(),
                slack_id: None,
            },
            vault_root: ".".into(),
            vault_dirs: Default::default(),
            documents: None,
            projection_cache: None,
            originals: Default::default(),
        }
    }

    fn file(id: &str, mime: &str, version: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": format!("{id}.md"),
            "mimeType": mime,
            "version": version,
            "createdTime": "2026-01-01T00:00:00Z",
            "modifiedTime": "2026-10-19T03:00:00Z",
            "parents": ["folder-id"],
        })
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap()
    }

    fn params(max_files: usize) -> GoogleDriveParams {
        GoogleDriveParams {
            folder_id: "folder-id".into(),
            name_prefix: None,
            lookback_days: default_lookback_days(),
            max_files,
        }
    }

    #[tokio::test]
    async fn relative_remote_dependencies_do_not_become_vault_addresses() {
        let mut accepted = Vec::new();
        for (mime, original) in [
            ("text/markdown", "[Remote](../concepts/owned.md)"),
            ("text/html", "<a href=\"../concepts/owned.md\">Remote</a>"),
        ] {
            let doc = file("doc", mime, "1");
            let metadata = doc.clone();
            let server = server(move |url| {
                if url
                    .query_pairs()
                    .any(|(key, value)| key == "alt" && value == "media")
                {
                    response(mime, original)
                } else {
                    json_response(metadata.clone())
                }
            })
            .await;
            let http = client();
            let api = DriveApi {
                http: &http,
                token: "fixture",
                base: server.base.clone(),
            };
            let file: FileMeta = serde_json::from_value(doc).unwrap();
            let (start, end) = context().day_window(24, 0).unwrap();
            if let Ok(item) = api.capture(&file, "folder-id", start, end).await {
                let links = lk_core::link::extract_page_links(&item.body);
                accepted.push((
                    mime,
                    links
                        .into_iter()
                        .map(|link| {
                            lk_core::link::resolve_dest(
                                std::path::Path::new("wiki/documents/report.md"),
                                &link.dest,
                            )
                        })
                        .collect::<Vec<_>>(),
                ));
            }
        }
        assert!(
            accepted.is_empty(),
            "unresolved remote dependencies were accepted as vault addresses: {accepted:?}"
        );
    }

    #[tokio::test]
    async fn plain_projection_keeps_markdown_and_html_as_literal_source_text() {
        let original = "[Literal](ghost.md) <em>tag</em>";
        for mime in ["application/vnd.google-apps.presentation", "text/plain"] {
            let doc = file("doc", mime, "1");
            let metadata = doc.clone();
            let server = server(move |url| {
                if url.path().ends_with("/export")
                    || url
                        .query_pairs()
                        .any(|(key, value)| key == "alt" && value == "media")
                {
                    response("text/plain", original)
                } else {
                    json_response(metadata.clone())
                }
            })
            .await;
            let http = client();
            let api = DriveApi {
                http: &http,
                token: "fixture",
                base: server.base.clone(),
            };
            let file: FileMeta = serde_json::from_value(doc).unwrap();
            let (start, end) = context().day_window(24, 0).unwrap();
            let item = api.capture(&file, "folder-id", start, end).await.unwrap();
            assert!(
                lk_core::link::extract_dests(&item.body).is_empty(),
                "{mime}: {}",
                item.body
            );
            let visible = pulldown_cmark::Parser::new(&item.body)
                .filter_map(|event| match event {
                    pulldown_cmark::Event::Text(text) => Some(text.into_string()),
                    _ => None,
                })
                .collect::<String>();
            assert_eq!(visible, original);
            assert_eq!(
                item.metadata["source_record"]["content_digest"]["value"],
                blake3::hash(original.as_bytes()).to_hex().to_string()
            );
            assert_eq!(
                item.metadata["source_record"]["extraction"]["mime_type"],
                "text/plain"
            );
            assert_eq!(
                item.metadata["source_record"]["extraction"]["response_content_type"],
                "text/plain"
            );
        }
    }

    #[tokio::test]
    async fn requested_markdown_export_remains_distinct_from_response_content_type() {
        let doc = file("doc", "application/vnd.google-apps.document", "1");
        let metadata = doc.clone();
        let original = "# Heading\n\n[Related](https://example.com/related.md)\n";
        let server = server(move |url| {
            if url.path().ends_with("/export") {
                response("text/plain; charset=UTF-8", original)
            } else {
                json_response(metadata.clone())
            }
        })
        .await;
        let http = client();
        let api = DriveApi {
            http: &http,
            token: "fixture",
            base: server.base.clone(),
        };
        let file: FileMeta = serde_json::from_value(doc).unwrap();
        let (start, end) = context().day_window(24, 0).unwrap();
        let item = api.capture(&file, "folder-id", start, end).await.unwrap();
        assert_eq!(item.body, original);
        let extraction = &item.metadata["source_record"]["extraction"];
        assert_eq!(extraction["mime_type"], "text/markdown");
        assert_eq!(
            extraction["response_content_type"],
            "text/plain; charset=UTF-8"
        );
    }

    #[tokio::test]
    async fn exports_docs_and_follows_empty_pages_in_the_actual_shared_drive_window() {
        let mut doc = file("doc", "application/vnd.google-apps.document", "31");
        doc["modifiedTime"] = "2026-10-18T06:00:00Z".into();
        let expected = doc.clone();
        let server = server(move |url| match url.path() {
            "/drive/v3/files/folder-id" => json_response(serde_json::json!({
                "id": "folder-id", "mimeType": FOLDER_MIME, "driveId": "shared-drive-id",
            })),
            "/drive/v3/files" => {
                if url
                    .query_pairs()
                    .any(|(key, value)| key == "pageToken" && value == "next")
                {
                    json_response(serde_json::json!({"files": [doc]}))
                } else {
                    json_response(serde_json::json!({"files": [], "nextPageToken": "next"}))
                }
            }
            "/drive/v3/files/doc/export" => response(
                "text/markdown; charset=UTF-8",
                "# 지식\n\n근거 [문서](https://example.com).\n",
            ),
            "/drive/v3/files/doc" => json_response(doc.clone()),
            other => panic!("unexpected request {other}"),
        })
        .await;
        let http = client();
        let api = DriveApi {
            http: &http,
            token: "fixture-token",
            base: server.base.clone(),
        };
        let observation = api.observe(&params(1), &context()).await.unwrap();
        assert_eq!(observation.outcome, crate::ObservationOutcome::Observed);
        let (start, end) = context().day_window(24, 0).unwrap();
        assert_eq!(
            observation.scope,
            crate::ObservationScope::Window { start, end }
        );
        let [item] = observation.items.as_slice() else {
            panic!("expected one document")
        };
        assert_eq!(item.body, "# 지식\n\n근거 [문서](https://example.com).\n");
        assert_eq!(item.external_id.as_deref(), Some("doc"));
        assert_eq!(item.timestamp.to_string(), "2026-10-18T06:00:00Z");
        assert_eq!(
            item.metadata["source_identity"],
            serde_json::json!({
                "provider": "google-drive", "namespace": "files", "document": "doc",
            })
        );
        let record = &item.metadata["source_record"];
        assert_eq!(record["version"], expected["version"]);
        assert_eq!(record["extraction"]["method"], "export");
        assert_eq!(record["extraction"]["mime_type"], "text/markdown");
        assert_eq!(
            record["extraction"]["response_content_type"],
            "text/markdown; charset=UTF-8"
        );
        assert_eq!(record["extraction"]["scope"], "document-text");
        assert_eq!(
            record["content_digest"]["value"],
            blake3::hash(item.body.as_bytes()).to_hex().to_string()
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        let listing = requests
            .iter()
            .find(|url| url.path() == "/drive/v3/files")
            .unwrap();
        let query: BTreeMap<_, _> = listing.query_pairs().into_owned().collect();
        assert!(query["q"].contains("modifiedTime >= '2026-10-17T15:00:00Z'"));
        assert!(query["q"].contains("modifiedTime < '2026-10-19T15:00:00Z'"));
        assert_eq!(query["corpora"], "drive");
        assert_eq!(query["driveId"], "shared-drive-id");
        assert!(query["fields"].contains("incompleteSearch"));
        let export = requests
            .iter()
            .find(|url| url.path().ends_with("/export"))
            .unwrap();
        assert!(
            export
                .query_pairs()
                .any(|(key, value)| key == "mimeType" && value == "text/markdown")
        );
        assert!(!export.query_pairs().any(|(key, _)| key == "alt"));
    }

    #[tokio::test]
    async fn lookback_override_and_historical_targets_use_exact_reported_query_bounds() {
        for (target, lookback_days, expected_start, expected_end) in [
            (
                jiff::civil::date(2026, 10, 19),
                0,
                "2026-10-18T15:00:00Z",
                "2026-10-19T15:00:00Z",
            ),
            (
                jiff::civil::date(2026, 9, 20),
                3,
                "2026-09-16T15:00:00Z",
                "2026-09-20T15:00:00Z",
            ),
        ] {
            let server = server(|url| match url.path() {
                "/drive/v3/files/folder-id" => {
                    json_response(serde_json::json!({"id": "folder-id", "mimeType": FOLDER_MIME}))
                }
                "/drive/v3/files" => json_response(serde_json::json!({"files": []})),
                other => panic!("unexpected request {other}"),
            })
            .await;
            let params: GoogleDriveParams = crate::parse_validated(&serde_json::json!({
                "folder_id": "folder-id", "lookback_days": lookback_days,
            }))
            .unwrap();
            let mut context = context();
            context.target_date = target;
            let http = client();
            let observation = DriveApi {
                http: &http,
                token: "fixture-token",
                base: server.base.clone(),
            }
            .observe(&params, &context)
            .await
            .unwrap();
            assert_eq!(observation.outcome, crate::ObservationOutcome::Observed);
            assert_eq!(
                observation.scope,
                crate::ObservationScope::Window {
                    start: expected_start.parse().unwrap(),
                    end: expected_end.parse().unwrap(),
                }
            );
            let requests = server.requests.lock().unwrap();
            let listing = requests
                .iter()
                .find(|url| url.path() == "/drive/v3/files")
                .unwrap();
            let query: BTreeMap<_, _> = listing.query_pairs().into_owned().collect();
            assert!(query["q"].contains(&format!("modifiedTime >= '{expected_start}'")));
            assert!(query["q"].contains(&format!("modifiedTime < '{expected_end}'")));
        }
    }

    #[tokio::test]
    async fn reports_truncation_provider_gaps_and_unreadable_files_without_fabricating_completion()
    {
        for case in [
            "cap",
            "incomplete",
            "unsupported",
            "changed",
            "missing-version",
            "relative-dependency",
            "empty",
        ] {
            let case = case.to_string();
            let current_case = case.clone();
            let doc = file("a-doc", "text/plain", "2");
            let server = server(move |url| match url.path() {
                "/drive/v3/files/folder-id" => {
                    json_response(serde_json::json!({"id": "folder-id", "mimeType": FOLDER_MIME}))
                }
                "/drive/v3/files" => {
                    let mut listing = serde_json::json!({"files": [doc]});
                    match current_case.as_str() {
                        "cap" => listing["nextPageToken"] = "pending".into(),
                        "incomplete" => listing["incompleteSearch"] = true.into(),
                        "unsupported" => listing["files"].as_array_mut().unwrap().push(file(
                            "z-binary",
                            "application/pdf",
                            "1",
                        )),
                        "changed" => listing["files"].as_array_mut().unwrap().push(file(
                            "z-changing",
                            "text/plain",
                            "1",
                        )),
                        "missing-version" => {
                            let mut missing = file("z-missing", "text/plain", "1");
                            missing.as_object_mut().unwrap().remove("version");
                            listing["files"].as_array_mut().unwrap().push(missing);
                        }
                        "relative-dependency" => listing["files"]
                            .as_array_mut()
                            .unwrap()
                            .push(file("z-relative", "text/markdown", "1")),
                        "empty" => listing["files"] = serde_json::json!([]),
                        _ => unreachable!(),
                    }
                    json_response(listing)
                }
                "/drive/v3/files/a-doc" => {
                    if url
                        .query_pairs()
                        .any(|(key, value)| key == "alt" && value == "media")
                    {
                        response("text/plain", "retained observation")
                    } else {
                        json_response(doc.clone())
                    }
                }
                "/drive/v3/files/z-changing" => {
                    if url
                        .query_pairs()
                        .any(|(key, value)| key == "alt" && value == "media")
                    {
                        response("text/plain", "unverified changing observation")
                    } else {
                        json_response(file("z-changing", "text/plain", "2"))
                    }
                }
                "/drive/v3/files/z-relative" => {
                    if url
                        .query_pairs()
                        .any(|(key, value)| key == "alt" && value == "media")
                    {
                        response("text/markdown", "[Remote](../concepts/owned.md)")
                    } else {
                        json_response(file("z-relative", "text/markdown", "1"))
                    }
                }
                other => panic!("unexpected request {other}"),
            })
            .await;
            let http = client();
            let api = DriveApi {
                http: &http,
                token: "fixture-token",
                base: server.base.clone(),
            };
            let observation = api
                .observe(&params(if case == "cap" { 1 } else { 10 }), &context())
                .await
                .unwrap();
            if case == "empty" {
                assert!(observation.items.is_empty());
                assert_eq!(observation.outcome, crate::ObservationOutcome::Observed);
            } else {
                assert_eq!(
                    observation.outcome,
                    crate::ObservationOutcome::Partial,
                    "{case}"
                );
                assert_eq!(observation.items.len(), 1, "{case}");
                assert_eq!(observation.items[0].title, "a-doc.md", "{case}");
            }
        }
    }

    #[tokio::test]
    async fn all_unreadable_is_an_error_and_nonfolders_are_not_accepted() {
        for mime in [FOLDER_MIME, "text/plain"] {
            let server = server(move |url| match url.path() {
                "/drive/v3/files/folder-id" => {
                    json_response(serde_json::json!({"id": "folder-id", "mimeType": mime}))
                }
                "/drive/v3/files" => json_response(
                    serde_json::json!({"files": [file("binary", "application/pdf", "1")]}),
                ),
                other => panic!("unexpected request {other}"),
            })
            .await;
            let http = client();
            let api = DriveApi {
                http: &http,
                token: "fixture-token",
                base: server.base.clone(),
            };
            let error = api
                .observe(&params(10), &context())
                .await
                .err()
                .expect("must refuse unreadable source");
            if mime == FOLDER_MIME {
                assert!(matches!(
                    error,
                    SourceError::NothingObserved { attempted: 1, .. }
                ));
            } else {
                assert!(matches!(error, SourceError::InvalidParams(_)));
            }
        }
    }

    #[tokio::test]
    async fn text_decoding_is_bounded_and_never_replaces_undecodable_bytes() {
        let (korean, _, _) = encoding_rs::EUC_KR.encode("한글 근거");
        let korean = korean.into_owned();
        let server = server(move |url| match url.path() {
            "/encoded" => response("text/plain; charset=EUC-KR", &korean),
            "/malformed" => response("text/plain; charset=UTF-8", [0xff]),
            "/oversized" => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", MAX_CONTENT_BYTES + 1).into_bytes(),
            "/chunked" => {
                let bytes = vec![b'x'; MAX_CONTENT_BYTES + 1];
                let mut response = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n", bytes.len()).into_bytes();
                response.extend(bytes);
                response.extend_from_slice(b"\r\n0\r\n\r\n");
                response
            }
            other => panic!("unexpected request {other}"),
        }).await;
        let http = client();
        for endpoint in ["encoded", "malformed", "oversized", "chunked"] {
            let response = http
                .get(server.base.join(&format!("/{endpoint}")).unwrap())
                .send()
                .await
                .unwrap();
            let result = read_content(response).await;
            if endpoint == "encoded" {
                assert_eq!(result.unwrap().0, "한글 근거");
            } else {
                assert!(result.is_err(), "{endpoint}");
            }
        }
    }
}
