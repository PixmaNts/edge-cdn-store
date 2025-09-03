//! Minimal Pingora proxy example wired to `edge-cdn-store`.
//!
//! What it demonstrates:
//! - Enabling cache for GET/HEAD and using a static storage instance.
//! - Adding cache visibility headers: `x-cache-status` and `x-total-time-ms`.
//! - Basic upstream timing log when a connection is established.
//!
//! This is a development aid to iterate on storage behavior end-to-end.
use async_trait::async_trait;
use edge_cdn_store::EdgeMemoryStorage;
use once_cell::sync::Lazy;
use pingora::cache::filters::{request_cacheable, resp_cacheable};
use pingora::cache::{CacheMetaDefaults, RespCacheable};
use pingora::cache::cache_control::CacheControl;
use log::info;
use pingora::http::ResponseHeader;
use pingora::prelude::*;
use pingora::protocols::Digest;
use std::time::Instant;
use prometheus::{register_int_counter, IntCounter};

static STORAGE: Lazy<&'static EdgeMemoryStorage> =
    Lazy::new(|| Box::leak(Box::new(EdgeMemoryStorage::new())));

const CACHE_DEFAULTS: CacheMetaDefaults =
    CacheMetaDefaults::new(|status| match status.as_u16() {
        200 | 203 | 204 | 206 => Some(std::time::Duration::from_secs(60)),
        301 | 308 => Some(std::time::Duration::from_secs(300)),
        _ => None,
    }, 5, 5);

pub struct CachingProxy;

#[derive(Default, Clone, Copy)]
pub struct Ctx {
    start: Option<Instant>,
    connected_at: Option<Instant>,
}

impl CachingProxy {
    pub fn new() -> Self { Self }
}

impl Default for CachingProxy {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProxyHttp for CachingProxy {
    type CTX = Ctx;

    fn new_ctx(&self) -> Self::CTX { Ctx::default() }

    async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut Ctx) -> Result<Box<HttpPeer>> {
        // For demo: always proxy to httpbin.org
        let peer = Box::new(HttpPeer::new(
            "httpbin.org:80",
            false,
            "httpbin.org".to_string(),
        ));
        info!("🔗 Proxying to httpbin.org");
        Ok(peer)
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut pingora::http::RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Ensure proper Host header
        upstream_request
            .insert_header("Host", "httpbin.org")
            .unwrap();
        Ok(())
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        ctx.start = Some(Instant::now());
        let uri = session.req_header().uri.path();
        info!("📥 Incoming request: {uri}");
        // Increase request metric
        static REQ_METRIC: once_cell::sync::Lazy<IntCounter> = once_cell::sync::Lazy::new(|| {
            register_int_counter!("edge_requests_total", "Total requests seen by example").unwrap()
        });
        REQ_METRIC.inc();

        // For demo: let's cache everything
        Ok(false) // false = continue processing (don't early return)
    }

    fn request_cache_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<()> {
        // Enable cache only for GET/HEAD
        if request_cacheable(session.req_header()) {
            session
                .cache
                .enable(*STORAGE as &'static (dyn pingora::cache::Storage + Sync), None, None, None, None);
            // basic tracing hookup (inactive span placeholder)
            session
                .cache
                .enable_tracing(pingora::cache::trace::Span::inactive());
        }
        Ok(())
    }

    fn response_cache_filter(
        &self,
        _session: &Session,
        resp: &ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<RespCacheable> {
        let cc = CacheControl::from_resp_headers(resp);
        Ok(resp_cacheable(cc.as_ref(), resp.clone(), false, &CACHE_DEFAULTS))
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Inject cache status header similar to Pingora examples
        if session.cache.enabled() {
            match session.cache.phase() {
                pingora::cache::CachePhase::Hit => upstream_response.insert_header("x-cache-status", "hit")?,
                pingora::cache::CachePhase::Miss => upstream_response.insert_header("x-cache-status", "miss")?,
                pingora::cache::CachePhase::Stale => upstream_response.insert_header("x-cache-status", "stale")?,
                pingora::cache::CachePhase::StaleUpdating => upstream_response.insert_header("x-cache-status", "stale-updating")?,
                pingora::cache::CachePhase::Expired => upstream_response.insert_header("x-cache-status", "expired")?,
                pingora::cache::CachePhase::Revalidated | pingora::cache::CachePhase::RevalidatedNoCache(_) => upstream_response.insert_header("x-cache-status", "revalidated")?,
                _ => upstream_response.insert_header("x-cache-status", "invalid")?,
            }
        } else {
            upstream_response.insert_header("x-cache-status", "no-cache")?;
        }

        // Demo headers and simple timing
        upstream_response.insert_header("Cache-Control", "max-age=300")?;
        upstream_response.insert_header("X-Served-By", "EdgeCDN-Store")?;
        if let Some(start) = ctx.start.take() {
            let dur_ms = start.elapsed().as_millis();
            upstream_response.insert_header("x-total-time-ms", dur_ms.to_string())?;
        }
        info!("📤 Response from upstream: {}", upstream_response.status);
        Ok(())
    }

    async fn connected_to_upstream(
        &self,
        _session: &mut Session,
        reused: bool,
        _peer: &HttpPeer,
        #[cfg(unix)] _fd: std::os::unix::io::RawFd,
        #[cfg(windows)] _sock: std::os::windows::io::RawSocket,
        _digest: Option<&Digest>,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(start) = ctx.start {
            let ms = start.elapsed().as_millis();
            info!("⏱️ Connected to upstream (reused={reused}) after {ms} ms");
        } else {
            info!("⏱️ Connected to upstream (reused={reused})");
        }
        ctx.connected_at = Some(Instant::now());
        Ok(())
    }

    // (removed duplicate response_filter)

    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora::Error>,
        _ctx: &mut Self::CTX,
    ) {
        let response_code = session
            .response_written()
            .map_or(0, |resp| resp.status.as_u16());
        let status_emoji = if (200..300).contains(&response_code) {
            "✅"
        } else {
            "❌"
        };
        info!(
            "🚀 {} -> {} {}",
            session.req_header().uri.path(),
            response_code,
            status_emoji
        );
    }
}

fn main() {
    env_logger::init();

    let opt = Opt::parse_args();
    let mut server = Server::new(Some(opt)).unwrap();
    server.bootstrap();

    let proxy = CachingProxy::new();

    let mut proxy = http_proxy_service(&server.configuration, proxy);
    proxy.add_tcp("0.0.0.0:8080");

    // Prometheus metrics endpoint
    let mut prometheus_service_http = pingora::services::listening::Service::prometheus_http_service();
    prometheus_service_http.add_tcp("127.0.0.1:6192");

    info!("🎉 Starting EdgeCDN Server on http://localhost:8080");
    info!("📖 Try: curl http://localhost:8080/json");
    info!("📖 Try: curl http://localhost:8080/headers");

    server.add_service(prometheus_service_http);
    server.add_service(proxy);
    server.run_forever();
}
