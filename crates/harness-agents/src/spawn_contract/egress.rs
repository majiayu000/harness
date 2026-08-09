use harness_core::agent::AGENT_EGRESS_PROXY_IMAGE_ENV;
use harness_core::config::isolation::IsolationTier;
use harness_core::error::HarnessError;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::process::{Command, Output};
use std::time::Duration;

use super::docker_ownership::{append_string_labels, unique_resource_name, ManagedDockerResource};

pub(super) const LEGACY_EGRESS_PROXY_ENV: &str = "HARNESS_AGENT_EGRESS_PROXY";
pub(crate) const CONTAINER_EGRESS_CANARY_VERIFIED: &str = "HARNESS_EGRESS_CANARY_VERIFIED_V1";
const DEFAULT_EGRESS_PROXY_IMAGE: &str = "harness-egress-proxy:latest";
const PROXY_PORT: u16 = 8080;
const PROXY_ALIAS: &str = "egress-proxy";
const PROXY_HEALTH_ATTEMPTS: usize = 50;
const PROXY_HEALTH_INTERVAL: Duration = Duration::from_millis(100);
const PROXY_CANARY_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) fn proxy_env_keys() -> [&'static str; 6] {
    [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ]
}

pub(super) fn apply_proxy_env(env: &mut BTreeMap<String, String>, proxy_url: &str) {
    for key in proxy_env_keys() {
        env.insert(key.to_string(), proxy_url.to_string());
    }
    for key in ["NO_PROXY", "no_proxy"] {
        env.insert(key.to_string(), "localhost,127.0.0.1".to_string());
    }
}

