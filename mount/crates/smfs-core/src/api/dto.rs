use serde::{Deserialize, Serialize};

/// A document returned by the Supermemory API.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    pub id: String,
    pub filepath: Option<String>,
    pub custom_id: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub content: Option<String>,
    pub status: String,
    pub container_tags: Option<Vec<String>>,
    pub created_at: String,
    pub updated_at: String,
}

/// POST /v3/documents
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateDocumentReq {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filepath: Option<String>,
    pub container_tag: String,
}

/// Response from POST /v3/documents
#[derive(Debug, Deserialize)]
pub struct CreateDocumentResp {
    pub id: String,
    pub status: String,
}

/// PATCH /v3/documents/:id
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateDocumentReq {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filepath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// POST /v3/documents/list
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListDocumentsReq {
    pub container_tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filepath: Option<String>,
    pub limit: u32,
    pub page: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_content: Option<bool>,
}

/// Response from POST /v3/documents/list
#[derive(Debug, Deserialize)]
pub struct ListDocumentsResp {
    pub memories: Vec<Document>,
    pub pagination: Pagination,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pagination {
    pub current_page: u32,
    pub limit: u32,
    pub total_items: u32,
    pub total_pages: u32,
}

/// DELETE /v3/documents/bulk
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkDeleteReq {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filepath: Option<String>,
}

/// Response from DELETE /v3/documents/bulk
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkDeleteResp {
    pub success: bool,
    pub deleted_count: u32,
}

/// POST /v4/profile
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileReq {
    pub container_tag: String,
}

/// Response from POST /v4/profile
#[derive(Debug, Deserialize)]
pub struct ProfileResp {
    pub profile: Profile,
}

#[derive(Debug, Deserialize)]
pub struct Profile {
    #[serde(rename = "static")]
    pub static_memories: Option<Vec<String>>,
    pub dynamic: Option<Vec<String>>,
}

/// POST /v4/search
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchReq {
    pub q: String,
    pub container_tag: String,
    pub search_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filepath: Option<String>,
    pub include: SearchInclude,
}

#[derive(Debug, Serialize)]
pub struct SearchInclude {
    pub documents: bool,
}

/// Response from POST /v4/search
#[derive(Debug, Deserialize)]
pub struct SearchResp {
    pub results: Vec<SearchResult>,
    pub timing: Option<f64>,
    pub total: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub memory: Option<String>,
    pub chunk: Option<String>,
    pub similarity: f64,
    pub filepath: Option<String>,
}
