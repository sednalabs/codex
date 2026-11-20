use crate::connect_policy::TargetCheckedTcpConnector;
use crate::connect_policy::is_non_public_target;
use crate::policy::is_private_network_ip;
use crate::state::NetworkProxyState;
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use rama_core::Layer;
use rama_core::Service;
use rama_core::error::ErrorExt as _;
use rama_core::error::extra::OpaqueError;
use rama_core::extensions::ExtensionsRef;
use rama_core::service::BoxService;
use rama_core::service::service_fn;
use rama_http::Body;
use rama_http::HeaderName;
use rama_http::HeaderValue;
use rama_http::Request;
use rama_http::Response;
use rama_http::Version;
use rama_http::layer::version_adapter::RequestVersionAdapter;
use rama_http_backend::client::HttpClientService;
use rama_http_backend::client::HttpConnectorLayer;
use rama_http_backend::client::proxy::layer::HttpProxyConnector;
use rama_http_backend::client::proxy::layer::HttpProxyConnectorLayer;
use rama_net::AuthorityInputExt;
use rama_net::ProtocolInputExt;
use rama_net::address::Host;
use rama_net::address::HostWithPort;
use rama_net::address::ProxyAddress;
use rama_net::client::EstablishedClientConnection;
use rama_net::uri::Uri;
use rama_tls_rustls::client::RustlsClientConfigExt;
use rama_tls_rustls::client::TlsConnectorLayer;
use rama_tls_rustls::client::client_root_certs;
use rama_tls_rustls::dep::rustls;
use rama_tls_rustls::types::client::TlsClientConfig;
use std::sync::Arc;
use std::time::Instant;
use tracing::info;
use tracing::warn;

#[cfg(target_os = "macos")]
use rama_unix::client::UnixConnector;

const TOOL_CALL_HEADER: HeaderName = HeaderName::from_static("x-openai-internal-tool-call-id");

#[derive(Clone, Copy, Debug)]
pub(crate) struct OptionsAsterisk;

impl rama_core::extensions::Extension for OptionsAsterisk {}

#[derive(Clone)]
struct ToolCallAttribution {
    endpoint: ProxyAddress,
    call_id: HeaderValue,
}

impl ToolCallAttribution {
    fn call_id_for(&self, proxy: &ProxyAddress) -> Option<HeaderValue> {
        let http = &rama_net::Protocol::HTTP;
        (self.endpoint.address == proxy.address
            && self.endpoint.protocol.as_ref().unwrap_or(http)
                == proxy.protocol.as_ref().unwrap_or(http))
        .then(|| self.call_id.clone())
    }
}

#[derive(Clone, Default)]
struct ProxyConfig {
    http: Option<ProxyAddress>,
    https: Option<ProxyAddress>,
    all: Option<ProxyAddress>,
    proxy_private_ips_via_upstream: bool,
    attribution: Option<ToolCallAttribution>,
}

impl ProxyConfig {
    fn from_env(state: &NetworkProxyState) -> Self {
        let http = read_proxy_env(&["HTTP_PROXY", "http_proxy"]);
        let https = read_proxy_env(&["HTTPS_PROXY", "https_proxy"]);
        let all = read_proxy_env(&["ALL_PROXY", "all_proxy"]);
        Self {
            http,
            https,
            all,
            proxy_private_ips_via_upstream: state.proxy_private_ips_via_upstream,
            attribution: state
                .process_log_metadata
                .tool_call_id
                .as_deref()
                .and_then(|id| {
                    let endpoint = read_proxy_env(&["CODEX_TOOL_CALL_ATTRIBUTION_PROXY"])?;
                    Some(ToolCallAttribution {
                        endpoint,
                        call_id: HeaderValue::from_str(id).ok()?,
                    })
                }),
        }
    }

    fn proxy_for_protocol(&self, is_secure: bool) -> Option<ProxyAddress> {
        if is_secure {
            self.https
                .clone()
                .or_else(|| self.http.clone())
                .or_else(|| self.all.clone())
        } else {
            self.http.clone().or_else(|| self.all.clone())
        }
    }

    fn proxy_for_target(&self, target: &HostWithPort, is_secure: bool) -> Option<ProxyAddress> {
        let proxy_private_ip = self.proxy_private_ips_via_upstream
            && matches!(&target.host, Host::Address(ip) if is_private_network_ip(*ip));
        if is_non_public_target(&target.host) && !proxy_private_ip {
            return None;
        }
        self.proxy_for_protocol(is_secure)
    }
}

fn read_proxy_env(keys: &[&str]) -> Option<ProxyAddress> {
    for key in keys {
        let Ok(value) = std::env::var(key) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match ProxyAddress::try_from(value) {
            Ok(proxy) => {
                if proxy
                    .protocol
                    .as_ref()
                    .map(rama_net::Protocol::is_http)
                    .unwrap_or(true)
                {
                    return Some(proxy);
                }
                warn!("ignoring {key}: non-http proxy protocol");
            }
            Err(err) => {
                warn!("ignoring {key}: invalid proxy address ({err})");
            }
        }
    }
    None
}