pub(super) fn container_canary_command(
    program: OsString,
    child_args: Vec<OsString>,
    allowed_host: &str,
) -> Vec<OsString> {
    const SCRIPT: &str = r#"allowed_host="$1"
shift
curl --silent --show-error --noproxy '' --proxy "$HTTP_PROXY" --output /dev/null --max-time 10 "https://${allowed_host}/" || { echo "first-party egress proxy could not reach allowlisted host ${allowed_host}" >&2; exit 70; }
status="$(curl --silent --show-error --noproxy '' --proxy "$HTTP_PROXY" --output /dev/null --write-out '%{http_code}' --max-time 5 http://harness-egress-canary.invalid/)" || { echo 'first-party egress proxy canary was unreachable' >&2; exit 70; }
if [ "$status" != "403" ]; then echo "first-party egress proxy canary returned $status instead of 403" >&2; exit 70; fi
printf '%s\n' 'HARNESS_EGRESS_CANARY_VERIFIED_V1'
exec "$@""#;
    let mut args = vec![
        OsString::from("sh"),
        OsString::from("-c"),
        OsString::from(SCRIPT),
        OsString::from("harness-egress-canary"),
        OsString::from(allowed_host),
        program,
    ];
    args.extend(child_args);
    args
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EgressProxyRoute {
    proxy_url: String,
    local_proxy_port: Option<u16>,
    container_network: Option<String>,
}

impl EgressProxyRoute {
    pub(super) fn host(port: u16) -> Self {
        Self {
            proxy_url: format!("http://127.0.0.1:{port}"),
            local_proxy_port: Some(port),
            container_network: None,
        }
    }

    pub(super) fn container(network: String, proxy_url: String) -> Self {
        Self {
            proxy_url,
            local_proxy_port: None,
            container_network: Some(network),
        }
    }

    pub(super) fn proxy_url(&self) -> &str {
        &self.proxy_url
    }

    pub(super) fn local_proxy_port(&self) -> Option<u16> {
        self.local_proxy_port
    }

    pub(super) fn container_network(&self) -> Option<&str> {
        self.container_network.as_deref()
    }

    pub(super) fn requires_container_canary(&self) -> bool {
        self.container_network.is_some()
    }
}

#[derive(Debug)]
pub(crate) struct EgressProxyLease {
    route: EgressProxyRoute,
    container_name: String,
    network_name: Option<String>,
}

#[derive(Debug)]
struct EgressCleanupRequest {
    container_name: String,
    network_name: Option<String>,
}

impl EgressProxyLease {
    pub(super) fn start(
        tier: IsolationTier,
        allowlist: &[String],
        env_vars: &HashMap<String, String>,
    ) -> Result<Self, HarnessError> {
        if env_vars
            .get(LEGACY_EGRESS_PROXY_ENV)
            .filter(|value| !value.trim().is_empty())
            .is_some()
        {
            return Err(agent_error(format!(
                "{LEGACY_EGRESS_PROXY_ENV} is no longer accepted because external proxy URLs cannot prove allowlist enforcement; configure {AGENT_EGRESS_PROXY_IMAGE_ENV} instead"
            )));
        }
        let image = env_vars
            .get(AGENT_EGRESS_PROXY_IMAGE_ENV)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_EGRESS_PROXY_IMAGE);
        let verification_host = allowlist.first().ok_or_else(|| {
            agent_error("first-party egress proxy requires a non-empty allowlist")
        })?;
        let suffix = unique_resource_name("");
        let container_name = format!("harness-egress-proxy-{suffix}");
        let allowlist_value = allowlist.join(",");
        let allow_rfc2544 = docker_runtime_name()?.eq_ignore_ascii_case("orbstack");

        match tier {
            IsolationTier::Host => {
                start_proxy_container(
                    &container_name,
                    None,
                    image,
                    &allowlist_value,
                    allow_rfc2544,
                    true,
                )?;
                let result = (|| {
                    wait_for_proxy_health(&container_name)?;
                    let port = published_proxy_port(&container_name)?;
                    verify_proxy_canary(port, verification_host)?;
                    Ok(Self {
                        route: EgressProxyRoute::host(port),
                        container_name: container_name.clone(),
                        network_name: None,
                    })
                })();
                if result.is_err() {
                    cleanup_container(&container_name);
                }
                result
            }
            IsolationTier::Container => {
                let network_name = format!("harness-egress-{suffix}");
                let mut network_args = vec![
                    "network".to_string(),
                    "create".to_string(),
                    "--internal".to_string(),
                ];
                append_string_labels(&mut network_args, ManagedDockerResource::EgressNetwork);
                network_args.push(network_name.clone());
                run_docker_owned(&network_args)?;
                let result = (|| {
                    start_proxy_container(
                        &container_name,
                        Some(&network_name),
                        image,
                        &allowlist_value,
                        allow_rfc2544,
                        false,
                    )?;
                    run_docker(&["network", "connect", "bridge", &container_name])?;
                    wait_for_proxy_health(&container_name)?;
                    Ok(Self {
                        route: EgressProxyRoute::container(
                            network_name.clone(),
                            format!("http://{PROXY_ALIAS}:{PROXY_PORT}"),
                        ),
                        container_name: container_name.clone(),
                        network_name: Some(network_name.clone()),
                    })
                })();
                if result.is_err() {
                    cleanup_container(&container_name);
                    cleanup_network(&network_name);
                }
                result
            }
            IsolationTier::Microvm => Err(agent_error(
                "egress proxy cannot start for the unimplemented microvm isolation tier",
            )),
        }
    }

    pub(super) fn route(&self) -> &EgressProxyRoute {
        &self.route
    }

    pub(crate) fn validate_health(&self) -> Result<(), HarnessError> {
        let status = proxy_health_status(&self.container_name)?;
        if status == "running healthy" {
            Ok(())
        } else {
            Err(agent_error(format!(
                "first-party egress proxy is not healthy for adapter reuse: {status}"
            )))
        }
    }

    #[cfg(test)]
    pub(super) fn container_name(&self) -> &str {
        &self.container_name
    }
}

impl Drop for EgressProxyLease {
    fn drop(&mut self) {
        schedule_cleanup(EgressCleanupRequest {
            container_name: std::mem::take(&mut self.container_name),
            network_name: self.network_name.take(),
        });
    }
}

fn schedule_cleanup(request: EgressCleanupRequest) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn_blocking(move || cleanup_resources(request));
    } else {
        // Synchronous callers have no async worker to protect, and completing
        // cleanup here prevents short-lived CLI processes from leaking Docker
        // resources during process exit.
        cleanup_resources(request);
    }
}

fn cleanup_resources(request: EgressCleanupRequest) {
    if request.container_name.is_empty() {
        return;
    }
    cleanup_container(&request.container_name);
    if let Some(network_name) = request.network_name {
        cleanup_network(&network_name);
    }
}

