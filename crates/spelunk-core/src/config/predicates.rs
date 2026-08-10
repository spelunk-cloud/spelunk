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

/// Return `true` if `url` targets a loopback address (`127.0.0.0/8`, `localhost`, `::1`).
///
/// The authority is parsed, never prefix-matched: userinfo is removed, the port
/// and IPv6 brackets are stripped, and what remains must be `localhost` or an
/// address literal that the standard library parses and reports as loopback.
/// Decoration around a loopback-looking host therefore cannot smuggle a
/// different host past the check: `127.0.0.1.evil.example` and
/// `127.0.0.1@evil.example` both name `evil.example` and are not loopback.
///
/// This is a lightweight string check: no DNS resolution.
pub fn is_loopback_url(url: &str) -> bool {
    url_host(url).is_some_and(host_is_loopback)
}

/// Extract the host from `url`: scheme, userinfo, port and IPv6 brackets removed.
fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);

    // The authority ends at the first `/`, `?`, `#` or `\`. Without bounding
    // it, an `@` later in the URL would move the apparent host past the real
    // one: `http://evil.example/?@127.0.0.1`. The backslash belongs in that set
    // because a WHATWG URL parser (which is what actually opens the
    // connection) treats it as a path separator for http(s), so in
    // `http://evil.example\@127.0.0.1` the host is `evil.example`.
    let authority = match rest.find(['/', '?', '#', '\\']) {
        Some(idx) => &rest[..idx],
        None => rest,
    };

    // Userinfo is everything before the *last* `@`: in `127.0.0.1:1234@host`
    // the leading text is a credential shaped like `host:port`, not a host.
    let host_port = match authority.rfind('@') {
        Some(idx) => &authority[idx + 1..],
        None => authority,
    };

    if let Some(after_bracket) = host_port.strip_prefix('[') {
        return after_bracket.split(']').next();
    }
    // Several colons and no brackets means a bare IPv6 literal, which must not
    // be truncated at the first colon the way `host:port` is.
    if host_port.matches(':').count() > 1 {
        return Some(host_port);
    }
    host_port.split(':').next()
}

