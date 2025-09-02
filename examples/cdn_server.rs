use async_trait::async_trait;
use edge_cdn_store::EdgeMemoryStorage;
use log::info;
use pingora::prelude::*;
use pingora::http::ResponseHeader;
use pingora::proxy::*;
use std::sync::Arc;

pub struct CachingProxy {
    // In real code, you'd have your storage here
    _storage: Arc<EdgeMemoryStorage>,
}

impl CachingProxy {
    pub fn new() -> Self {
        Self {
            _storage: Arc::new(EdgeMemoryStorage::new()),
        }
    }
}

#[async_trait]
impl ProxyHttp for CachingProxy {
    type CTX = ();
    
    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut ()) -> Result<Box<HttpPeer>> {
        // For demo: always proxy to httpbin.org
        let peer = Box::new(HttpPeer::new("httpbin.org:80", false, "httpbin.org".to_string()));
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

    async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
        let uri = session.req_header().uri.path();
        info!("📥 Incoming request: {}", uri);
        
        // For demo: let's cache everything
        Ok(false) // false = continue processing (don't early return)
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        info!("📤 Response from upstream: {}", upstream_response.status);
        
        // Add cache headers for demo
        upstream_response
            .insert_header("Cache-Control", "max-age=300")
            .unwrap();
        upstream_response
            .insert_header("X-Served-By", "EdgeCDN-Store")
            .unwrap();
            
        Ok(())
    }

    async fn logging(
        &self,
        session: &mut Session,
        _e: Option<&pingora::Error>,
        _ctx: &mut Self::CTX,
    ) {
        let response_code = session
            .response_written()
            .map_or(0, |resp| resp.status.as_u16());
        info!(
            "🚀 {} -> {} {}",
            session.req_header().uri.path(),
            response_code,
            if response_code >= 200 && response_code < 300 { "✅" } else { "❌" }
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

    info!("🎉 Starting EdgeCDN Server on http://localhost:8080");
    info!("📖 Try: curl http://localhost:8080/json");
    info!("📖 Try: curl http://localhost:8080/headers");
    
    server.add_service(proxy);
    server.run_forever();
}