pub(crate) fn proxy_for_connect(
    target: &HostWithPort,
    state: &NetworkProxyState,
) -> (Option<ProxyAddress>, Option<HeaderValue>) {
    let config = ProxyConfig::from_env(state);
    let proxy = config.proxy_for_target(target, /*is_secure*/ true);
    let call_id = config
        .attribution
        .as_ref()
        .zip(proxy.as_ref())
        .and_then(|(attribution, proxy)| attribution.call_id_for(proxy));
    (proxy, call_id)
}

pub(crate) fn upstream_proxy_connector(
    transport: TargetCheckedTcpConnector,
    connect_call_id: Option<HeaderValue>,
) -> HttpProxyConnector<TargetCheckedTcpConnector> {
    let mut layer = HttpProxyConnectorLayer::optional();
    if let Some(id) = connect_call_id {
        layer.set_custom_header(TOOL_CALL_HEADER, id);
    }
    layer.into_layer(transport)
}

#[derive(Clone)]
pub(crate) struct UpstreamClient {
    connector: BoxService<
        Request<Body>,
        EstablishedClientConnection<HttpClientService<Body>, Request<Body>>,
        OpaqueError,
    >,
    proxy_config: ProxyConfig,
}

impl UpstreamClient {
    pub(crate) fn direct(state: Arc<NetworkProxyState>) -> Self {
        Self::new(
            ProxyConfig::default(),
            TargetCheckedTcpConnector::new(state),
            client_root_certs(),
        )
    }

    pub(crate) fn from_env_proxy(state: Arc<NetworkProxyState>) -> Self {
        Self::new(
            ProxyConfig::from_env(&state),
            TargetCheckedTcpConnector::new(state),
            client_root_certs(),
        )
    }

    pub(crate) fn direct_with_tls_root_store(
        state: Arc<NetworkProxyState>,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        Self::new(
            ProxyConfig::default(),
            TargetCheckedTcpConnector::new(state),
            tls_root_store,
        )
    }

    pub(crate) fn from_env_proxy_with_tls_root_store(
        state: Arc<NetworkProxyState>,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        Self::new(
            ProxyConfig::from_env(&state),
            TargetCheckedTcpConnector::new(state),
            tls_root_store,
        )
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn unix_socket(path: &str) -> Self {
        let connector = build_unix_connector(path);
        Self {
            connector,
            proxy_config: ProxyConfig::default(),
        }
    }

    fn new(
        proxy_config: ProxyConfig,
        transport: TargetCheckedTcpConnector,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        let connector =
            build_http_connector(transport, tls_root_store, proxy_config.attribution.clone());
        Self {
            connector,
            proxy_config,
        }
    }
}

impl Service<Request<Body>> for UpstreamClient {
    type Output = Response;
    type Error = OpaqueError;

    async fn serve(&self, mut req: Request<Body>) -> Result<Self::Output, Self::Error> {
        let options_asterisk = req.extensions().get_ref::<OptionsAsterisk>().is_some();
        if options_asterisk && req.method().as_str() != "OPTIONS" {
            return Err(OpaqueError::from_static_str(
                "asterisk request-target is only supported for OPTIONS",
            ));
        }
        if req.uri().as_str() == "*" && !options_asterisk {
            return Err(OpaqueError::from_static_str(
                "asterisk request-target requires an authorized route",
            ));
        }
        if options_asterisk {
            // Establish the authorized route using the original absolute
            // authority, then restore the asterisk form for HTTP/1.1. Rama 0.3's
            // HTTP/2 adapter cannot preserve trusted authority and the asterisk path.
            *req.version_mut() = Version::HTTP_11;
        }

        let protocol = req.protocol().cloned();
        let secure = protocol.as_ref().is_some_and(rama_net::Protocol::is_secure);
        let default_port = protocol
            .as_ref()
            .and_then(rama_net::Protocol::default_port)
            .unwrap_or(80);
        // The alpha request context preferred a domain ProxyTarget after the
        // URI authority and before TLS SNI, Forwarded, and Host fallback.
        let target = if req.uri().host().is_none() {
            req.extensions()
                .get_ref::<crate::ProxyTarget>()
                .filter(|target| matches!(&target.0.host, Host::Name(_)))
                .map(|target| target.0.clone())
                .or_else(|| {
                    req.authority()
                        .map(|authority| authority.into_host_with_port_or(default_port))
                })
        } else {
            req.authority()
                .map(|authority| authority.into_host_with_port_or(default_port))
        };
        let authority = target
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "<unknown>".to_string());
        let proxy = target.as_ref().map_or_else(
            || self.proxy_config.proxy_for_protocol(/*is_secure*/ false),
            |target| self.proxy_config.proxy_for_target(target, secure),
        );
        req.headers_mut().remove(TOOL_CALL_HEADER);
        match proxy.as_ref() {
            Some(proxy) => info!(
                "HTTP upstream route selected (target={authority}, route=upstream_proxy, proxy={})",
                proxy.address
            ),
            None => info!("HTTP upstream route selected (target={authority}, route=direct)"),
        }
        if let Some(proxy) = proxy {
            if protocol
                .as_ref()
                .is_some_and(|protocol| *protocol == rama_net::Protocol::HTTP)
                && let Some(attribution) = &self.proxy_config.attribution
                && let Some(id) = attribution.call_id_for(&proxy)
            {
                req.headers_mut().insert(TOOL_CALL_HEADER, id);
            }
            req.extensions().insert(proxy);
        }

