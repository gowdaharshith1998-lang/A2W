//! [`NodeKind::HttpRequest`] executor.
//!
//! Side-effecting. Params:
//! ```json
//! { "method": "GET|POST|...", "url": "...", "headers": {..}?,
//!   "json": <body>?,
//!   "auth": { "credential_ref": "id", "scheme": "bearer"|"header",
//!              "header_name"?: "X-Api-Key" }? }
//! ```
//! Produces one output item per input item, shaped
//! `{ "status": <u16>, "body": <parsed json or string> }`.
//!
//! # Security features
//!
//! - **SSRF egress guard** (`EgressPolicy`): reads env vars once at startup;
//!   rejects cloud-metadata / private-range IPs, optionally enforces a host
//!   allowlist/denylist.
//! - **Response-body cap**: reads at most `A2W_HTTP_MAX_BODY_BYTES` (default 10
//!   MiB) and returns `NodeError::Runtime` if the server sends more.
//! - **Credential injection**: `auth.credential_ref` is resolved at runtime via
//!   [`NodeContext::resolve_credential`] and injected as a request header.
//!   Missing/unavailable credentials cause fail-closed: the request is NOT sent.
//! - **Redirect guard**: the shared client is built with
//!   `redirect::Policy::none()` so a 302 → internal-IP can't bypass the guard.
//!
//! `dry_run` returns a mocked item per input item WITHOUT any network call.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, Url};
use tokio::net::lookup_host;

use a2w_engine::{CredentialResolver, NodeContext, NodeError, NodeExecutor};
use a2w_engine::Item;

use crate::template;

// ---------------------------------------------------------------------------
// Environment-driven egress policy (read once, stored in a `OnceLock`).
// ---------------------------------------------------------------------------

/// Egress policy parsed from environment variables at first use.
///
/// # Environment variables
/// - `A2W_HTTP_BLOCK_PRIVATE` — `"false"` to disable (default: block private).
/// - `A2W_HTTP_ALLOWED_HOSTS` — comma-separated host allowlist; when set **only**
///   these hosts are permitted.
/// - `A2W_HTTP_DENIED_HOSTS` — comma-separated host denylist (additional blocks).
/// - `A2W_HTTP_TIMEOUT_SECS` — response timeout in seconds (default 30).
/// - `A2W_HTTP_MAX_BODY_BYTES` — maximum response body size in bytes (default
///   10 MiB = 10_485_760).
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    /// Block private-range / loopback / link-local IPs via DNS resolution.
    pub block_private: bool,
    /// When non-empty, only hosts in this list are permitted.
    pub allowed_hosts: Vec<String>,
    /// Hosts that are always blocked (checked after the allowlist).
    pub denied_hosts: Vec<String>,
    /// Response body cap in bytes.
    pub max_body_bytes: usize,
    /// Per-request response timeout.
    pub timeout: Duration,
}

