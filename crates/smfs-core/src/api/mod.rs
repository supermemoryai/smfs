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
    document_ids: parking_lot::RwLock<std::collections::HashMap<String, String>>,
    cached_ids: parking_lot::RwLock<Result<Vec<String>, String>>,
    document_ids_ready: tokio::sync::OnceCell<()>,
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
    pub org_id: Option<String>,
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
            document_ids: parking_lot::RwLock::new(std::collections::HashMap::new()),
            cached_ids: parking_lot::RwLock::new(Ok(Vec::new())),
            document_ids_ready: tokio::sync::OnceCell::new(),
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
        let org_id = body
            .get("org")
            .and_then(|o| o.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);
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
            org_id,
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

    pub async fn list_documents_page(
        &self,
        body: &ListDocumentsReq,
    ) -> Result<ListDocumentsResp, ApiError> {
        self.prepare_document_ids().await?;
        if body
            .container_tags
            .iter()
            .any(|tag| tag != &self.container_tag)
        {
            return Err(ApiError::Rejected {
                status: 400,
                body: "A list request must use the mount namespace".to_string(),
            });
        }
        let mut response = self.read_document_page(body).await?;
        for doc in &mut response.memories {
            doc.id = self.cached_document_id(&doc.id);
        }
        Ok(response)
    }

    async fn read_document_page(
        &self,
        body: &ListDocumentsReq,
    ) -> Result<ListDocumentsResp, ApiError> {
        let query = serde_json::json!({
            "page": body.page, "limit": body.limit,
            "sort": body.sort.as_deref().unwrap_or("createdAt"),
            "order": body.order.as_deref().unwrap_or("desc"),
        });
        let request = self
            .authed(
                self.http
                    .post(self.url(&self.namespace_path("/list/documents"))),
            )
            .query(&query);
        let mut payload =
            serde_json::json!({ "include": { "content": body.include_content.unwrap_or(false) } });
        if let Some(filepath) = &body.filepath {
            payload["filepath"] = filepath.clone().into();
        }
        let response: ListDocumentsResp = RetryableRequest::new(request)
            .json(&payload)
            .send_with_retry()
            .await?
            .parse_json()
            .await?;
        Ok(response)
    }

    pub(crate) fn set_cached_document_ids(&self, ids: Result<Vec<String>, String>) {
        *self.cached_ids.write() = ids;
    }

    async fn prepare_document_ids(&self) -> Result<(), ApiError> {
        self.document_ids_ready
            .get_or_try_init(|| async {
                let ids = self
                    .cached_ids
                    .read()
                    .clone()
                    .map_err(|body| ApiError::Server { status: 500, body })?;
                if ids.is_empty() {
                    return Ok(());
                }
                let mut current_ids = std::collections::HashSet::new();
                let mut page = 1;
                loop {
                    let response = self
                        .read_document_page(&ListDocumentsReq {
                            container_tags: vec![self.container_tag.clone()],
                            filepath: None,
                            page,
                            limit: 100,
                            include_content: Some(false),
                            sort: None,
                            order: None,
                        })
                        .await?;
                    let empty = response.memories.is_empty();
                    current_ids.extend(response.memories.into_iter().map(|doc| doc.id));
                    if empty || page >= response.pagination.total_pages {
                        break;
                    }
                    page += 1;
                }
                let ids: Vec<_> = ids
                    .into_iter()
                    .filter(|id| !current_ids.contains(id))
                    .collect();
                for batch in ids.chunks(8) {
                    let results = futures::future::join_all(
                        batch.iter().map(|id| self.get_document_state(id)),
                    )
                    .await;
                    for (id, result) in batch.iter().zip(results) {
                        match result {
                            Ok(doc) => self.retain_document_id(&doc.id, id),
                            Err(ApiError::NotFound) => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub(crate) fn retain_document_id(&self, public_id: &str, cached_id: &str) {
        if public_id != cached_id {
            self.document_ids
                .write()
                .insert(public_id.to_string(), cached_id.to_string());
        }
    }

    fn cached_document_id(&self, id: &str) -> String {
        self.document_ids
            .read()
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    pub async fn get_document(&self, id: &str) -> Result<Document, ApiError> {
        let mut doc: Document = self.get_document_state(id).await?;
        if let Some(filepath) = doc.filepath.as_deref() {
            if doc
                .type_
                .as_deref()
                .is_some_and(|kind| kind != "text" && kind != "note")
            {
                doc.url = self
                    .list_documents(Some(filepath))
                    .await?
                    .into_iter()
                    .find(|entry| entry.id == doc.id)
                    .and_then(|entry| entry.url);
            }
        }
        Ok(doc)
    }

    async fn get_document_state(&self, id: &str) -> Result<Document, ApiError> {
        let mut doc: Document = self
            .get(&self.namespace_path(&format!("/document/{}", encode_segment(id))))
            .send_with_retry()
            .await?
            .parse_json()
            .await?;
        doc.id = self.cached_document_id(&doc.id);
        Ok(doc)
    }

    pub async fn get_document_states(&self, ids: &[&str]) -> Result<Vec<Document>, ApiError> {
        let mut docs = Vec::with_capacity(ids.len());
        for batch in ids.chunks(8) {
            for result in
                futures::future::join_all(batch.iter().map(|id| self.get_document_state(id))).await
            {
                match result {
                    Ok(doc) => docs.push(doc),
                    Err(ApiError::NotFound) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(docs)
    }

    pub async fn create_document(
        &self,
        content: &str,
        filepath: &str,
    ) -> Result<CreateDocumentResp, ApiError> {
        self.create_document_with_id(content, filepath, None).await
    }

    pub(crate) async fn create_document_with_id(
        &self,
        content: &str,
        filepath: &str,
        id: Option<&str>,
    ) -> Result<CreateDocumentResp, ApiError> {
        let mut body = serde_json::json!(CreateDocumentReq {
            content: content.to_string(),
            filepath: Some(filepath.to_string()),
            metadata: Some(self.mount_metadata()),
        });
        if let Some(id) = id {
            body["id"] = serde_json::Value::String(id.to_string());
        }

        let response = self
            .post(&self.namespace_path("/document"))
            .json(&body)
            .send_with_retry()
            .await;
        match response {
            Ok(response) => response.parse_json().await,
            Err(ApiError::Conflict(conflict)) if id.is_some() => {
                let id = id.expect("guarded above");
                let existing = self.get_document(id).await?;
                if existing.id != id || existing.filepath.as_deref() != Some(filepath) {
                    return Err(ApiError::Conflict(conflict));
                }
                if existing.content.as_deref() == Some(content) {
                    return Ok(CreateDocumentResp {
                        id: existing.id,
                        status: existing.status,
                    });
                }
                if existing.status != "done" && existing.status != "failed" {
                    return Err(ApiError::Conflict(conflict));
                }
                self.update_document(
                    id,
                    &UpdateDocumentReq {
                        content: Some(content.to_string()),
                        filepath: Some(filepath.to_string()),
                        metadata: None,
                    },
                )
                .await?;
                Ok(CreateDocumentResp {
                    id: id.to_string(),
                    status: "queued".to_string(),
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Binary uploads rely on persistent push-queue retries after transient failures.
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
            .text("filepath", filepath.to_string())
            .text("metadata", metadata_json);

        let resp = self
            .authed(
                self.http
                    .post(self.url(&self.namespace_path("/document/file"))),
            )
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
        self.patch(&self.namespace_path(&format!("/document/{}", encode_segment(id))))
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

        self.patch(&self.namespace_path(""))
            .json(&Body {
                memory_filesystem_paths: paths,
            })
            .send_with_retry()
            .await?;
        Ok(())
    }

    /// Bulk delete documents by filepath prefix or exact match.
    pub async fn delete_documents(&self, filepath: &str) -> Result<BulkDeleteResp, ApiError> {
        let docs: Vec<Document> = self
            .list_documents(Some(filepath))
            .await?
            .into_iter()
            .filter(|doc| {
                doc.filepath.as_deref().is_some_and(|path| {
                    if filepath.ends_with('/') {
                        path.starts_with(filepath)
                    } else {
                        path == filepath
                    }
                })
            })
            .collect();
        let mut deleted_count = 0;
        for batch in docs.chunks(100) {
            let ids: Vec<&str> = batch.iter().map(|doc| doc.id.as_str()).collect();
            deleted_count += self.delete_documents_by_ids(&ids).await?.deleted_count;
        }
        Ok(BulkDeleteResp {
            success: true,
            deleted_count,
        })
    }

    pub async fn delete_documents_by_ids(&self, ids: &[&str]) -> Result<BulkDeleteResp, ApiError> {
        if ids.is_empty() {
            return Ok(BulkDeleteResp {
                success: true,
                deleted_count: 0,
            });
        }
        let response: DeleteDocumentsResult = self
            .delete(&self.namespace_path("/document"))
            .json(&serde_json::json!({ "ids": ids }))
            .send_with_retry()
            .await?
            .parse_json()
            .await?;
        let errors: Vec<_> = response
            .errors
            .into_iter()
            .filter(|error| error.error != "Document not found")
            .collect();
        if !errors.is_empty() {
            return Err(ApiError::Conflict(
                errors
                    .iter()
                    .map(|error| format!("{}: {}", error.id, error.error))
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        Ok(BulkDeleteResp {
            success: true,
            deleted_count: response.count,
        })
    }

    /// Semantic search across memories and chunks.
    pub async fn search(
        &self,
        query: &str,
        filepath: Option<&str>,
    ) -> Result<SearchResp, ApiError> {
        let body = SearchReq {
            query: query.to_string(),
            search_mode: "hybrid".to_string(),
            filepath: filepath.map(String::from),
            include: SearchInclude { documents: true },
        };

        let mut response: SearchResp = self
            .post_read(&self.namespace_path("/search"))
            .json(&body)
            .send_with_retry()
            .await?
            .parse_json()
            .await?;
        for result in &mut response.results {
            result.id = self.cached_document_id(&result.id);
        }
        Ok(response)
    }

    /// Get the memory profile for the container tag.
    pub async fn get_profile(&self) -> Result<ProfileResp, ApiError> {
        self.post_read(&self.namespace_path("/profile"))
            .json(&serde_json::json!({}))
            .send_with_retry()
            .await?
            .parse_json()
            .await
    }

    fn namespace_path(&self, suffix: &str) -> String {
        format!("/ns/{}{}", encode_segment(&self.container_tag), suffix)
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

fn encode_segment(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}
