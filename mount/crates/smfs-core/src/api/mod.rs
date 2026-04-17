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
use std::time::Duration;

const MAX_RETRIES: u32 = 5;
const INITIAL_BACKOFF_MS: u64 = 100;

pub struct ApiClient {
    http: Client,
    base_url: String,
    api_key: String,
    container_tag: String,
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
        }
    }

    pub fn container_tag(&self) -> &str {
        &self.container_tag
    }

    /// Validate an API key by hitting GET /v3/session.
    /// This is a standalone function (no ApiClient instance needed).
    pub async fn validate_key(base_url: &str, api_key: &str) -> Result<String, ApiError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ApiError::Network(e))?;

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

        // Extract org name from response for display.
        let body: serde_json::Value = resp.json().await.map_err(ApiError::Network)?;
        let org_name = body
            .get("org")
            .and_then(|o| o.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(org_name)
    }

    /// List documents, optionally filtered by filepath prefix or exact match.
    pub async fn list_documents(
        &self,
        filepath: Option<&str>,
    ) -> Result<Vec<Document>, ApiError> {
        let mut page = 1u32;
        let mut all = Vec::new();

        loop {
            let body = ListDocumentsReq {
                container_tags: vec![self.container_tag.clone()],
                filepath: filepath.map(String::from),
                limit: 100,
                page,
                include_content: Some(true),
            };

            let resp: ListDocumentsResp = self
                .post("/v3/documents/list")
                .json(&body)
                .send_with_retry()
                .await?
                .parse_json()
                .await?;

            let count = resp.memories.len();
            all.extend(resp.memories);

            if page >= resp.pagination.total_pages || count == 0 {
                break;
            }
            page += 1;
        }

        Ok(all)
    }

    /// Get a single document by ID or customId.
    pub async fn get_document(&self, id: &str) -> Result<Document, ApiError> {
        self.get(&format!("/v3/documents/{id}"))
            .send_with_retry()
            .await?
            .parse_json()
            .await
    }

    /// Create a new document with content and filepath.
    pub async fn create_document(
        &self,
        content: &str,
        filepath: &str,
    ) -> Result<CreateDocumentResp, ApiError> {
        let body = CreateDocumentReq {
            content: content.to_string(),
            filepath: Some(filepath.to_string()),
            container_tag: self.container_tag.clone(),
        };

        self.post("/v3/documents")
            .json(&body)
            .send_with_retry()
            .await?
            .parse_json()
            .await
    }

    /// Update a document (filepath, content, or both).
    pub async fn update_document(
        &self,
        id: &str,
        req: &UpdateDocumentReq,
    ) -> Result<(), ApiError> {
        self.patch(&format!("/v3/documents/{id}"))
            .json(req)
            .send_with_retry()
            .await?;
        Ok(())
    }

    /// Bulk delete documents by filepath prefix or exact match.
    pub async fn delete_documents(
        &self,
        filepath: &str,
    ) -> Result<BulkDeleteResp, ApiError> {
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

        self.post("/v4/search")
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

        self.post("/v4/profile")
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

    fn post(&self, path: &str) -> RetryableRequest {
        RetryableRequest::new(self.authed(self.http.post(self.url(path))))
    }

    fn patch(&self, path: &str) -> RetryableRequest {
        RetryableRequest::new(self.authed(self.http.patch(self.url(path))))
    }

    fn delete(&self, path: &str) -> RetryableRequest {
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
                    if status == StatusCode::TOO_MANY_REQUESTS
                        || status.is_server_error()
                    {
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

                    // Other 4xx — don't retry
                    let body = resp.text().await.unwrap_or_default();
                    return Err(ApiError::Server {
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