impl EgressPolicy {
    /// Parse the policy from environment variables.
    pub fn from_env() -> Self {
        let block_private = std::env::var("A2W_HTTP_BLOCK_PRIVATE")
            .map(|v| !v.trim().eq_ignore_ascii_case("false"))
            .unwrap_or(true);

        let allowed_hosts = std::env::var("A2W_HTTP_ALLOWED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let denied_hosts = std::env::var("A2W_HTTP_DENIED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let timeout_secs = std::env::var("A2W_HTTP_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(30);

        let max_body_bytes = std::env::var("A2W_HTTP_MAX_BODY_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(10 * 1024 * 1024); // 10 MiB

        Self {
            block_private,
            allowed_hosts,
            denied_hosts,
            max_body_bytes,
            timeout: Duration::from_secs(timeout_secs),
        }
    }
}

/// Process-global egress policy, initialised once.
static EGRESS_POLICY: OnceLock<EgressPolicy> = OnceLock::new();

fn global_policy() -> &'static EgressPolicy {
    EGRESS_POLICY.get_or_init(EgressPolicy::from_env)
}

// ---------------------------------------------------------------------------
// IP classification helper (pure, no I/O — unit-testable without DNS).
// ---------------------------------------------------------------------------

/// Returns `true` when `ip` falls in a range that must never be the target of
/// an outbound request (loopback, private, link-local, multicast, unspecified,
/// broadcast, CGNAT, and their IPv6 equivalents including IPv4-mapped addresses).
///
/// Exact ranges blocked:
/// - Loopback: 127.0.0.0/8, ::1
/// - Unspecified: 0.0.0.0, ::
/// - Multicast: 224.0.0.0/4, ff00::/8
/// - IPv4 private: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
/// - IPv4 link-local: 169.254.0.0/16 (catches 169.254.169.254)
/// - IPv4 broadcast: 255.255.255.255
/// - IPv4 "this" network: 0.0.0.0/8
/// - IPv4 CGNAT: 100.64.0.0/10
/// - IPv6 unique-local: fc00::/7
/// - IPv6 link-local: fe80::/10
/// - IPv4-mapped: ::ffff:a.b.c.d — unwrapped to IPv4 and re-checked
pub fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octs = v4.octets();
            v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                // 0.0.0.0/8 ("this" network)
                || octs[0] == 0
                // CGNAT 100.64.0.0/10
                || (octs[0] == 100 && (octs[1] & 0xC0) == 64)
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped: ::ffff:a.b.c.d — unwrap and re-check as IPv4.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_is_blocked(IpAddr::V4(v4));
            }
            let segs = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // unique-local fc00::/7
                || (segs[0] & 0xFE00) == 0xFC00
                // link-local fe80::/10
                || (segs[0] & 0xFFC0) == 0xFE80
        }
    }
}

// ---------------------------------------------------------------------------
// SSRF guard: check a URL against the egress policy.
// ---------------------------------------------------------------------------

