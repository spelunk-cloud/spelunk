// Network egress trap for local-tier CLI flows.
//
// The product's headline privacy claim is that code never leaves the local
// machine unless a team `server_url` is explicitly configured. Every
// outbound HTTP(S) call this workspace makes goes through `reqwest`
// (verified by grepping the crate for raw `std::net`/socket use: there is
// none), and no call site here disables env-based proxying
// (`Client::builder().no_proxy()` does not appear anywhere in the
// workspace), so pointing `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` at a sink
// we control, with `NO_PROXY` carving out loopback, turns every reqwest
// call site in the binary into an observable event without touching
// production code. This is pure userspace (env vars + a local HTTP
// server), so it works identically on macOS, Linux, and Windows: no
// netns/sandbox, no platform-conditional skip.
//
// Known boundary: `NO_PROXY` matching is hostname-level, not
// `host:port`-level (verified empirically: a `NO_PROXY=127.0.0.1:<port>`
// entry does not stop reqwest from proxying a request to a *different*
// `127.0.0.1:<other_port>`; both got proxied in testing, so the port
// qualifier was silently ignored). This trap therefore proves "nothing left
// the loopback interface", not "nothing reached a loopback port other than
// the sanctioned inference server". A stray request to an unintended
// loopback service is out of scope for this mechanism; each test additionally
// constrains the loopback surface to exactly the mock server(s) it starts, so
// a wrong-port bug fails the command outright (connection refused) rather
// than passing silently.

use assert_cmd::Command;
use wiremock::MockServer;

// The `Host`/CONNECT-authority header on a proxied `Request` survives even
// though `wiremock` never completes the CONNECT tunnel (it 404s the
// pseudo-request, which is enough to make `reqwest` fail the outbound call);
// see `Request::from_hyper` in wiremock 0.6, which folds the CONNECT
// authority-form target into a `host` header. That header is the only
// reliable way to name the destination for both plain HTTP and HTTPS-via-
// CONNECT requests.
fn destination(r: &wiremock::Request) -> String {
    r.headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} {}", r.method, r.url))
}

// A proxy-based egress trap: every non-loopback HTTP(S) call a wired
// `Command` makes is funneled here instead of reaching the real network.
pub struct EgressTrap {
    sink: MockServer,
}

impl EgressTrap {
    pub async fn start() -> Self {
        Self {
            sink: MockServer::start().await,
        }
    }

    // `http://` URL of the trap's sink, for callers that need to apply the
    // same proxy env vars `wire()` sets on a `Command` to the current
    // process instead (see `self_test_trap_catches_rogue_call`).
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.sink.address())
    }

    // Route everything except loopback through this trap. Sets both
    // upper- and lower-case proxy env vars since libraries disagree on
    // which case they read.
    pub fn wire(&self, cmd: &mut Command) {
        let proxy = self.proxy_url();
        for var in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            cmd.env(var, &proxy);
        }
        // Bare hostnames only (see module doc: NO_PROXY is not port-scoped
        // in practice), so every test must keep its loopback surface to
        // exactly the mock server(s) it starts.
        for var in ["NO_PROXY", "no_proxy"] {
            cmd.env(var, "127.0.0.1,localhost,::1");
        }
    }

    // Assert nothing reached the trap. Panics naming every destination seen
    // otherwise: the loud, specific failure the story requires instead of a
    // generic "test failed".
    pub async fn assert_clean(&self) {
        let seen = self.sink.received_requests().await.expect(
            "wiremock request journaling must stay enabled (default for MockServer::start())",
        );
        assert!(
            seen.is_empty(),
            "egress trap caught {} unexpected non-loopback connection attempt(s): [{}]",
            seen.len(),
            seen.iter().map(destination).collect::<Vec<_>>().join(", "),
        );
    }

    // Like `assert_clean()`, but returns the destinations instead of
    // panicking: for the self-test that proves a rogue call is actually
    // caught (a passing assertion there would prove nothing).
    pub async fn destinations_seen(&self) -> Vec<String> {
        self.sink
            .received_requests()
            .await
            .expect(
                "wiremock request journaling must stay enabled (default for MockServer::start())",
            )
            .iter()
            .map(destination)
            .collect()
    }
}

// Point loopback auto-discovery (`SPELUNK_STATE_DIR`/`server.port`, the
// same mechanism `capability::probe::probe_loopback` reads) at `url`. This
// is the "auto-discovered inference server" path, deliberately distinct
// from an explicit `server_url` (a team-server opt-in, out of scope here).
pub fn write_loopback_state(state_dir: &std::path::Path, url: &str) {
    std::fs::create_dir_all(state_dir).expect("create state dir");
    let port: u16 = url
        .rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .parse()
        .expect("uri port is numeric");
    std::fs::write(state_dir.join("server.port"), format!("{port}\n")).expect("write server.port");
}
