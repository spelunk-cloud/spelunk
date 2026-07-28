/// Returns `true` when `SPELUNK_NO_SERVER` is set to a truthy value.
///
/// This is the hard offline kill-switch shared by [`Config::resolve_mode`] and
/// the CLI capability probe; both must agree on what "no server" means.
pub fn no_server_env_set() -> bool {
    matches!(
        std::env::var("SPELUNK_NO_SERVER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Return `true` if `url` targets a loopback address (`127.x.x.x`, `localhost`, `::1`).
///
/// This is a lightweight string check — no DNS resolution.
pub fn is_loopback_url(url: &str) -> bool {
    // Strip scheme and authority prefix up to the host.
    let host_part = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");

    // Extract the host (before any path or port).
    let host = if let Some(idx) = host_part.find('/') {
        &host_part[..idx]
    } else {
        host_part
    };
    // Drop port if present (handle IPv6 bracketed form too).
    let host = if host.starts_with('[') {
        // IPv6: [::1]:port or [::1]
        host.trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or(host)
    } else {
        host.split(':').next().unwrap_or(host)
    };

    matches!(host, "localhost" | "127.0.0.1" | "::1") || host.starts_with("127.")
}

/// Return `true` when `url` targets a loopback host (see [`is_loopback_url`])
/// but names no explicit port.
///
/// A loopback `server_url` with no port can never be the auto-discovered
/// local daemon (which always binds a specific port, default `7777`): it is a
/// near-certain leftover misconfiguration (e.g. a stale `server_url` after a
/// team-server value was cleared down to a bare host). Callers use this to
/// warn, not reject: unlike [`validate_transport_url`], a portless loopback
/// URL is not a security problem, so it's a warning rather than a hard error.
pub fn is_loopback_url_missing_port(url: &str) -> bool {
    if !is_loopback_url(url) {
        return false;
    }
    let host_part = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host = host_part.split('/').next().unwrap_or(host_part);
    match host.strip_prefix('[') {
        // IPv6: `[::1]` (no port) vs `[::1]:7777` (port present after `]:`).
        Some(_) => !host.contains("]:"),
        None => !host.contains(':'),
    }
}

/// Validate that `url` is an acceptable transport for sending a bearer token /
/// talking to a spelunk-server: either `https://` (any host), or `http://` to a
/// loopback host (`127.0.0.1`, `::1`, `localhost`).
///
/// A non-loopback `http://` URL is invalid config — plaintext HTTP outside the
/// loopback interface would send the bearer token (and query content) in the
/// clear. There is no opt-out env var: the fix is always "use https, or
/// loopback".
///
/// Like [`is_loopback_url`], this is a lightweight string check on the literal
/// host — there is no DNS resolution. A `/etc/hosts` alias or other custom DNS
/// entry that resolves to a loopback address but isn't spelled `127.x.x.x`,
/// `::1`, or `localhost` is **not** recognised as loopback and is rejected;
/// this is intentional (fail closed, not open) and the known limitation of a
/// string-based check.
///
/// Returns `Ok(())` for a valid URL, or a one-line `Err` naming the fix.
pub fn validate_transport_url(url: &str) -> Result<(), String> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if url.starts_with("http://") {
        if is_loopback_url(url) {
            return Ok(());
        }
        return Err(format!(
            "invalid server URL {url:?}: plaintext http:// is only allowed to a loopback \
             address (127.0.0.1/::1/localhost); use https:// for any other host"
        ));
    }
    Err(format!(
        "invalid server URL {url:?}: expected an http:// or https:// URL"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_loopback_url ──────────────────────────────────────────────────────

    #[test]
    fn is_loopback_url_recognises_127_0_0_1() {
        assert!(is_loopback_url("http://127.0.0.1:7777"));
        assert!(is_loopback_url("http://127.0.0.1:7777/"));
        assert!(is_loopback_url("http://127.0.0.1"));
    }

    #[test]
    fn is_loopback_url_recognises_localhost() {
        assert!(is_loopback_url("http://localhost:7777"));
        assert!(is_loopback_url("http://localhost"));
    }

    #[test]
    fn is_loopback_url_recognises_ipv6_loopback() {
        assert!(is_loopback_url("http://[::1]:7777"));
        assert!(is_loopback_url("http://[::1]"));
    }

    #[test]
    fn is_loopback_url_recognises_127_subnet() {
        assert!(is_loopback_url("http://127.1.2.3:7777"));
    }

    #[test]
    fn is_loopback_url_rejects_non_loopback() {
        assert!(!is_loopback_url("http://spelunk.internal:7777"));
        assert!(!is_loopback_url("http://192.168.1.100:7777"));
        assert!(!is_loopback_url("https://example.com"));
        assert!(!is_loopback_url("http://10.0.0.1"));
    }

    #[test]
    fn is_loopback_url_rejects_address_with_127_in_path() {
        // Should NOT match just because "127" appears somewhere
        assert!(!is_loopback_url("http://example.com/proxy/127.0.0.1"));
    }

    // ── is_loopback_url_missing_port ─────────────────────────────────────────

    #[test]
    fn is_loopback_url_missing_port_flags_bare_localhost() {
        // The exact field-observed misconfig: a stale `server_url =
        // "http://localhost"` with no port, which can never be the
        // auto-discovered daemon (default port 7777).
        assert!(is_loopback_url_missing_port("http://localhost"));
        assert!(is_loopback_url_missing_port("https://localhost"));
        assert!(is_loopback_url_missing_port("http://localhost/"));
        assert!(is_loopback_url_missing_port("http://127.0.0.1"));
        assert!(is_loopback_url_missing_port("http://[::1]"));
        assert!(is_loopback_url_missing_port("http://[::1]/"));
    }

    #[test]
    fn is_loopback_url_missing_port_accepts_when_port_present() {
        assert!(!is_loopback_url_missing_port("http://localhost:7777"));
        assert!(!is_loopback_url_missing_port("http://127.0.0.1:7777"));
        assert!(!is_loopback_url_missing_port("http://[::1]:7777"));
    }

    #[test]
    fn is_loopback_url_missing_port_ignores_non_loopback_hosts() {
        // A non-loopback host without a port is a normal https:// URL
        // (default port 443), not a misconfiguration signal.
        assert!(!is_loopback_url_missing_port("https://example.com"));
        assert!(!is_loopback_url_missing_port("http://team-server:7777"));
    }

    // ── validate_transport_url (loopback-only plaintext http) ──────────────────

    #[test]
    fn validate_transport_url_rejects_non_loopback_http() {
        let err = validate_transport_url("http://team-server:7777")
            .expect_err("non-loopback http:// must be rejected");
        assert!(err.contains("http://team-server:7777"));
        assert!(err.contains("https"));
        assert!(err.contains("loopback"));
    }

    #[test]
    fn validate_transport_url_rejects_non_loopback_ip_http() {
        assert!(validate_transport_url("http://192.168.1.100:7777").is_err());
        assert!(validate_transport_url("http://10.0.0.1:7777").is_err());
    }

    #[test]
    fn validate_transport_url_accepts_loopback_http() {
        assert!(validate_transport_url("http://127.0.0.1:7777").is_ok());
        assert!(validate_transport_url("http://localhost:7777").is_ok());
        assert!(validate_transport_url("http://[::1]:7777").is_ok());
        assert!(validate_transport_url("http://127.5.6.7:7777").is_ok());
    }

    #[test]
    fn validate_transport_url_accepts_any_https() {
        assert!(validate_transport_url("https://team-server:7777").is_ok());
        assert!(validate_transport_url("https://example.com").is_ok());
        assert!(validate_transport_url("https://127.0.0.1:7777").is_ok());
    }

    #[test]
    fn validate_transport_url_rejects_unknown_scheme() {
        let err = validate_transport_url("ftp://team-server:7777").unwrap_err();
        assert!(err.contains("http"));
    }

    /// IPv6 non-loopback addresses must be rejected too, not just the IPv4 case
    /// — the check is symmetric across address families.
    #[test]
    fn validate_transport_url_rejects_non_loopback_ipv6_http() {
        assert!(validate_transport_url("http://[2001:db8::1]:7777").is_err());
        assert!(validate_transport_url("http://[fe80::1]:7777").is_err());
    }

    /// Known limitation, asserted so it can't silently regress into a security
    /// hole: this is a *string* check with no DNS resolution. A hostname alias
    /// (e.g. an `/etc/hosts` entry) that an OS resolver would send to
    /// 127.0.0.1 is NOT recognised as loopback here and is correctly rejected
    /// (fail closed) rather than accepted on the assumption it "means"
    /// loopback. If a caller ever needs alias support, that requires an
    /// explicit, reviewed allow-list — not a silent DNS lookup at validation
    /// time (which would also make validation do network I/O).
    #[test]
    fn validate_transport_url_rejects_hostname_alias_even_if_it_would_resolve_to_loopback() {
        let err = validate_transport_url("http://my-loopback-alias:7777")
            .expect_err("a non-literal loopback hostname must be rejected, not DNS-resolved");
        assert!(err.contains("loopback"));
    }

    /// `localhost` with an explicit port-less bare form and a trailing slash
    /// path both still resolve to the same host extraction as the bracketed
    /// IPv6 case — pin the unbracketed `::1` (no port) form too, since the
    /// bracket-stripping logic in `is_loopback_url` is easy to regress.
    #[test]
    fn validate_transport_url_accepts_bare_ipv6_loopback_no_port() {
        assert!(validate_transport_url("http://[::1]/").is_ok());
    }
}
