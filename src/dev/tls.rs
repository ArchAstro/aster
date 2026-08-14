use std::collections::HashMap;
use std::convert::Infallible;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::BufReader;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use fs2::FileExt;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CONNECTION, HOST, UPGRADE};
use hyper::http::uri::Authority;
use hyper::server::conn::http1::Builder as ServerBuilder;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;
use tokio_rustls::rustls::{self, ServerConfig};
use tokio_rustls::TlsAcceptor;

use crate::config::{DevServiceConfig, DevTlsProxyConfig, DevWorkspaceConfig};

const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";
const DYNAMIC_CERT_DIR: &str = "hosts";
const DYNAMIC_CERT_LOCK: &str = "hosts.lock";
const MAX_DYNAMIC_CERTIFICATES: usize = 64;

pub fn setup_tls(workspace_root: &Path, config: &DevWorkspaceConfig, edge: &str) -> Result<()> {
    let (_, tls) = configured_proxy(config, edge)?;
    validate_local_dns(tls)?;
    let cert_dir = certificate_directory(workspace_root, edge)?;
    fs::create_dir_all(&cert_dir)
        .with_context(|| format!("failed to create {}", cert_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cert_dir, fs::Permissions::from_mode(0o700))?;
    }

    let mkcert = std::env::var_os("ASTER_MKCERT_BIN").unwrap_or_else(|| "mkcert".into());
    run_mkcert(
        Command::new(&mkcert).arg("-install"),
        "install the local CA",
    )?;

    let cert_path = cert_dir.join(CERT_FILE);
    let key_path = cert_dir.join(KEY_FILE);
    let _dynamic_lock = lock_dynamic_certificates(&cert_dir)?;
    let dynamic_dir = cert_dir.join(DYNAMIC_CERT_DIR);
    if dynamic_dir.exists() {
        fs::remove_dir_all(&dynamic_dir)
            .with_context(|| format!("failed to clear {}", dynamic_dir.display()))?;
    }
    let mut command = Command::new(&mkcert);
    command
        .arg("-cert-file")
        .arg(&cert_path)
        .arg("-key-file")
        .arg(&key_path)
        .args(&tls.certificate_hosts);
    run_mkcert(&mut command, "generate the development certificate")?;
    if !cert_path.is_file() || !key_path.is_file() {
        bail!(
            "mkcert succeeded but did not write {} and {}",
            cert_path.display(),
            key_path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
    }
    println!(
        "TLS edge '{edge}' certificate ready in {}",
        cert_dir.display()
    );
    Ok(())
}

fn run_mkcert(command: &mut Command, action: &str) -> Result<()> {
    let status = command.status().with_context(|| {
        format!("failed to run mkcert to {action}; install it with `brew install mkcert`")
    })?;
    if !status.success() {
        bail!("mkcert failed to {action} with status {status}");
    }
    Ok(())
}

pub fn serve_tls(workspace_root: &Path, config: &DevWorkspaceConfig, edge: &str) -> Result<()> {
    let (service, tls) = configured_proxy(config, edge)?;
    validate_local_dns(tls)?;
    let cert_dir = certificate_directory(workspace_root, edge)?;
    let cert_path = cert_dir.join(CERT_FILE);
    let key_path = cert_dir.join(KEY_FILE);
    if !cert_path.is_file() || !key_path.is_file() {
        bail!("TLS edge '{edge}' has no certificate; run `aster services tls setup {edge}`");
    }

    let ports = match std::env::var("ASTER_RESOLVED_PORTS") {
        Ok(encoded) => serde_json::from_str(&encoded)
            .context("ASTER_RESOLVED_PORTS is not a valid named-port map")?,
        Err(std::env::VarError::NotPresent) => super::resolve_dev_ports(workspace_root, config)?,
        Err(error) => return Err(error).context("failed to read ASTER_RESOLVED_PORTS"),
    };
    let port_name = service
        .port
        .as_deref()
        .expect("TLS proxy configuration requires a service port");
    let listen_port = ports
        .get(port_name)
        .copied()
        .ok_or_else(|| anyhow!("TLS edge '{edge}' references unknown port '{port_name}'"))?;
    let mut routes = Vec::new();
    for route in &tls.routes {
        let port = ports.get(&route.upstream_port).copied().ok_or_else(|| {
            anyhow!(
                "TLS route references unknown port '{}'",
                route.upstream_port
            )
        })?;
        let selector = match (&route.host, &route.host_suffix) {
            (Some(host), None) => RouteSelector::Exact(host.to_ascii_lowercase()),
            (None, Some(suffix)) => RouteSelector::Suffix(suffix.to_ascii_lowercase()),
            _ => unreachable!("TLS route configuration is validated"),
        };
        routes.push(ResolvedRoute { selector, port });
    }
    routes.sort_by_key(|route| std::cmp::Reverse(route.selector.len()));

    let runtime = tokio::runtime::Runtime::new().context("failed to start TLS runtime")?;
    runtime.block_on(serve(
        edge.to_string(),
        listen_port,
        cert_dir,
        cert_path,
        key_path,
        tls.certificate_hosts.clone(),
        Arc::new(routes),
    ))
}

async fn serve(
    edge: String,
    listen_port: u16,
    cert_dir: PathBuf,
    cert_path: PathBuf,
    key_path: PathBuf,
    certificate_hosts: Vec<String>,
    routes: Arc<Vec<ResolvedRoute>>,
) -> Result<()> {
    let tls = Arc::new(load_server_config(
        &cert_path,
        &key_path,
        cert_dir,
        certificate_hosts,
        routes.clone(),
    )?);
    let listener = TcpListener::bind(("127.0.0.1", listen_port))
        .await
        .with_context(|| format!("TLS edge '{edge}' could not bind 127.0.0.1:{listen_port}"))?;
    println!("TLS edge '{edge}' ready https://127.0.0.1:{listen_port}");

    loop {
        let (stream, peer) = listener.accept().await?;
        let acceptor = TlsAcceptor::from(tls.clone());
        let routes = routes.clone();
        tokio::spawn(async move {
            let stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("TLS handshake from {peer} failed: {error}");
                    return;
                }
            };
            let service = service_fn(move |request| proxy_request(request, routes.clone()));
            let result = ServerBuilder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
            if let Err(error) = result {
                eprintln!("TLS connection from {peer} failed: {error}");
            }
        });
    }
}

