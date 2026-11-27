//! The static PWA (docs/design/w1-pwa.md §2.1): `GET /` is `index.html`, `GET /assets/…` an
//! exact embedded file, from a table the build embeds ([`EMBEDDED`], from `CHALDEA_WEB_DIST`
//! through `build.rs`). Production mounts it over [`Mounts::mvp_with`]'s 501 routes only when
//! the table is not empty ([`mount_pwa`]); [`Mounts::mvp`] keeps both routes 501.

#[cfg(test)]
mod dist;
#[cfg(test)]
mod tests;
// `dist`'s `super::trim`.
#[cfg(test)]
use crate::lifecycle::trim;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use axum::response::IntoResponse;
use futures_util::future::BoxFuture;

use super::ctx::ReadCtx;
use super::error::ApiError;
use super::routes::{Mount, Mounts, ReadHandler, spec};

/// One embedded file.
#[derive(Debug)]
pub struct Asset {
    /// The request path: `/` for `index.html`, else `/assets/…`.
    pub path: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/pwa_assets.rs"));

/// The page's policy; `connect-src` names the socket URL of the request's own `Host`.
const CSP_HEAD: &str = "default-src 'self'; connect-src 'self' ws://";
const CSP_TAIL: &str = "; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; \
     form-action 'none'; require-trusted-types-for 'script'; trusted-types 'none'";
const IMMUTABLE: &str = "max-age=31536000, immutable";

/// `mounts` with both PWA routes served from `assets`, or unchanged (both 501) when `assets`
/// is empty.
///
/// # Panics
///
/// Never: both routes are in `ROUTES`.
#[must_use]
pub fn mount_pwa(mounts: Mounts, assets: &'static [Asset]) -> Mounts {
    if assets.is_empty() {
        tracing::info!("built without the PWA: / and /assets/ answer 501");
        return mounts;
    }
    let route = |path| spec(&Method::GET, path).unwrap_or_else(|| panic!("GET {path} is a route"));
    mounts
        .with(Mount::read(route("/"), PwaHandler(assets)))
        .with(Mount::read(route("/assets/{*path}"), PwaHandler(assets)))
}

/// The handler of both PWA routes over one table: an exact lookup of the request path.
pub struct PwaHandler(pub &'static [Asset]);

impl ReadHandler for PwaHandler {
    fn call(
        &self,
        _ctx: ReadCtx,
        req: Request<Body>,
    ) -> BoxFuture<'static, Result<Response<Body>, ApiError>> {
        let resp = respond(self.0, &req);
        Box::pin(async move { resp })
    }
}

fn respond(assets: &[Asset], req: &Request<Body>) -> Result<Response<Body>, ApiError> {
    let path = req.uri().path();
    let Some(asset) = assets.iter().find(|a| a.path == path) else {
        let mut resp = ApiError::not_found().into_response();
        let h = resp.headers_mut();
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        resource_headers(h);
        return Ok(resp);
    };
    let mut resp = Response::new(Body::from(Bytes::from_static(asset.bytes)));
    *resp.status_mut() = StatusCode::OK;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    resource_headers(h);
    if asset.path == "/" {
        h.insert(header::CONTENT_SECURITY_POLICY, csp(req.headers())?);
        h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        h.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        h.insert(
            "cross-origin-opener-policy",
            HeaderValue::from_static("same-origin"),
        );
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    } else {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
    }
    Ok(resp)
}

/// Every response of the PWA routes, the 404 included.
fn resource_headers(h: &mut HeaderMap) {
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
}

/// The policy for the request's `Host`, lowercased (the outer layer has admitted exactly one,
/// naming the daemon).
fn csp(headers: &HeaderMap) -> Result<HeaderValue, ApiError> {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .ok_or_else(ApiError::internal)?
        .to_ascii_lowercase();
    HeaderValue::from_str(&format!("{CSP_HEAD}{host}{CSP_TAIL}")).map_err(|_| ApiError::internal())
}