fn start_proxy_container(
    name: &str,
    network: Option<&str>,
    image: &str,
    allowlist: &str,
    allow_rfc2544: bool,
    publish_local_port: bool,
) -> Result<(), HarnessError> {
    let mut args = vec![
        "run".to_string(),
        "--detach".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        name.to_string(),
    ];
    append_string_labels(&mut args, ManagedDockerResource::EgressProxy);
    if let Some(network) = network {
        args.extend([
            "--network".to_string(),
            network.to_string(),
            "--network-alias".to_string(),
            PROXY_ALIAS.to_string(),
        ]);
    }
    if publish_local_port {
        args.extend(["--publish".to_string(), format!("127.0.0.1::{PROXY_PORT}")]);
    }
    args.extend([
        "--env".to_string(),
        format!("HARNESS_EGRESS_ALLOWLIST={allowlist}"),
    ]);
    if allow_rfc2544 {
        args.extend([
            "--env".to_string(),
            "HARNESS_EGRESS_ALLOW_RFC2544_DNS=1".to_string(),
        ]);
    }
    args.push(image.to_string());
    run_docker_owned(&args).map(|_| ())
}

fn docker_runtime_name() -> Result<String, HarnessError> {
    let output = run_docker(&["info", "--format", "{{.OperatingSystem}}"])?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn wait_for_proxy_health(container_name: &str) -> Result<(), HarnessError> {
    for _ in 0..PROXY_HEALTH_ATTEMPTS {
        let status = proxy_health_status(container_name)?;
        match status.as_str() {
            "running healthy" => return Ok(()),
            value if value.starts_with("exited ") || value.ends_with(" unhealthy") => {
                return Err(agent_error(format!(
                    "first-party egress proxy failed health validation: {value}"
                )));
            }
            _ => std::thread::sleep(PROXY_HEALTH_INTERVAL),
        }
    }
    Err(agent_error(
        "first-party egress proxy did not become healthy before dispatch",
    ))
}

fn proxy_health_status(container_name: &str) -> Result<String, HarnessError> {
    let output = run_docker(&[
        "inspect",
        "--format",
        "{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}",
        container_name,
    ])?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn published_proxy_port(container_name: &str) -> Result<u16, HarnessError> {
    let output = run_docker(&["port", container_name, "8080/tcp"])?;
    let rendered = String::from_utf8_lossy(&output.stdout);
    rendered
        .lines()
        .find_map(|line| {
            line.rsplit_once(':')
                .and_then(|(_, port)| port.parse().ok())
        })
        .ok_or_else(|| agent_error("Docker did not publish the first-party proxy loopback port"))
}

fn verify_proxy_canary(port: u16, allowed_host: &str) -> Result<(), HarnessError> {
    let allowed_request = format!(
        "CONNECT {allowed_host}:443 HTTP/1.1\r\nHost: {allowed_host}:443\r\nConnection: close\r\n\r\n"
    );
    verify_proxy_response(
        port,
        allowed_request.as_bytes(),
        b"HTTP/1.1 200 Connection Established\r\n",
        "could not reach the configured allowlisted host",
    )?;
    verify_proxy_rejects_mismatched_sni(port, allowed_host)?;
    verify_proxy_response(
        port,
        b"GET http://harness-egress-canary.invalid/ HTTP/1.1\r\nHost: harness-egress-canary.invalid\r\nConnection: close\r\n\r\n",
        b"HTTP/1.1 403 Forbidden\r\n",
        "did not return the required 403 refusal",
    )
}

fn verify_proxy_rejects_mismatched_sni(port: u16, allowed_host: &str) -> Result<(), HarnessError> {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let mut stream =
        TcpStream::connect_timeout(&address.into(), PROXY_CANARY_TIMEOUT).map_err(|error| {
            agent_error(format!(
                "egress proxy SNI canary could not connect: {error}"
            ))
        })?;
    stream
        .set_read_timeout(Some(PROXY_CANARY_TIMEOUT))
        .map_err(|error| agent_error(format!("egress proxy SNI canary setup failed: {error}")))?;
    let connect = format!(
        "CONNECT {allowed_host}:443 HTTP/1.1\r\nHost: {allowed_host}:443\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(connect.as_bytes())
        .map_err(|error| agent_error(format!("egress proxy SNI canary write failed: {error}")))?;
    let response = read_canary_response(&mut stream)?;
    if !response.starts_with(b"HTTP/1.1 200 Connection Established\r\n") {
        return Err(agent_error(
            "egress proxy SNI canary could not open the allowlisted CONNECT target",
        ));
    }

    let mismatched_host = if allowed_host.eq_ignore_ascii_case("denied.invalid") {
        "blocked.invalid"
    } else {
        "denied.invalid"
    };
    stream
        .write_all(&tls_client_hello(mismatched_host)?)
        .map_err(|error| agent_error(format!("egress proxy SNI canary write failed: {error}")))?;
    let response = read_canary_response(&mut stream)?;
    if response.starts_with(b"HTTP/1.1 403 Forbidden\r\n") {
        Ok(())
    } else {
        Err(agent_error(
            "egress proxy canary did not reject mismatched TLS SNI",
        ))
    }
}

fn tls_client_hello(server_name: &str) -> Result<Vec<u8>, HarnessError> {
    let name = server_name.as_bytes();
    if !server_name.is_ascii() {
        return Err(agent_error("egress proxy SNI canary hostname is not ASCII"));
    }
    let name_length = u16::try_from(name.len())
        .map_err(|_| agent_error("egress proxy SNI canary hostname is too long"))?;
    let name_list_length = name_length
        .checked_add(3)
        .ok_or_else(|| agent_error("egress proxy SNI canary hostname is too long"))?;
    let sni_payload_length = name_list_length
        .checked_add(2)
        .ok_or_else(|| agent_error("egress proxy SNI canary hostname is too long"))?;
    let extensions_length = sni_payload_length
        .checked_add(4)
        .ok_or_else(|| agent_error("egress proxy SNI canary hostname is too long"))?;
    let body_length = extensions_length
        .checked_add(43)
        .ok_or_else(|| agent_error("egress proxy SNI canary ClientHello is too long"))?;
    let record_length = body_length
        .checked_add(4)
        .ok_or_else(|| agent_error("egress proxy SNI canary ClientHello is too long"))?;

    let mut hello = Vec::with_capacity(usize::from(record_length) + 5);
    hello.extend_from_slice(&[22, 3, 1]);
    hello.extend_from_slice(&record_length.to_be_bytes());
    hello.extend_from_slice(&[1, 0]);
    hello.extend_from_slice(&body_length.to_be_bytes());
    hello.extend_from_slice(&[3, 3]);
    hello.extend_from_slice(&[0; 32]);
    hello.push(0);
    hello.extend_from_slice(&[0, 2, 0x13, 0x01, 1, 0]);
    hello.extend_from_slice(&extensions_length.to_be_bytes());
    hello.extend_from_slice(&[0, 0]);
    hello.extend_from_slice(&sni_payload_length.to_be_bytes());
    hello.extend_from_slice(&name_list_length.to_be_bytes());
    hello.push(0);
    hello.extend_from_slice(&name_length.to_be_bytes());
    hello.extend_from_slice(name);
    Ok(hello)
}

fn verify_proxy_response(
    port: u16,
    request: &[u8],
    expected_status: &[u8],
    failure: &str,
) -> Result<(), HarnessError> {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let mut stream = TcpStream::connect_timeout(&address.into(), PROXY_CANARY_TIMEOUT)
        .map_err(|error| agent_error(format!("egress proxy canary could not connect: {error}")))?;
    stream
        .set_read_timeout(Some(PROXY_CANARY_TIMEOUT))
        .map_err(|error| agent_error(format!("egress proxy canary setup failed: {error}")))?;
    stream
        .write_all(request)
        .map_err(|error| agent_error(format!("egress proxy canary write failed: {error}")))?;
    let response = read_canary_response(&mut stream)?;
    if response.starts_with(expected_status) {
        Ok(())
    } else {
        Err(agent_error(format!("egress proxy canary {failure}")))
    }
}

fn read_canary_response(reader: &mut impl Read) -> Result<Vec<u8>, HarnessError> {
    let mut response = vec![0_u8; 1024];
    let mut total_read = 0;
    while total_read < response.len()
        && !response[..total_read]
            .windows(4)
            .any(|window| window == b"\r\n\r\n")
    {
        let size = reader
            .read(&mut response[total_read..])
            .map_err(|error| agent_error(format!("egress proxy canary read failed: {error}")))?;
        if size == 0 {
            break;
        }
        total_read += size;
    }
    response.truncate(total_read);
    Ok(response)
}

fn run_docker(args: &[&str]) -> Result<Output, HarnessError> {
    let owned = args
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    run_docker_owned(&owned)
}

fn run_docker_owned(args: &[String]) -> Result<Output, HarnessError> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|error| agent_error(format!("failed to invoke Docker for egress: {error}")))?;
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Docker egress command failed");
    Err(agent_error(format!("Docker egress setup failed: {detail}")))
}

fn cleanup_container(name: &str) {
    cleanup_docker_resource(&["rm", "--force", name], "container", name);
}

fn cleanup_network(name: &str) {
    cleanup_docker_resource(&["network", "rm", name], "network", name);
}

fn cleanup_docker_resource(args: &[&str], kind: &str, name: &str) {
    match Command::new("docker").args(args).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let detail = String::from_utf8_lossy(&output.stderr);
            if !detail.contains("No such container") && !detail.contains("not found") {
                tracing::error!(resource_kind = kind, resource_name = name, error = %detail.trim(), "failed to clean up egress resource");
            }
        }
        Err(error) => {
            tracing::error!(resource_kind = kind, resource_name = name, %error, "failed to invoke Docker for egress cleanup")
        }
    }
}

