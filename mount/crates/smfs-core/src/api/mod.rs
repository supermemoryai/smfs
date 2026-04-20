//! Supermemory HTTP API client.
//!
//! Typed wrapper over the Supermemory REST endpoints. One client per mount,
//! one mount per container tag. Retries network errors and 5xx with
//! exponential backoff; surfaces 4xx as typed errors without retrying.

pub mod dto;
pub mod error;

pub use dto::*;
pub use error::ApiError;

use reqwest::{Client, RequestBuilder, Response, StatusCode};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const MAX_RETRIES: u32 = 5;
const INITIAL_BACKOFF_MS: u64 = 100;

pub struct ApiClient {
    http: Client,
    base_url: String,
    api_key: String,
    container_tag: String,
    /// User ID owning the API key (from `GET /v3/session`). Stamped into
    /// `metadata.lastEditedBy` on every mount-originated write. `None` if
    /// session lookup failed at mount startup — writes still go through
    /// with just `metadata.source`.
    user_id: Option<String>,
    /// Count of write-side HTTP calls (POST/PATCH/DELETE) dispatched by this
    /// client. Used by tests to verify coalescing behavior; safe to ignore in
    /// production — it's a simple atomic counter.
    write_calls: AtomicU32,
}

/// Result of `ApiClient::validate_key`. Carries everything we extract from
/// `/v3/session` that the mount code cares about.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub org_name: String,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub user_email: Option<String>,
    pub plan: Option<String>,
}

impl std::fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClient")
            .field("base_url", &self.base_url)
            .field("container_tag", &self.container_tag)
            .finish_non_exhaustive()
    }
}

