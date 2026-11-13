use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Flat metadata bag — matches the server's Drizzle schema type
/// `Record<string, string | number | boolean | string[]>`
/// (see mono/packages/db/schema/content.ts). We type the value as
/// `serde_json::Value` because the server accepts any of those primitives
/// at the JSON level; `Value` lets us emit the right shape without
/// inventing an enum.
pub type MetadataMap = HashMap<String, serde_json::Value>;

/// A document returned by the Supermemory API.
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "NamespaceDocument")]
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
    #[serde(default)]
    pub metadata: Option<MetadataMap>,
    pub type_: Option<String>,
    /// R2 URL for binary docs; null for plain text. Used to rehydrate raw
    /// files on a clean remount.
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateDocumentReq {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filepath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MetadataMap>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDocumentResp {
    pub id: String,
    pub status: String,
}

#[derive(Debug, Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateDocumentReq {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filepath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MetadataMap>,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListDocumentsResp {
    #[serde(rename = "documents")]
    pub memories: Vec<Document>,
    pub pagination: Pagination,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessingDocumentsResp {
    pub documents: Vec<Document>,
    pub total_count: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pagination {
    pub current_page: u32,
    #[serde(default)]
    pub limit: u32,
    pub total_items: u32,
    pub total_pages: u32,
}

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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkDeleteResp {
    pub success: bool,
    pub deleted_count: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileReq {}

#[derive(Debug, Deserialize)]
pub struct ProfileResp {
    pub profile: Profile,
}

#[derive(Debug, Deserialize)]
pub struct Profile {
    #[serde(rename = "static")]
    #[serde(deserialize_with = "profile_text")]
    pub static_memories: Option<Vec<String>>,
    #[serde(deserialize_with = "profile_text")]
    pub dynamic: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchReq {
    pub query: String,
    pub search_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filepath: Option<String>,
    pub include: SearchInclude,
}

#[derive(Debug, Serialize)]
pub struct SearchInclude {
    pub documents: bool,
}

#[derive(Debug, Deserialize)]
pub struct SearchResp {
    pub results: Vec<SearchResult>,
    #[serde(rename = "searchTime")]
    pub timing: Option<f64>,
    pub total: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(from = "NamespaceSearchResult")]
pub struct SearchResult {
    pub id: String,
    pub memory: Option<String>,
    pub chunk: Option<String>,
    pub similarity: f64,
    pub filepath: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NamespaceSystem {
    status: String,
    created_at: String,
    updated_at: String,
    filepath: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NamespaceDocument {
    id: String,
    title: Option<String>,
    summary: Option<String>,
    content: Option<String>,
    metadata: Option<MetadataMap>,
    #[serde(rename = "type")]
    type_: Option<String>,
    url: Option<String>,
    system: NamespaceSystem,
}

impl From<NamespaceDocument> for Document {
    fn from(doc: NamespaceDocument) -> Self {
        Self {
            id: doc.id,
            filepath: doc.system.filepath,
            custom_id: None,
            title: doc.title,
            summary: doc.summary,
            content: doc.content,
            status: doc.system.status,
            container_tags: None,
            created_at: doc.system.created_at,
            updated_at: doc.system.updated_at,
            metadata: doc.metadata,
            type_: doc.type_,
            url: doc.url,
        }
    }
}

#[derive(Debug, Deserialize)]
struct NamespaceSearchResult {
    id: String,
    memory: Option<String>,
    chunk: Option<String>,
    similarity: f64,
    system: SearchSystem,
    included: Option<SearchIncluded>,
}

#[derive(Debug, Deserialize)]
struct SearchSystem {
    filepath: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchIncluded {
    document: Option<SearchDocument>,
}

#[derive(Debug, Deserialize)]
struct SearchDocument {
    id: String,
}

impl From<NamespaceSearchResult> for SearchResult {
    fn from(result: NamespaceSearchResult) -> Self {
        Self {
            id: result
                .included
                .and_then(|i| i.document)
                .map(|d| d.id)
                .unwrap_or(result.id),
            memory: result.memory,
            chunk: result.chunk,
            similarity: result.similarity,
            filepath: result.system.filepath,
        }
    }
}

fn profile_text<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    struct Memory {
        memory: String,
    }
    Ok(Option::<Vec<Memory>>::deserialize(deserializer)?
        .map(|items| items.into_iter().map(|item| item.memory).collect()))
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeleteDocumentsResult {
    pub count: u32,
    pub errors: Vec<DeleteDocumentError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeleteDocumentError {
    pub id: String,
    pub error: String,
}
