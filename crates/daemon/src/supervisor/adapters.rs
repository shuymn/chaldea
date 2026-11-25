//! `GET /api/adapters` (design §2.8): this host's rows of `CAPABILITIES`, with no store
//! read.

use axum::body::Body;
use axum::http::{Method, Request, Response};
use chaldea_core::Open;
use chaldea_core::adapter::{CAPABILITIES, HostOs};
use chaldea_core::sync::{AdapterInfo, AdaptersResponse};
use futures_util::future::BoxFuture;

use crate::http::{ApiError, Mount, ReadCtx, ReadHandler, json, no_query, spec};

/// The response for `host_os`: its rows, in `CAPABILITIES` order.
pub(crate) fn adapters(host_os: HostOs) -> AdaptersResponse {
    AdaptersResponse {
        host_os: Open::Known(host_os),
        adapters: CAPABILITIES
            .iter()
            .filter(|c| c.os() == host_os)
            .map(|c| AdapterInfo {
                agent: Open::Known(c.agent()),
                pty_encoding: Open::Known(c.pty_encoding()),
                first_prompt: Open::Known(c.first_prompt()),
            })
            .collect(),
    }
}

struct ReadAdapters(HostOs);

impl ReadHandler for ReadAdapters {
    fn call(
        &self,
        _ctx: ReadCtx,
        req: Request<Body>,
    ) -> BoxFuture<'static, Result<Response<Body>, ApiError>> {
        let host_os = self.0;
        Box::pin(async move {
            no_query(&req)?;
            json(&adapters(host_os))
        })
    }
}

/// The route's mount.
pub(crate) fn mount(host_os: HostOs) -> Mount {
    let route = spec(&Method::GET, "/api/adapters")
        .unwrap_or_else(|| panic!("GET /api/adapters is in ROUTES"));
    Mount::read(route, ReadAdapters(host_os))
}