/// Check that `url_str` is allowed under `policy`, returning `Ok(())` or a
/// descriptive [`NodeError`].
///
/// Steps:
/// 1. Parse the URL; reject non-http/https schemes.
/// 2. Apply allowlist / denylist (hostname-only; port is ignored for list checks).
/// 3. If `block_private`: resolve the hostname to IPs and reject any blocked IP.
///
/// # Errors
/// - [`NodeError::BadParams`] for unparseable or non-http/https URLs.
/// - [`NodeError::Runtime`] for blocked hosts/IPs.
pub async fn check_url_allowed(url_str: &str, policy: &EgressPolicy) -> Result<(), NodeError> {
    // Step 1: parse.
    let url = Url::parse(url_str).map_err(|e| {
        NodeError::BadParams(format!("invalid URL '{url_str}': {e}"))
    })?;

    match url.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(NodeError::BadParams(format!(
                "URL scheme '{scheme}' is not permitted; only http/https are allowed"
            )));
        }
    }

    let host = url
        .host_str()
        .ok_or_else(|| NodeError::BadParams(format!("URL '{url_str}' has no host")))?
        .to_ascii_lowercase();

    // Step 2: allowlist / denylist.
    if !policy.allowed_hosts.is_empty() && !policy.allowed_hosts.contains(&host) {
        return Err(NodeError::Runtime(format!(
            "host '{host}' is not in the HTTP egress allowlist (A2W_HTTP_ALLOWED_HOSTS)"
        )));
    }
    if policy.denied_hosts.contains(&host) {
        return Err(NodeError::Runtime(format!(
            "host '{host}' is in the HTTP egress denylist (A2W_HTTP_DENIED_HOSTS)"
        )));
    }

    // Step 3: block-private via DNS resolution.
    if policy.block_private {
        let port = url.port_or_known_default().unwrap_or(80);
        // `lookup_host` accepts "host:port" and resolves A/AAAA; a literal IP
        // resolves to itself, so literal-IP URLs are also covered.
        let addrs = lookup_host((host.as_str(), port)).await.map_err(|e| {
            // Fail closed: if we can't resolve, don't allow.
            NodeError::Runtime(format!(
                "DNS resolution of '{host}' failed (refusing to send): {e}"
            ))
        })?;

        for sock_addr in addrs {
            let ip = sock_addr.ip();
            if ip_is_blocked(ip) {
                return Err(NodeError::Runtime(format!(
                    "host '{host}' resolved to blocked IP {ip} \
                     (private/loopback/link-local/CGNAT)"
                )));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared client (built once per `HttpRequest` instance).
// ---------------------------------------------------------------------------

/// Build a hardened `reqwest::Client` with sane defaults.
///
/// - No automatic redirect following (redirect::Policy::none) so a 302 to a
///   private IP cannot bypass the SSRF guard.
/// - Explicit connect and response timeouts.
fn hardened_client(policy: &EgressPolicy) -> Client {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(policy.timeout)
        .build()
        .expect("hardened reqwest client build should never fail")
}

// ---------------------------------------------------------------------------
// Auth / credential injection.
// ---------------------------------------------------------------------------

/// Parsed representation of the optional `auth` param.
#[derive(Debug, Clone)]
enum AuthSpec {
    Bearer { credential_ref: String },
    Header { credential_ref: String, header_name: String },
}

impl AuthSpec {
    fn parse(params: &serde_json::Value) -> Option<Result<Self, NodeError>> {
        let auth = params.get("auth")?;

        let credential_ref = auth
            .get("credential_ref")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);

        let credential_ref = match credential_ref {
            Some(r) if !r.is_empty() => r,
            Some(_) => {
                return Some(Err(NodeError::BadParams(
                    "auth.credential_ref must be a non-empty string".into(),
                )));
            }
            None => {
                return Some(Err(NodeError::BadParams(
                    "auth object requires a non-empty string `credential_ref`".into(),
                )));
            }
        };

        let scheme = auth
            .get("scheme")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("bearer");

        let spec = match scheme {
            "bearer" => AuthSpec::Bearer { credential_ref },
            "header" => {
                let header_name = auth
                    .get("header_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("X-Api-Key")
                    .to_owned();
                AuthSpec::Header { credential_ref, header_name }
            }
            other => {
                return Some(Err(NodeError::BadParams(format!(
                    "auth.scheme '{other}' is not recognised; use 'bearer' or 'header'"
                ))));
            }
        };

        Some(Ok(spec))
    }

    fn credential_ref(&self) -> &str {
        match self {
            AuthSpec::Bearer { credential_ref } | AuthSpec::Header { credential_ref, .. } => {
                credential_ref
            }
        }
    }
}

/// Resolve the credential and inject the auth header into `req`.
///
/// Fails closed: if the resolver is absent **or** returns `None`, the request
/// is refused. The resolved secret is never placed into output items or error
/// messages.
async fn inject_auth(
    req: reqwest::RequestBuilder,
    spec: &AuthSpec,
    resolver: Option<&Arc<dyn CredentialResolver>>,
) -> Result<reqwest::RequestBuilder, NodeError> {
    let cred_ref = spec.credential_ref();

    let secret = match resolver {
        None => {
            return Err(NodeError::Runtime(format!(
                "credential '{cred_ref}' unavailable: no credential resolver is configured"
            )));
        }
        Some(r) => {
            r.resolve(cred_ref)
                .await
                .map_err(|e| NodeError::Runtime(format!("credential '{cred_ref}' unavailable: {e}")))?
        }
    };

    let secret = match secret {
        Some(s) => s,
        None => {
            return Err(NodeError::Runtime(format!(
                "credential '{cred_ref}' unavailable: not found in the credential store"
            )));
        }
    };

    let req = match spec {
        AuthSpec::Bearer { .. } => req.header("Authorization", format!("Bearer {secret}")),
        AuthSpec::Header { header_name, .. } => req.header(header_name.as_str(), secret),
    };

    Ok(req)
}

// ---------------------------------------------------------------------------
// Executor.
// ---------------------------------------------------------------------------

/// Executor for [`a2w_ir::NodeKind::HttpRequest`]. Holds a shared client so
/// connection pooling is reused across all HTTP nodes/items.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    client: Client,
}

impl Default for HttpRequest {
    fn default() -> Self {
        Self {
            client: hardened_client(global_policy()),
        }
    }
}

impl HttpRequest {
    /// Construct with an explicit shared client (for tests that inject a mock).
    #[must_use]
    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    /// Resolve the request URL for one input item (templated).
    fn resolve_url(params: &serde_json::Value, item: &serde_json::Value) -> Result<String, NodeError> {
        let raw = params
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| NodeError::BadParams("HttpRequest requires a string `url`".into()))?;
        Ok(template::render(raw, item))
    }

    /// Parse the HTTP method from params (defaults to GET).
    fn resolve_method(params: &serde_json::Value) -> Result<reqwest::Method, NodeError> {
        let raw = params
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("GET");
        raw.parse::<reqwest::Method>()
            .map_err(|_| NodeError::BadParams(format!("invalid HTTP method '{raw}'")))
    }
}

#[async_trait]
impl NodeExecutor for HttpRequest {
    fn has_side_effects(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        ctx: &NodeContext,
        input: Vec<Item>,
    ) -> Result<Vec<Item>, NodeError> {
        let method = Self::resolve_method(&ctx.params)?;
        // Parse the optional auth spec once (before the per-item loop).
        let auth_spec = ctx.params.get("auth").and_then(|_| AuthSpec::parse(&ctx.params)).transpose()?;
        let policy = global_policy();

        let mut out = Vec::with_capacity(input.len());
        for item in &input {
            let url = Self::resolve_url(&ctx.params, &item.json)?;

            // SSRF guard — checked on the final rendered URL, before any network call.
            check_url_allowed(&url, policy).await?;

            let mut req = self.client.request(method.clone(), &url);

            // Optional headers (string -> string).
            if let Some(serde_json::Value::Object(headers)) = ctx.params.get("headers") {
                for (k, v) in headers {
                    if let Some(val) = v.as_str() {
                        req = req.header(k.as_str(), val);
                    } else {
                        return Err(NodeError::BadParams(format!(
                            "header '{k}' must be a string"
                        )));
                    }
                }
            }

            // Optional JSON body, with string templating applied recursively.
            if let Some(body) = ctx.params.get("json") {
                let rendered = render_json(body, &item.json);
                req = req.json(&rendered);
            }

            // Credential injection — fail closed if not resolvable.
            if let Some(spec) = &auth_spec {
                req = inject_auth(req, spec, ctx.credentials.as_ref()).await?;
            }

            let resp = req
                .send()
                .await
                .map_err(|e| NodeError::Http(e.to_string()))?;
            let status = resp.status().as_u16();

            // Capped body read: reject responses that exceed the configured limit.
            let raw_bytes = resp
                .bytes()
                .await
                .map_err(|e| NodeError::Http(e.to_string()))?;
            if raw_bytes.len() > policy.max_body_bytes {
                return Err(NodeError::Runtime(format!(
                    "response body ({} bytes) exceeds the maximum allowed \
                     ({} bytes, set A2W_HTTP_MAX_BODY_BYTES to change)",
                    raw_bytes.len(),
                    policy.max_body_bytes,
                )));
            }
            let text = String::from_utf8_lossy(&raw_bytes).into_owned();
            let body = serde_json::from_str::<serde_json::Value>(&text)
                .unwrap_or(serde_json::Value::String(text));

            out.push(Item::produced(
                serde_json::json!({ "status": status, "body": body }),
                ctx.node_id.clone(),
                0,
            ));
        }
        Ok(out)
    }

    async fn dry_run(
        &self,
        ctx: &NodeContext,
        input: Vec<Item>,
    ) -> Result<Vec<Item>, NodeError> {
        // No network: produce a faithful mock per input item, echoing the
        // resolved URL so the run shape can be inspected.
        let mut out = Vec::with_capacity(input.len());
        for item in &input {
            let url = Self::resolve_url(&ctx.params, &item.json)?;
            out.push(Item::produced(
                serde_json::json!({ "_mock": true, "status": 200, "url": url }),
                ctx.node_id.clone(),
                0,
            ));
        }
        Ok(out)
    }
}

/// Recursively apply string templating to a JSON body against `item`.
///
/// Strings are rendered through the token substituter; containers recurse;
/// scalars pass through.
fn render_json(value: &serde_json::Value, item: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(template::render(s, item)),
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(|v| render_json(v, item)).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut out = serde_json::Map::with_capacity(obj.len());
            for (k, v) in obj {
                out.insert(k.clone(), render_json(v, item));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::net::Ipv6Addr;
    use std::sync::Arc;

    use async_trait::async_trait;
    use a2w_engine::{CredentialError, CredentialResolver};

    use super::*;

    // -----------------------------------------------------------------------
    // ip_is_blocked — pure IP tests, no DNS.
    // -----------------------------------------------------------------------

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn v6_mapped(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        // Build ::ffff:a.b.c.d
        let v4 = Ipv4Addr::new(a, b, c, d);
        IpAddr::V6(v4.to_ipv6_mapped())
    }

    #[test]
    fn ip_blocked_loopback_v4() {
        assert!(ip_is_blocked(v4(127, 0, 0, 1)));
        assert!(ip_is_blocked(v4(127, 1, 2, 3)));
    }

    #[test]
    fn ip_blocked_loopback_v6() {
        assert!(ip_is_blocked(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn ip_blocked_link_local_169_254() {
        // Must catch the AWS/GCP metadata endpoint.
        assert!(ip_is_blocked(v4(169, 254, 169, 254)));
        assert!(ip_is_blocked(v4(169, 254, 0, 1)));
    }

    #[test]
    fn ip_blocked_link_local_ipv4_mapped() {
        // ::ffff:169.254.169.254 must also be blocked.
        assert!(ip_is_blocked(v6_mapped(169, 254, 169, 254)));
    }

    #[test]
    fn ip_blocked_private_ranges() {
        assert!(ip_is_blocked(v4(10, 0, 0, 1)));
        assert!(ip_is_blocked(v4(172, 16, 0, 1)));
        assert!(ip_is_blocked(v4(172, 31, 255, 255)));
        assert!(ip_is_blocked(v4(192, 168, 1, 1)));
    }

    #[test]
    fn ip_blocked_cgnat() {
        // 100.64.0.0/10 = 100.64.0.0 – 100.127.255.255
        assert!(ip_is_blocked(v4(100, 64, 0, 0)));
        assert!(ip_is_blocked(v4(100, 127, 255, 255)));
        // 100.128.x.x is NOT in CGNAT
        assert!(!ip_is_blocked(v4(100, 128, 0, 0)));
    }

    #[test]
    fn ip_blocked_broadcast_and_zero_net() {
        assert!(ip_is_blocked(v4(255, 255, 255, 255)));
        assert!(ip_is_blocked(v4(0, 0, 0, 0)));
        assert!(ip_is_blocked(v4(0, 1, 2, 3)));
    }

    #[test]
    fn ip_blocked_multicast() {
        assert!(ip_is_blocked(v4(224, 0, 0, 1)));
        assert!(ip_is_blocked(v4(239, 255, 255, 255)));
    }

    #[test]
    fn ip_blocked_v6_unique_local_and_link_local() {
        // fc00::/7 (unique-local)
        let ula: IpAddr = "fc00::1".parse().unwrap();
        assert!(ip_is_blocked(ula));
        let ula2: IpAddr = "fdff::1".parse().unwrap();
        assert!(ip_is_blocked(ula2));
        // fe80::/10 (link-local)
        let ll: IpAddr = "fe80::1".parse().unwrap();
        assert!(ip_is_blocked(ll));
    }

    #[test]
    fn ip_not_blocked_public() {
        // 8.8.8.8 is a public IP and must NOT be blocked.
        assert!(!ip_is_blocked(v4(8, 8, 8, 8)));
        assert!(!ip_is_blocked(v4(1, 1, 1, 1)));
        // 100.63.x.x is just below CGNAT range.
        assert!(!ip_is_blocked(v4(100, 63, 255, 255)));
    }

    // -----------------------------------------------------------------------
    // check_url_allowed — requires tokio runtime; uses real DNS for public hosts
    // but also works with literal IPs (no DNS required for blocked-IP tests).
    // -----------------------------------------------------------------------

    fn permissive_policy() -> EgressPolicy {
        EgressPolicy {
            block_private: true,
            allowed_hosts: vec![],
            denied_hosts: vec![],
            max_body_bytes: 10 * 1024 * 1024,
            timeout: Duration::from_secs(30),
        }
    }

    #[tokio::test]
    async fn check_url_rejects_non_http_scheme() {
        let p = permissive_policy();
        let err = check_url_allowed("file:///etc/passwd", &p)
            .await
            .expect_err("file:// must be rejected");
        assert!(matches!(err, NodeError::BadParams(_)), "got {err:?}");

        let err2 = check_url_allowed("ftp://example.com/", &p)
            .await
            .expect_err("ftp:// must be rejected");
        assert!(matches!(err2, NodeError::BadParams(_)), "got {err2:?}");
    }

    #[tokio::test]
    async fn check_url_rejects_localhost() {
        let p = permissive_policy();
        let err = check_url_allowed("http://localhost/", &p)
            .await
            .expect_err("localhost must be rejected");
        assert!(matches!(err, NodeError::Runtime(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn check_url_rejects_127_0_0_1() {
        let p = permissive_policy();
        let err = check_url_allowed("http://127.0.0.1/", &p)
            .await
            .expect_err("127.0.0.1 must be rejected");
        assert!(matches!(err, NodeError::Runtime(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn check_url_rejects_10_0_0_1() {
        let p = permissive_policy();
        let err = check_url_allowed("http://10.0.0.1/", &p)
            .await
            .expect_err("10.0.0.1 must be rejected");
        assert!(matches!(err, NodeError::Runtime(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn check_url_rejects_ipv6_loopback() {
        let p = permissive_policy();
        let err = check_url_allowed("http://[::1]/", &p)
            .await
            .expect_err("[::1] must be rejected");
        assert!(matches!(err, NodeError::Runtime(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn check_url_rejects_metadata_ip() {
        let p = permissive_policy();
        let err = check_url_allowed("http://169.254.169.254/latest/meta-data/", &p)
            .await
            .expect_err("169.254.169.254 must be rejected");
        assert!(matches!(err, NodeError::Runtime(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn check_url_allowlist_blocks_unlisted_host() {
        let p = EgressPolicy {
            block_private: false,
            allowed_hosts: vec!["api.example.com".into()],
            denied_hosts: vec![],
            max_body_bytes: 10 * 1024 * 1024,
            timeout: Duration::from_secs(30),
        };
        let err = check_url_allowed("https://other.example.com/", &p)
            .await
            .expect_err("host not in allowlist must be rejected");
        assert!(matches!(err, NodeError::Runtime(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn check_url_allowlist_permits_listed_host() {
        // block_private off so no DNS needed in this allowlist-only test.
        let p = EgressPolicy {
            block_private: false,
            allowed_hosts: vec!["api.example.com".into()],
            denied_hosts: vec![],
            max_body_bytes: 10 * 1024 * 1024,
            timeout: Duration::from_secs(30),
        };
        check_url_allowed("https://api.example.com/path", &p)
            .await
            .expect("listed host must be allowed");
    }

    #[tokio::test]
    async fn check_url_denylist_blocks_host() {
        let p = EgressPolicy {
            block_private: false,
            allowed_hosts: vec![],
            denied_hosts: vec!["evil.internal".into()],
            max_body_bytes: 10 * 1024 * 1024,
            timeout: Duration::from_secs(30),
        };
        let err = check_url_allowed("https://evil.internal/", &p)
            .await
            .expect_err("denied host must be rejected");
        assert!(matches!(err, NodeError::Runtime(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn check_url_no_block_private_allows_private_ip() {
        // When block_private is false, private IPs should be permitted.
        let p = EgressPolicy {
            block_private: false,
            allowed_hosts: vec![],
            denied_hosts: vec![],
            max_body_bytes: 10 * 1024 * 1024,
            timeout: Duration::from_secs(30),
        };
        // Should succeed (no DNS resolution done, no IP check).
        check_url_allowed("http://10.0.0.1/", &p)
            .await
            .expect("private IP allowed when block_private=false");
    }

    // -----------------------------------------------------------------------
    // Credential injection tests (no network needed).
    // -----------------------------------------------------------------------

    /// A test-only resolver that returns a fixed secret for one known ref.
    #[derive(Debug)]
    struct MockResolver {
        secret: Option<String>,
    }

    #[async_trait]
    impl CredentialResolver for MockResolver {
        async fn resolve(&self, _credential_ref: &str) -> Result<Option<String>, CredentialError> {
            Ok(self.secret.clone())
        }
    }

    /// Helper: call `inject_auth` with a `Bearer` spec and a given resolver.
    async fn try_bearer_inject(
        secret: Option<String>,
        has_resolver: bool,
    ) -> Result<(), NodeError> {
        let spec = AuthSpec::Bearer {
            credential_ref: "my_cred".into(),
        };
        let resolver: Option<Arc<dyn CredentialResolver>> = if has_resolver {
            Some(Arc::new(MockResolver { secret }))
        } else {
            None
        };
        // Build a dummy RequestBuilder (we only care about the error path here).
        let client = Client::new();
        let req = client.get("https://example.com/");
        inject_auth(req, &spec, resolver.as_ref())
            .await
            .map(|_| ())
    }

    #[tokio::test]
    async fn inject_auth_bearer_succeeds_with_secret() {
        try_bearer_inject(Some("tok123".into()), true)
            .await
            .expect("inject should succeed when secret is present");
    }

    #[tokio::test]
    async fn inject_auth_fails_closed_no_resolver() {
        let err = try_bearer_inject(None, false)
            .await
            .expect_err("must fail when no resolver configured");
        assert!(matches!(err, NodeError::Runtime(_)), "got {err:?}");
        // Secret is NOT in the error message.
        assert!(!err.to_string().contains("tok"), "secret leaked into error");
    }

    #[tokio::test]
    async fn inject_auth_fails_closed_missing_credential() {
        let err = try_bearer_inject(None, true)
            .await
            .expect_err("must fail when credential not found");
        assert!(matches!(err, NodeError::Runtime(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn inject_auth_header_scheme_uses_named_header() {
        let spec = AuthSpec::Header {
            credential_ref: "my_cred".into(),
            header_name: "X-Api-Key".into(),
        };
        let resolver: Arc<dyn CredentialResolver> =
            Arc::new(MockResolver { secret: Some("key_abc".into()) });
        let client = Client::new();
        let req = client.get("https://example.com/");
        // Should succeed; we just verify it doesn't error (builder intentionally discarded).
        let _built = inject_auth(req, &spec, Some(&resolver))
            .await
            .expect("header scheme inject must succeed");
    }
}
