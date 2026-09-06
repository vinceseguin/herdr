//! The web app, embedded in the binary at build time.
//!
//! A gateway is one file an operator drops on a LAN machine; asking them to
//! also copy a `web/dist` tree next to it would be a second way to get the
//! deployment wrong. So the assets are `include_bytes!`d from `web/dist` into
//! [`ASSETS`], and `build.rs` re-runs when that directory changes.
//!
//! **There is no filesystem lookup here, by construction.** A request path is
//! matched against a fixed table of names, so `..`, an absolute path, a
//! percent-encoded separator or a symlink cannot reach anything: the worst a
//! crafted path can do is miss the table. That is the whole path-traversal
//! story for the static surface.
//!
//! E3 ships one placeholder `index.html` (E4 replaces the tree and may
//! generate the table from its build). The one routing rule the placeholder
//! already needs is the SPA fallback: a `GET` for a path with no file
//! extension is the app's own client-side route, so it is answered with
//! `index.html`; anything that looks like a file and is not in the table is a
//! 404.

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::gateway::http::ApiError;

/// One file compiled into the binary.
pub(crate) struct EmbeddedAsset {
    /// Path relative to `web/dist`, without a leading slash.
    pub(crate) path: &'static str,
    pub(crate) bytes: &'static [u8],
}

/// The document served for the app's own routes.
const INDEX_PATH: &str = "index.html";

/// Everything under `web/dist`, in one table.
pub(crate) static ASSETS: &[EmbeddedAsset] = &[EmbeddedAsset {
    path: INDEX_PATH,
    bytes: include_bytes!("../../web/dist/index.html"),
}];

/// `Content-Type` per extension.
///
/// A fixed table rather than a mime-guessing dependency: the gateway serves
/// its own build output, so the set of extensions is known, and an unknown one
/// must be a 404 rather than a guessed type a browser might sniff.
const CONTENT_TYPES: &[(&str, &str)] = &[
    ("html", "text/html; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("json", "application/json"),
    ("webmanifest", "application/manifest+json"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("ico", "image/x-icon"),
    ("woff2", "font/woff2"),
];

/// What an asset with an unknown extension is served as.
///
/// Reachable only if E4 adds a file whose extension is not in the table; an
/// opaque type plus `nosniff` is the safe reading of "we do not know".
const FALLBACK_CONTENT_TYPE: &str = "application/octet-stream";

/// The `Content-Type` for a file name, or `None` when the extension is not one
/// the gateway serves.
fn content_type_for(path: &str) -> Option<&'static str> {
    let extension = extension_of(path)?;
    CONTENT_TYPES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(extension))
        .map(|(_, value)| *value)
}

/// The extension of the last path segment, or `None` when it has none.
fn extension_of(path: &str) -> Option<&str> {
    let last = path.rsplit('/').next().unwrap_or(path);
    let (stem, extension) = last.rsplit_once('.')?;
    // A dotfile (`.env`) is not an extension, and neither is a trailing dot.
    if stem.is_empty() || extension.is_empty() {
        return None;
    }
    Some(extension)
}

/// The asset for a request path (with or without its leading slash), or `None`.
///
/// `/` and any extension-less path resolve to `index.html`: the app owns its
/// own routes and the server does not know them.
pub(crate) fn lookup(path: &str) -> Option<&'static EmbeddedAsset> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() || extension_of(trimmed).is_none() {
        return find(INDEX_PATH);
    }
    find(trimmed)
}

fn find(path: &str) -> Option<&'static EmbeddedAsset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

/// The router's fallback: the embedded app, `GET`/`HEAD` only.
///
/// Reached only for paths no route claimed, so an unknown `/api/*` path lands
/// here too. Those are answered `404 {"error":"not_found"}` rather than with
/// the HTML shell: an API client that gets a login page instead of JSON has no
/// way to tell a routing mistake from a real answer.
pub(crate) async fn serve(method: Method, uri: Uri) -> Response {
    let path = uri.path();
    if path.starts_with("/api/") || path == "/api" {
        return ApiError::not_found().into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed").into_response();
    }
    let Some(asset) = lookup(path) else {
        return ApiError::not_found().into_response();
    };
    asset.response(method == Method::HEAD)
}