type ProxyBody = BoxBody<Bytes, hyper::Error>;

async fn proxy_request(
    mut request: Request<Incoming>,
    routes: Arc<Vec<ResolvedRoute>>,
) -> std::result::Result<Response<ProxyBody>, Infallible> {
    let authority = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_host_authority);
    let Some(authority) = authority else {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "missing or invalid Host header\n",
        ));
    };
    let hostname = authority.host().to_ascii_lowercase();
    let Some(port) = match_route(&hostname, &routes) else {
        return Ok(text_response(
            StatusCode::MISDIRECTED_REQUEST,
            "unknown TLS hostname\n",
        ));
    };
    let upgrade_requested = request.headers().contains_key(UPGRADE)
        && request
            .headers()
            .get(CONNECTION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            });
    let downstream_upgrade = upgrade_requested.then(|| hyper::upgrade::on(&mut request));

    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let uri = format!("http://127.0.0.1:{port}{path}");
    match uri.parse::<Uri>() {
        Ok(uri) => *request.uri_mut() = uri,
        Err(_) => {
            return Ok(text_response(
                StatusCode::BAD_REQUEST,
                "invalid request URI\n",
            ))
        }
    }
    request
        .headers_mut()
        .insert("x-forwarded-proto", HeaderValue::from_static("https"));
    let forwarded_host = HeaderValue::from_str(authority.as_str())
        .expect("parsed HTTP authority is a valid header value");
    request
        .headers_mut()
        .insert("x-forwarded-host", forwarded_host);

    let client: Client<HttpConnector, Incoming> =
        Client::builder(TokioExecutor::new()).build_http();
    match client.request(request).await {
        Ok(mut response) => {
            if response.status() == StatusCode::SWITCHING_PROTOCOLS {
                if let Some(downstream_upgrade) = downstream_upgrade {
                    let upstream_upgrade = hyper::upgrade::on(&mut response);
                    tokio::spawn(async move {
                        let (downstream, upstream) =
                            match tokio::try_join!(downstream_upgrade, upstream_upgrade) {
                                Ok(upgrades) => upgrades,
                                Err(error) => {
                                    eprintln!("TLS upgrade failed: {error}");
                                    return;
                                }
                            };
                        let mut downstream = TokioIo::new(downstream);
                        let mut upstream = TokioIo::new(upstream);
                        if let Err(error) = copy_bidirectional(&mut downstream, &mut upstream).await
                        {
                            eprintln!("TLS upgraded connection failed: {error}");
                        }
                    });
                }
            }
            Ok(response.map(BodyExt::boxed))
        }
        Err(error) => Ok(text_response(
            StatusCode::BAD_GATEWAY,
            &format!("TLS upstream unavailable: {error}\n"),
        )),
    }
}