fn agent_error(message: impl Into<String>) -> HarnessError {
    HarnessError::AgentExecution(message.into())
}

#[cfg(test)]
mod tests {
    use super::{apply_proxy_env, read_canary_response, verify_proxy_canary};
    use std::collections::BTreeMap;
    use std::io::{self, Read, Write};
    use std::net::{Ipv4Addr, TcpListener};

    struct ChunkedReader<'a> {
        bytes: &'a [u8],
        offset: usize,
        chunk_size: usize,
    }

    impl Read for ChunkedReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let remaining = &self.bytes[self.offset..];
            let size = remaining.len().min(buffer.len()).min(self.chunk_size);
            buffer[..size].copy_from_slice(&remaining[..size]);
            self.offset += size;
            Ok(size)
        }
    }

    #[test]
    fn canary_reader_collects_a_fragmented_status_line() -> Result<(), Box<dyn std::error::Error>> {
        let mut reader = ChunkedReader {
            bytes: b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            offset: 0,
            chunk_size: 3,
        };

        let response = read_canary_response(&mut reader)?;

        assert!(response.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
        Ok(())
    }

    #[test]
    fn proxy_canary_rejects_connect_proxy_without_sni_enforcement(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let server = std::thread::spawn(move || -> io::Result<()> {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept()?;
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0_u8; 1];
                    stream.read_exact(&mut byte)?;
                    request.push(byte[0]);
                }
                if attempt == 0 {
                    stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
                } else if request.starts_with(b"GET ") {
                    stream.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")?;
                } else {
                    stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
                    let mut client_hello = [0_u8; 512];
                    if stream.read(&mut client_hello)? == 0 {
                        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                    }
                    stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
                }
            }
            Ok(())
        });

        let result = verify_proxy_canary(port, "example.com");
        server
            .join()
            .map_err(|_| io::Error::other("fake proxy thread panicked"))??;

        assert!(
            result.is_err(),
            "canary accepted a proxy that ignored TLS SNI"
        );
        Ok(())
    }

    #[test]
    fn proxy_environment_overrides_both_no_proxy_casings() {
        let mut env = BTreeMap::from([
            ("NO_PROXY".to_string(), "api.openai.com".to_string()),
            ("no_proxy".to_string(), "api.anthropic.com".to_string()),
        ]);

        apply_proxy_env(&mut env, "http://127.0.0.1:18080");

        assert_eq!(env["NO_PROXY"], "localhost,127.0.0.1");
        assert_eq!(env["no_proxy"], "localhost,127.0.0.1");
    }
}