        let connect_started_at = Instant::now();
        let EstablishedClientConnection {
            input: mut req,
            conn: http_connection,
        } = match self.connector.serve(req).await {
            Ok(connection) => {
                info!(
                    "HTTP upstream connection established (target={authority}, elapsed_ms={})",
                    connect_started_at.elapsed().as_millis()
                );
                connection
            }
            Err(err) => {
                warn!(
                    "HTTP upstream connection failed (target={authority}, elapsed_ms={})",
                    connect_started_at.elapsed().as_millis()
                );
                return Err(err);
            }
        };

        req.extensions().extend(http_connection.extensions());

        if options_asterisk {
            *req.uri_mut() = Uri::parse("*".to_string())
                .map_err(rama_core::error::ErrorExt::into_opaque_error)?;
            *req.version_mut() = Version::HTTP_11;
        }

        let request_started_at = Instant::now();
        match http_connection.serve(req).await {
            Ok(resp) => {
                info!(
                    "HTTP upstream response headers received (target={authority}, elapsed_ms={})",
                    request_started_at.elapsed().as_millis()
                );
                Ok(resp)
            }
            Err(err) => {
                warn!(
                    "HTTP upstream response headers failed (target={authority}, elapsed_ms={})",
                    request_started_at.elapsed().as_millis()
                );
                Err(err
                    .context("HTTP upstream request failed")
                    .into_opaque_error())
            }
        }
    }
}

fn is_options_asterisk_request(req: &Request<Body>) -> bool {
    req.method().as_str() == "OPTIONS" && req.extensions().get_ref::<OptionsAsterisk>().is_some()
}

fn build_http_connector(
    transport: TargetCheckedTcpConnector,
    tls_root_store: Arc<rustls::RootCertStore>,
    attribution: Option<ToolCallAttribution>,
) -> BoxService<
    Request<Body>,
    EstablishedClientConnection<HttpClientService<Body>, Request<Body>>,
    OpaqueError,
> {
    ensure_rustls_crypto_provider();
    let connector = service_fn(move |req: Request<Body>| {
        let transport = transport.clone();
        let tls_root_store = tls_root_store.clone();
        let attribution = attribution.clone();
        let options_asterisk = is_options_asterisk_request(&req);
        async move {
            let connect_call_id = attribution
                .as_ref()
                .zip(req.extensions().get_ref::<ProxyAddress>())
                .and_then(|(attribution, proxy)| attribution.call_id_for(proxy));
            let proxy = upstream_proxy_connector(transport, connect_call_id);
            let mut client_config =
                rustls::ClientConfig::builder_with_protocol_versions(rustls::ALL_VERSIONS)
                    .with_root_certificates(tls_root_store)
                    .with_no_client_auth();
            client_config.alpn_protocols = if options_asterisk {
                vec![b"http/1.1".to_vec()]
            } else {
                vec![b"h2".to_vec(), b"http/1.1".to_vec()]
            };
            let tls_config = TlsClientConfig::new()
                .with_modify_rustls_config(move |_| Ok(client_config.clone()));
            let tls = TlsConnectorLayer::auto()
                .with_base_config(tls_config)
                .into_layer(proxy);
            let tls = RequestVersionAdapter::new(tls).with_default_version(Version::HTTP_11);
            let connector = HttpConnectorLayer::<Body>::default().into_layer(tls);
            connector.serve(req).await
        }
    });
    connector.boxed()
}

#[cfg(test)]
#[path = "upstream_tests.rs"]
mod tests;

#[cfg(target_os = "macos")]
fn build_unix_connector(
    path: &str,
) -> BoxService<
    Request<Body>,
    EstablishedClientConnection<HttpClientService<Body>, Request<Body>>,
    OpaqueError,
> {
    let transport = UnixConnector::fixed(path);
    let connector = HttpConnectorLayer::<Body>::default().into_layer(transport);
    connector.boxed()
}