fn text_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let body = Full::new(Bytes::copy_from_slice(message.as_bytes()))
        .map_err(|never| match never {})
        .boxed();
    Response::builder().status(status).body(body).unwrap()
}

fn parse_host_authority(value: &str) -> Option<Authority> {
    if value.contains('@') || value.matches(':').count() > 1 {
        return None;
    }
    if let Some((hostname, port)) = value.rsplit_once(':') {
        if hostname.is_empty() || port.parse::<u16>().ok().filter(|port| *port > 0).is_none() {
            return None;
        }
    }
    let authority = value.parse::<Authority>().ok()?;
    if authority.host().is_empty() {
        return None;
    }
    Some(authority)
}

#[derive(Clone)]
struct ResolvedRoute {
    selector: RouteSelector,
    port: u16,
}

#[derive(Clone)]
enum RouteSelector {
    Exact(String),
    Suffix(String),
}

impl RouteSelector {
    fn len(&self) -> usize {
        match self {
            Self::Exact(value) | Self::Suffix(value) => value.len(),
        }
    }

    fn matches(&self, hostname: &str) -> bool {
        match self {
            Self::Exact(value) => hostname == value,
            Self::Suffix(value) => hostname.ends_with(value) && hostname.len() > value.len(),
        }
    }
}

fn match_route(hostname: &str, routes: &[ResolvedRoute]) -> Option<u16> {
    routes
        .iter()
        .find(|route| route.selector.matches(hostname))
        .map(|route| route.port)
}

fn load_certified_key(cert_path: &Path, key_path: &Path) -> Result<Arc<CertifiedKey>> {
    let mut cert_reader = BufReader::new(fs::File::open(cert_path)?);
    let certificates =
        rustls_pemfile::certs(&mut cert_reader).collect::<std::result::Result<Vec<_>, _>>()?;
    let mut key_reader = BufReader::new(fs::File::open(key_path)?);
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or_else(|| anyhow!("no private key found in {}", key_path.display()))?;
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    Ok(Arc::new(
        CertifiedKey::from_der(certificates, key, &provider)
            .context("certificate and private key do not match")?,
    ))
}

fn load_server_config(
    cert_path: &Path,
    key_path: &Path,
    cert_dir: PathBuf,
    certificate_hosts: Vec<String>,
    routes: Arc<Vec<ResolvedRoute>>,
) -> Result<ServerConfig> {
    let default = load_certified_key(cert_path, key_path)?;
    let resolver = DynamicCertificateResolver {
        default,
        certificate_hosts,
        routes,
        cert_dir,
        mkcert: std::env::var_os("ASTER_MKCERT_BIN").unwrap_or_else(|| "mkcert".into()),
        cache: Mutex::new(HashMap::new()),
    };
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

struct DynamicCertificateResolver {
    default: Arc<CertifiedKey>,
    certificate_hosts: Vec<String>,
    routes: Arc<Vec<ResolvedRoute>>,
    cert_dir: PathBuf,
    mkcert: OsString,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for DynamicCertificateResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DynamicCertificateResolver")
            .field("certificate_hosts", &self.certificate_hosts)
            .field("cert_dir", &self.cert_dir)
            .finish_non_exhaustive()
    }
}

impl ResolvesServerCert for DynamicCertificateResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let hostname = client_hello.server_name()?.to_ascii_lowercase();
        if self
            .certificate_hosts
            .iter()
            .any(|configured| certificate_host_covers(configured, &hostname))
        {
            return Some(self.default.clone());
        }
        match_route(&hostname, &self.routes)?;

        let mut cache = self.cache.lock().ok()?;
        if let Some(certified) = cache.get(&hostname) {
            return Some(certified.clone());
        }
        match issue_dynamic_certificate(&self.mkcert, &self.cert_dir, &hostname) {
            Ok(certified) => {
                cache.insert(hostname, certified.clone());
                Some(certified)
            }
            Err(error) => {
                eprintln!("TLS certificate issuance failed: {error:#}");
                None
            }
        }
    }
}