impl ApiClient {
    pub fn new(base_url: &str, api_key: &str, container_tag: &str) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");

        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            container_tag: container_tag.to_string(),
            user_id: None,
            write_calls: AtomicU32::new(0),
        }
    }

    /// Attach the user ID extracted from `/v3/session`. The mount CLI
    /// fetches this at startup; tests may leave it unset to verify the
    /// graceful degradation path.
    pub fn with_user_id(mut self, user_id: String) -> Self {
        self.user_id = Some(user_id);
        self
    }

    pub fn container_tag(&self) -> &str {
        &self.container_tag
    }

    /// Number of write-side HTTP calls (POST/PATCH/DELETE) made by this
    /// client since the counter was last read. Cheap atomic — intended for
    /// test assertions on coalescing behavior.
    pub fn write_calls(&self) -> u32 {
        self.write_calls.load(Ordering::Relaxed)
    }

    /// Validate an API key by hitting GET /v3/session.
    /// This is a standalone function (no ApiClient instance needed).
    pub async fn validate_key(base_url: &str, api_key: &str) -> Result<SessionInfo, ApiError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(ApiError::Network)?;

        let url = format!("{}/v3/session", base_url.trim_end_matches('/'));
        let resp = http
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
            .map_err(ApiError::Network)?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ApiError::Auth);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Server {
                status: status.as_u16(),
                body,
            });
        }

        let body: serde_json::Value = resp.json().await.map_err(ApiError::Network)?;
        let org_name = body
            .get("org")
            .and_then(|o| o.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown")
            .to_string();
        let user_id = body
            .get("user")
            .and_then(|u| u.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let user_name = body
            .get("user")
            .and_then(|u| u.get("name"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let user_email = body
            .get("user")
            .and_then(|u| u.get("email"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let plan = body.get("plan").and_then(|v| v.as_str()).map(String::from);

        Ok(SessionInfo {
            org_name,
            user_id,
            user_name,
            user_email,
            plan,
        })
    }

    /// List documents, optionally filtered by filepath prefix or exact match.
    pub async fn list_documents(&self, filepath: Option<&str>) -> Result<Vec<Document>, ApiError> {
        let mut page = 1u32;
        let mut all = Vec::new();

        loop {
            let body = ListDocumentsReq {
                container_tags: vec![self.container_tag.clone()],
                filepath: filepath.map(String::from),
                limit: 100,
                page,
                include_content: Some(true),
                sort: None,
                order: None,
            };

            let resp = self.list_documents_page(&body).await?;
            let count = resp.memories.len();
            all.extend(resp.memories);

            if page >= resp.pagination.total_pages || count == 0 {
                break;
            }
            page += 1;
        }

        Ok(all)
    }

    /// Fetch a single page from /v3/documents/list. Caller owns the request
    /// shape (sort, order, limit, page). Used by the sync engine to walk
    /// updatedAt-sorted pages and stop early at the watermark.
    pub async fn list_documents_page(
        &self,
        body: &ListDocumentsReq,
    ) -> Result<ListDocumentsResp, ApiError> {
        self.post_read("/v3/documents/list")
            .json(body)
            .send_with_retry()
            .await?
            .parse_json()
            .await
    }

    /// Get a single document by ID or customId.
    pub async fn get_document(&self, id: &str) -> Result<Document, ApiError> {
        self.get(&format!("/v3/documents/{id}"))
            .send_with_retry()
            .await?
            .parse_json()
            .await
    }

    /// List every document in this container tag whose server-side
    /// processing isn't yet `done` (or `failed`). Returns up to the
    /// server's 4-hour staleness cutoff.
    ///
    /// One bulk request replaces N per-id GETs in the inflight poller.
    pub async fn get_processing_documents(&self) -> Result<Vec<Document>, ApiError> {
        let path = format!(
            "/v3/documents/processing?containerTag={}",
            self.container_tag
        );
        let resp: ProcessingDocumentsResp = self
            .get(&path)
            .send_with_retry()
            .await?
            .parse_json()
            .await?;
        Ok(resp.documents)
    }

    /// Create a new document with content and filepath. The request is
    /// stamped with `metadata.source = "supermemoryfs"` (plus
    /// `metadata.lastEditedBy` when a user id is available) for attribution.
    pub async fn create_document(
        &self,
        content: &str,
        filepath: &str,
    ) -> Result<CreateDocumentResp, ApiError> {
        let body = CreateDocumentReq {
            content: content.to_string(),
            filepath: Some(filepath.to_string()),
            container_tag: self.container_tag.clone(),
            metadata: Some(self.mount_metadata()),
        };

        self.post("/v3/documents")
            .json(&body)
            .send_with_retry()
            .await?
            .parse_json()
            .await
    }

    /// POST a binary to `/v3/documents/file` via multipart. No in-request
    /// retry — push-queue backoff handles transient failures.
    pub async fn create_document_multipart(
        &self,
        bytes: &[u8],
        filepath: &str,
        mime: &str,
        filename: &str,
    ) -> Result<CreateDocumentResp, ApiError> {
        self.write_calls.fetch_add(1, Ordering::Relaxed);

        let part = reqwest::multipart::Part::bytes(bytes.to_vec())
            .file_name(filename.to_string())
            .mime_str(mime)
            .map_err(|_| ApiError::Rejected {
                status: 415,
                body: format!("invalid mime type '{}'", mime),
            })?;

        let metadata_json =
            serde_json::to_string(&self.mount_metadata()).unwrap_or_else(|_| "{}".to_string());

        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("containerTag", self.container_tag.clone())
            .text("filepath", filepath.to_string())
            .text("metadata", metadata_json);

        let resp = self
            .authed(self.http.post(self.url("/v3/documents/file")))
            .multipart(form)
            .send()
            .await
            .map_err(ApiError::Network)?;

        let status = resp.status();

        if status.is_success() {
            return resp
                .json::<CreateDocumentResp>()
                .await
                .map_err(ApiError::Network);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ApiError::Auth);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ApiError::RateLimited);
        }

        let body = resp.text().await.unwrap_or_default();
        if status.is_server_error() {
            return Err(ApiError::Server {
                status: status.as_u16(),
                body,
            });
        }
        Err(ApiError::Rejected {
            status: status.as_u16(),
            body,
        })
    }

    /// Update a document (filepath, content, or both). Metadata stamping
    /// (`source`, `lastEditedBy`) is merged into whatever the caller passed
    /// so we never stomp caller-provided keys.
    pub async fn update_document(&self, id: &str, req: &UpdateDocumentReq) -> Result<(), ApiError> {
        let mut stamped = req.clone();
        let stamp = self.mount_metadata();
        let merged = match stamped.metadata.take() {
            Some(mut existing) => {
                for (k, v) in stamp {
                    existing.entry(k).or_insert(v);
                }
                existing
            }
            None => stamp,
        };
        stamped.metadata = Some(merged);
        self.patch(&format!("/v3/documents/{id}"))
            .json(&stamped)
            .send_with_retry()
            .await?;
        Ok(())
    }

    /// Build the metadata object this client stamps onto every outgoing
    /// write. `source` is always present; `lastEditedBy` is included only
    /// when the client knows its user id (`with_user_id`).
    fn mount_metadata(&self) -> crate::api::dto::MetadataMap {
        let mut m = crate::api::dto::MetadataMap::new();
        m.insert(
            "source".into(),
            serde_json::Value::String("supermemoryfs".into()),
        );
        if let Some(uid) = &self.user_id {
            m.insert(
                "lastEditedBy".into(),
                serde_json::Value::String(uid.clone()),
            );
        }
        m
    }

    pub async fn update_memory_paths(&self, paths: Vec<String>) -> Result<(), ApiError> {
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body {
            memory_filesystem_paths: Vec<String>,
        }

        self.patch(&format!("/v3/container-tags/{}", self.container_tag))
            .json(&Body {
                memory_filesystem_paths: paths,
            })
            .send_with_retry()
            .await?;
        Ok(())
    }

    /// Bulk delete documents by filepath prefix or exact match.
    pub async fn delete_documents(&self, filepath: &str) -> Result<BulkDeleteResp, ApiError> {
        let body = BulkDeleteReq {
            ids: None,
            container_tags: Some(vec![self.container_tag.clone()]),
            filepath: Some(filepath.to_string()),
        };

        self.delete("/v3/documents/bulk")
            .json(&body)
            .send_with_retry()
            .await?
            .parse_json()
            .await
    }

    pub async fn delete_documents_by_ids(&self, ids: &[&str]) -> Result<BulkDeleteResp, ApiError> {
        let body = BulkDeleteReq {
            ids: Some(ids.iter().map(|s| s.to_string()).collect()),
            container_tags: Some(vec![self.container_tag.clone()]),
            filepath: None,
        };

        self.delete("/v3/documents/bulk")
            .json(&body)
            .send_with_retry()
            .await?
            .parse_json()
            .await
    }

    /// Semantic search across memories and chunks.
    pub async fn search(
        &self,
        query: &str,
        filepath: Option<&str>,
    ) -> Result<SearchResp, ApiError> {
        let body = SearchReq {
            q: query.to_string(),
            container_tag: self.container_tag.clone(),
            search_mode: "hybrid".to_string(),
            filepath: filepath.map(String::from),
            include: SearchInclude { documents: true },
        };

        self.post_read("/v4/search")
            .json(&body)
            .send_with_retry()
            .await?
            .parse_json()
            .await
    }

    /// Get the memory profile for the container tag.
    pub async fn get_profile(&self) -> Result<ProfileResp, ApiError> {
        let body = ProfileReq {
            container_tag: self.container_tag.clone(),
        };

        self.post_read("/v4/profile")
            .json(&body)
            .send_with_retry()
            .await?
            .parse_json()
            .await
    }

    // -- private helpers --

    fn get(&self, path: &str) -> RetryableRequest {
        RetryableRequest::new(self.authed(self.http.get(self.url(path))))
    }

    /// POST endpoint that counts as a write-side call (for test visibility
    /// into coalescing). Use for actual mutations (document create).
    fn post(&self, path: &str) -> RetryableRequest {
        self.write_calls.fetch_add(1, Ordering::Relaxed);
        RetryableRequest::new(self.authed(self.http.post(self.url(path))))
    }

    /// POST endpoint that is semantically a read (list/search/profile).
    /// Doesn't affect `write_calls`.
    fn post_read(&self, path: &str) -> RetryableRequest {
        RetryableRequest::new(self.authed(self.http.post(self.url(path))))
    }

    fn patch(&self, path: &str) -> RetryableRequest {
        self.write_calls.fetch_add(1, Ordering::Relaxed);
        RetryableRequest::new(self.authed(self.http.patch(self.url(path))))
    }

    fn delete(&self, path: &str) -> RetryableRequest {
        self.write_calls.fetch_add(1, Ordering::Relaxed);
        RetryableRequest::new(self.authed(self.http.delete(self.url(path))))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn authed(&self, req: RequestBuilder) -> RequestBuilder {
        req.header("Authorization", format!("Bearer {}", self.api_key))
    }
}

/// Wraps a `RequestBuilder` with retry + JSON body support.
struct RetryableRequest {
    builder: Option<RequestBuilder>,
    json_body: Option<serde_json::Value>,
}

impl RetryableRequest {
    fn new(builder: RequestBuilder) -> Self {
        Self {
            builder: Some(builder),
            json_body: None,
        }
    }

    fn json<T: serde::Serialize>(mut self, body: &T) -> Self {
        self.json_body = Some(serde_json::to_value(body).expect("serialize body"));
        self
    }

    async fn send_with_retry(self) -> Result<ApiResponse, ApiError> {
        let builder = self.builder.expect("builder consumed");
        let json_body = self.json_body;

        let mut backoff = INITIAL_BACKOFF_MS;

        for attempt in 0..MAX_RETRIES {
            let mut req = builder
                .try_clone()
                .expect("request must be cloneable for retry");

            if let Some(ref body) = json_body {
                req = req.json(body);
            }

            let result = req.send().await;

            match result {
                Ok(resp) => {
                    let status = resp.status();

                    if status.is_success() {
                        return Ok(ApiResponse(resp));
                    }

                    // Don't retry client errors (except 429)
                    if status == StatusCode::UNAUTHORIZED {
                        return Err(ApiError::Auth);
                    }
                    if status == StatusCode::NOT_FOUND {
                        return Err(ApiError::NotFound);
                    }
                    if status == StatusCode::CONFLICT {
                        let body = resp.text().await.unwrap_or_default();
                        return Err(ApiError::Conflict(body));
                    }

                    // Retry 429 and 5xx
                    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                        if attempt < MAX_RETRIES - 1 {
                            tracing::warn!(
                                status = status.as_u16(),
                                attempt = attempt + 1,
                                "retrying after {}ms",
                                backoff,
                            );
                            tokio::time::sleep(Duration::from_millis(backoff)).await;
                            backoff *= 2;
                            continue;
                        }

                        if status == StatusCode::TOO_MANY_REQUESTS {
                            return Err(ApiError::RateLimited);
                        }

                        let body = resp.text().await.unwrap_or_default();
                        return Err(ApiError::Server {
                            status: status.as_u16(),
                            body,
                        });
                    }

                    let body = resp.text().await.unwrap_or_default();
                    return Err(ApiError::Rejected {
                        status: status.as_u16(),
                        body,
                    });
                }
                Err(e) => {
                    if attempt < MAX_RETRIES - 1 {
                        tracing::warn!(
                            error = %e,
                            attempt = attempt + 1,
                            "network error, retrying after {}ms",
                            backoff,
                        );
                        tokio::time::sleep(Duration::from_millis(backoff)).await;
                        backoff *= 2;
                        continue;
                    }
                    return Err(ApiError::Network(e));
                }
            }
        }

        unreachable!("loop should return before exhausting retries")
    }
}

/// Thin wrapper to parse JSON from a successful response.
struct ApiResponse(Response);

impl ApiResponse {
    async fn parse_json<T: serde::de::DeserializeOwned>(self) -> Result<T, ApiError> {
        Ok(self.0.json().await?)
    }
}
