//! Static asset serving for the built React SPA.
//!
//! Serves files from a directory (the Vite `dist/` output) and falls back to
//! `index.html` for any unmatched path so client-side routes like
//! `/sessions/:id` resolve to the SPA shell. API routes are registered on the
//! router and therefore take precedence over this fallback service.

use std::path::Path;

use tower_http::services::{ServeDir, ServeFile};

/// A `ServeDir` that serves `dir` and falls back to `dir/index.html` (SPA).
pub fn spa_service(dir: &str) -> ServeDir<ServeFile> {
    let index = Path::new(dir).join("index.html");
    ServeDir::new(dir).fallback(ServeFile::new(index))
}