fn certificate_host_covers(configured: &str, hostname: &str) -> bool {
    let configured = configured.to_ascii_lowercase();
    if let Some(suffix) = configured.strip_prefix("*.") {
        hostname.strip_suffix(suffix).is_some_and(|prefix| {
            prefix.ends_with('.') && !prefix[..prefix.len() - 1].contains('.')
        })
    } else {
        hostname == configured
    }
}

fn issue_dynamic_certificate(
    mkcert: &OsString,
    cert_dir: &Path,
    hostname: &str,
) -> Result<Arc<CertifiedKey>> {
    let _dynamic_lock = lock_dynamic_certificates(cert_dir)?;
    let dynamic_dir = cert_dir.join(DYNAMIC_CERT_DIR);
    let host_dir = dynamic_dir.join(hostname);
    let cert_path = host_dir.join(CERT_FILE);
    let key_path = host_dir.join(KEY_FILE);
    if cert_path.is_file() && key_path.is_file() {
        if let Ok(certified) = load_certified_key(&cert_path, &key_path) {
            return Ok(certified);
        }
    }
    if host_dir.exists() {
        fs::remove_dir_all(&host_dir)
            .with_context(|| format!("failed to clear invalid cache {}", host_dir.display()))?;
    }
    fs::create_dir_all(&dynamic_dir)?;
    let certificate_count = fs::read_dir(&dynamic_dir)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().is_dir())
        .count();
    if certificate_count >= MAX_DYNAMIC_CERTIFICATES {
        bail!("dynamic TLS certificate limit of {MAX_DYNAMIC_CERTIFICATES} reached");
    }
    fs::create_dir(&host_dir)
        .with_context(|| format!("failed to create {}", host_dir.display()))?;

    let process_id = std::process::id();
    let temp_cert_path = host_dir.join(format!(".{CERT_FILE}.{process_id}.tmp"));
    let temp_key_path = host_dir.join(format!(".{KEY_FILE}.{process_id}.tmp"));
    let mut command = Command::new(mkcert);
    command
        .arg("-cert-file")
        .arg(&temp_cert_path)
        .arg("-key-file")
        .arg(&temp_key_path)
        .arg(hostname);
    if let Err(error) = run_mkcert(
        &mut command,
        &format!("generate a certificate for {hostname}"),
    ) {
        let _ = fs::remove_file(&temp_cert_path);
        let _ = fs::remove_file(&temp_key_path);
        let _ = fs::remove_dir(&host_dir);
        return Err(error);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&temp_key_path, fs::Permissions::from_mode(0o600)) {
            clear_failed_certificate(&host_dir, &temp_cert_path, &temp_key_path);
            return Err(error.into());
        }
    }
    let certified = match load_certified_key(&temp_cert_path, &temp_key_path) {
        Ok(certified) => certified,
        Err(error) => {
            clear_failed_certificate(&host_dir, &temp_cert_path, &temp_key_path);
            return Err(error);
        }
    };
    if let Err(error) = fs::rename(&temp_cert_path, &cert_path) {
        clear_failed_certificate(&host_dir, &temp_cert_path, &temp_key_path);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&temp_key_path, &key_path) {
        clear_failed_certificate(&host_dir, &cert_path, &temp_key_path);
        return Err(error.into());
    }
    Ok(certified)
}

fn lock_dynamic_certificates(cert_dir: &Path) -> Result<File> {
    fs::create_dir_all(cert_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(cert_dir.join(DYNAMIC_CERT_LOCK))?;
    FileExt::lock_exclusive(&lock)?;
    Ok(lock)
}

fn clear_failed_certificate(host_dir: &Path, cert_path: &Path, key_path: &Path) {
    let _ = fs::remove_file(cert_path);
    let _ = fs::remove_file(key_path);
    let _ = fs::remove_dir(host_dir);
}

fn configured_proxy<'a>(
    config: &'a DevWorkspaceConfig,
    edge: &str,
) -> Result<(&'a DevServiceConfig, &'a DevTlsProxyConfig)> {
    let service = config.services.get(edge).ok_or_else(|| {
        let mut known = config
            .services
            .iter()
            .filter_map(|(name, service)| service.tls_proxy.as_ref().map(|_| name.as_str()))
            .collect::<Vec<_>>();
        known.sort_unstable();
        if known.is_empty() {
            anyhow!("unknown TLS service '{edge}'; no TLS proxy services are configured")
        } else {
            anyhow!(
                "unknown TLS service '{edge}'; configured TLS services: {}",
                known.join(", ")
            )
        }
    })?;
    let tls = service
        .tls_proxy
        .as_ref()
        .ok_or_else(|| anyhow!("service '{edge}' is not a TLS proxy"))?;
    Ok((service, tls))
}