/// `true` when `host` is exactly `localhost` or an address literal in
/// `127.0.0.0/8` / `::1`.
///
/// Address literals go through the standard library's parsers, which reject the
/// non-canonical forms a hand-rolled check tends to admit: `127.999.0.1` is out
/// of range and `0127.0.0.1` has a leading zero, so neither parses, and neither
/// can ride in on a `127.` prefix.
fn host_is_loopback(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return v4.is_loopback();
    }
    host.parse::<std::net::Ipv6Addr>()
        .is_ok_and(|v6| v6.is_loopback())
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
/// Like [`is_loopback_url`], this is a lightweight check on the literal host
/// with no DNS resolution. Two distinct consequences, which are easy to
/// conflate:
///
/// * A `/etc/hosts` alias or other custom DNS entry that resolves to a loopback
///   address but isn't spelled `127.x.x.x`, `::1`, or `localhost` is **not**
///   recognised as loopback and is rejected. This is intentional (fail closed,
///   not open) and the known limitation of a string-based check.
/// * The authority is *parsed* rather than prefix-matched: userinfo is stripped
///   at the last `@`, the authority ends at the first `/`, `?`, `#` or `\`, and
///   the remaining host must parse as an exact literal. Decoration around a
///   loopback-looking host cannot smuggle a different host past the check, so
///   `http://127.0.0.1@evil.example` and `http://127.0.0.1.evil.example` are
///   rejected as the non-loopback hosts they are.
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

    #[test]
    fn is_loopback_url_rejects_host_that_merely_starts_with_a_loopback_literal() {
        assert!(!is_loopback_url("http://127.0.0.1.evil.example"));
    }

    #[test]
    fn is_loopback_url_rejects_userinfo_that_looks_like_a_loopback_host() {
        assert!(!is_loopback_url("http://127.0.0.1@evil.example"));
    }

    #[test]
    fn is_loopback_url_rejects_userinfo_shaped_like_host_and_port() {
        // The colon makes the credential look like `host:port` to a check that
        // splits on `:` before it strips userinfo.
        assert!(!is_loopback_url("http://127.0.0.1:1234@evil.example"));
    }

    #[test]
    fn is_loopback_url_accepts_real_loopback_host_carrying_userinfo() {
        // Stripping userinfo must not swing the other way and reject a
        // genuinely loopback host that happens to carry credentials.
        assert!(is_loopback_url("http://evil.example@127.0.0.1:7777"));
        assert!(is_loopback_url("http://user:pass@localhost:7777"));
    }

    #[test]
    fn is_loopback_url_splits_userinfo_on_the_last_at_sign() {
        assert!(is_loopback_url("http://a@b@127.0.0.1:7777"));
        assert!(!is_loopback_url("http://127.0.0.1@a@evil.example"));
    }

    #[test]
    fn is_loopback_url_rejects_octet_out_of_range() {
        assert!(!is_loopback_url("http://127.999.0.1"));
    }

    #[test]
    fn is_loopback_url_rejects_non_canonical_leading_zero_octet() {
        // Pinned deliberately: `0127.0.0.1` is not a canonical dotted quad, so
        // it is rejected rather than normalised to 127.0.0.1.
        assert!(!is_loopback_url("http://0127.0.0.1"));
        assert!(!is_loopback_url("http://127.00.0.1"));
    }

    #[test]
    fn is_loopback_url_rejects_at_sign_beyond_the_authority() {
        // The query and fragment are not part of the authority, so an `@`
        // inside them must not relocate the host.
        assert!(!is_loopback_url("http://evil.example/?@127.0.0.1"));
        assert!(!is_loopback_url("http://evil.example?@127.0.0.1"));
        assert!(!is_loopback_url("http://evil.example#@127.0.0.1"));
    }

    #[test]
    fn is_loopback_url_rejects_backslash_delimited_authority() {
        // A URL parser following the WHATWG rules ends the authority at a
        // backslash for http(s), so the real host here is `evil.example` and
        // everything from the backslash on is the path. Treating the backslash
        // as an ordinary character would put `127.0.0.1` after the last `@` and
        // read the whole thing as loopback.
        assert!(!is_loopback_url(r"http://evil.example\@127.0.0.1"));
        assert!(!is_loopback_url(r"http://evil.example\@127.0.0.1:7777"));
        assert!(!is_loopback_url(r"http://evil.example\\@127.0.0.1"));
    }

    #[test]
    fn is_loopback_url_accepts_expanded_ipv6_loopback() {
        // Consequence of parsing the literal instead of comparing it to the
        // string "::1": every spelling the parser calls loopback is loopback.
        assert!(is_loopback_url("http://[0:0:0:0:0:0:0:1]:7777"));
    }

    #[test]
    fn is_loopback_url_rejects_partial_loopback_literals() {
        assert!(!is_loopback_url("http://127.0.0"));
        assert!(!is_loopback_url("http://127.0.0.1.2"));
        assert!(!is_loopback_url("http://localhost.evil.example"));
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

    // A bearer travels over plaintext http only to loopback, so an authority
    // that merely looks like loopback must not clear this gate.
    #[test]
    fn validate_transport_url_rejects_spoofed_loopback_authorities() {
        for url in [
            "http://127.0.0.1.evil.example",
            "http://127.0.0.1@evil.example",
            "http://127.0.0.1:1234@evil.example",
            "http://127.0.0.1.evil.example:7777/v1/health",
            r"http://evil.example\@127.0.0.1",
        ] {
            let err = validate_transport_url(url)
                .expect_err("a host that only looks like loopback must be rejected");
            assert!(err.contains("loopback"), "{url}: error must name the fix");
            assert!(err.contains("https"), "{url}: error must name the fix");
        }
    }
}