impl EmbeddedAsset {
    /// The type this asset is served as, from its extension.
    fn content_type(&self) -> &'static str {
        content_type_for(self.path).unwrap_or(FALLBACK_CONTENT_TYPE)
    }

    /// This asset as a response.
    ///
    /// `index.html` is `no-cache` (it is revalidated on every load so a
    /// redeploy is picked up); E4's hashed file names get `immutable` when
    /// they exist. Nothing here is cached by a shared proxy: the app is behind
    /// a token.
    fn response(&self, head_only: bool) -> Response {
        let cache_control = if self.path == INDEX_PATH {
            "no-cache, private"
        } else {
            "private, max-age=3600"
        };
        let body = if head_only {
            Body::empty()
        } else {
            Body::from(self.bytes)
        };
        let mut response = Response::new(body);
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(self.content_type()),
        );
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        );
        // The shell is same-origin only: nothing else may frame it, and a
        // browser must not sniff a type we did not send.
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from(self.bytes.len() as u64),
        );
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_is_embedded_and_looks_like_the_fleet_shell() {
        let index = find(INDEX_PATH).expect("index.html is embedded");
        let text = std::str::from_utf8(index.bytes).expect("index.html is utf-8");
        assert!(text.contains("Herdr Fleet"), "{text}");
        assert!(text.contains("/api/gateway"), "{text}");
        assert!(text.contains("herdr gateway pair"), "{text}");
    }

    #[test]
    fn every_asset_has_an_extension_the_table_knows() {
        for asset in ASSETS {
            assert_eq!(
                content_type_for(asset.path),
                Some(asset.content_type()),
                "{} has an extension the content-type table does not map",
                asset.path
            );
        }
    }

    #[test]
    fn extension_less_paths_are_app_routes() {
        for path in ["/", "", "/settings", "/hosts/lab-1", "/a.b/plain"] {
            let asset = lookup(path).expect("app routes fall back to the shell");
            assert_eq!(asset.path, INDEX_PATH, "path: {path}");
        }
    }

    #[test]
    fn a_missing_file_is_not_the_shell() {
        for path in ["/nope.png", "/app.js", "/index.htm", "/deep/thing.css"] {
            assert!(lookup(path).is_none(), "path: {path}");
        }
    }

    /// The table is matched exactly, so no traversal spelling reaches anything
    /// — including the one file that *is* embedded.
    #[test]
    fn traversal_spellings_never_reach_an_asset_by_a_file_path() {
        for path in [
            "/../Cargo.toml",
            "/../../etc/passwd",
            "/web/dist/index.html",
            "/./index.html",
            "//index.html",
            "/%2e%2e/Cargo.toml",
        ] {
            // Extension-less spellings still land on the shell, which is the
            // SPA rule and reveals nothing.
            if let Some(asset) = lookup(path) {
                assert_eq!(asset.path, INDEX_PATH, "path: {path}");
            }
        }
        // `index.html` is only reachable at the root of the table.
        assert!(lookup("/web/dist/index.html").is_none());
    }

    #[test]
    fn dotfiles_and_trailing_dots_have_no_extension() {
        assert_eq!(extension_of(".env"), None);
        assert_eq!(extension_of("thing."), None);
        assert_eq!(extension_of("dir.d/file"), None);
        assert_eq!(extension_of("app.js"), Some("js"));
    }

    #[test]
    fn unknown_extensions_have_no_content_type() {
        assert_eq!(content_type_for("thing.exe"), None);
        assert_eq!(content_type_for("thing.wasm"), None);
        assert_eq!(content_type_for("INDEX.HTML"), Some(CONTENT_TYPES[0].1));
    }
}