fn certificate_directory(workspace_root: &Path, edge: &str) -> Result<PathBuf> {
    if edge.is_empty()
        || Path::new(edge).components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("invalid TLS service name '{edge}'");
    }
    Ok(workspace_root.join(".aster").join("tls").join(edge))
}

fn validate_local_dns(tls: &DevTlsProxyConfig) -> Result<()> {
    use std::net::{IpAddr, ToSocketAddrs};

    let Some(domain) = tls.dns_domain.as_deref() else {
        return Ok(());
    };
    for probe in dns_probe_hostnames(tls, domain) {
        let resolves_local = (probe.as_str(), 443)
            .to_socket_addrs()
            .map(|addresses| {
                addresses.into_iter().any(|address| {
                    matches!(address.ip(), IpAddr::V4(ip) if ip.is_loopback())
                        || matches!(address.ip(), IpAddr::V6(ip) if ip.is_loopback())
                })
            })
            .unwrap_or(false);
        if !resolves_local {
            bail!(
                "TLS hostname '{probe}' does not resolve to loopback. Configure dnsmasq and the OS resolver, then flush Chrome's DNS cache. Fish setup:\n  brew install dnsmasq\n  set DNSMASQ_CONF (brew --prefix)/etc/dnsmasq.conf\n  grep -qxF 'address=/.{domain}/127.0.0.1' \"$DNSMASQ_CONF\"; or printf '\\naddress=/.{domain}/127.0.0.1\\n' | sudo tee -a \"$DNSMASQ_CONF\"\n  sudo brew services restart dnsmasq\n  sudo mkdir -p /etc/resolver\n  printf 'nameserver 127.0.0.1\\nport 53\\n' | sudo tee /etc/resolver/{domain}\nThen fully quit Chrome; if Secure DNS bypasses the OS resolver, disable it for local testing."
            );
        }
    }
    Ok(())
}

fn dns_probe_hostnames(
    tls: &DevTlsProxyConfig,
    domain: &str,
) -> std::collections::BTreeSet<String> {
    let mut probes = std::collections::BTreeSet::from([domain.to_string(), tls.open_host.clone()]);
    for hostname in &tls.certificate_hosts {
        probes.insert(match hostname.strip_prefix("*.") {
            Some(suffix) => format!("aster-dns-probe.{suffix}"),
            None => hostname.clone(),
        });
    }
    probes.extend(
        tls.routes
            .iter()
            .filter_map(|route| route.open_host.clone()),
    );
    probes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_route_matches_any_subdomain_depth_but_not_the_suffix_itself() {
        let routes = vec![ResolvedRoute {
            selector: RouteSelector::Suffix(".sites.test".to_string()),
            port: 4100,
        }];
        assert_eq!(match_route("one.sites.test", &routes), Some(4100));
        assert_eq!(match_route("one.two.sites.test", &routes), Some(4100));
        assert_eq!(match_route("sites.test", &routes), None);
    }

    #[test]
    fn host_authority_accepts_numeric_ports_and_rejects_userinfo() {
        let authority = parse_host_authority("app.example.test:8443").unwrap();
        assert_eq!(authority.host(), "app.example.test");
        assert_eq!(authority.port_u16(), Some(8443));
        assert!(parse_host_authority("app.example.test:443@evil.example").is_none());
        assert!(parse_host_authority("app.example.test:not-a-port").is_none());
    }

    #[test]
    fn dns_validation_probes_apex_open_host_and_a_wildcard_site_hostname() {
        let tls = DevTlsProxyConfig {
            certificate_hosts: vec![
                "app.example.test".to_string(),
                "*.local.example.test".to_string(),
            ],
            open_host: "app.example.test".to_string(),
            dns_domain: Some("example.test".to_string()),
            routes: vec![crate::config::DevTlsRouteConfig {
                host: None,
                host_suffix: Some(".example.test".to_string()),
                open_host: Some("demo.team.example.test".to_string()),
                upstream_port: "gateway".to_string(),
            }],
        };
        assert_eq!(
            dns_probe_hostnames(&tls, "example.test")
                .into_iter()
                .collect::<Vec<_>>(),
            [
                "app.example.test",
                "aster-dns-probe.local.example.test",
                "demo.team.example.test",
                "example.test"
            ]
        );
    }
}
