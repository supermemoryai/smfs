#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("auth failed (401)")]
    Auth,

    #[error("not found (404)")]
    NotFound,

    #[error("conflict (409): {0}")]
    Conflict(String),

    #[error("rate limited (429)")]
    RateLimited,

    /// Permanent 4xx rejection (not retryable).
    #[error("rejected ({status}): {body}")]
    Rejected { status: u16, body: String },

    #[error("server error ({status}): {body}")]
    Server { status: u16, body: String },
}
