use anyhow::{Context, Result};
use std::path::Path;

/// Apply the configured custom CA bundle to a reqwest client builder.
///
/// `ca_path` is the resolved [`Config::server_ca`] (env `SPELUNK_SERVER_CA`
/// precedence is already applied at load time). Adds every certificate in the
/// PEM bundle as a trust anchor **on top of** the built-in roots; certificate
/// verification stays on. A `None` path is a no-op, so every
/// team-server client site can route through this unconditionally.
pub fn apply_server_ca(
    builder: reqwest::ClientBuilder,
    ca_path: Option<&Path>,
) -> Result<reqwest::ClientBuilder> {
    let Some(path) = ca_path else {
        return Ok(builder);
    };
    let pem = std::fs::read(path)
        .with_context(|| format!("reading SPELUNK_SERVER_CA bundle at {}", path.display()))?;
    let certs = reqwest::Certificate::from_pem_bundle(&pem)
        .with_context(|| format!("parsing PEM CA bundle at {}", path.display()))?;
    // `from_pem_bundle` yields an empty vec for a file with no PEM blocks rather
    // than erroring — surface that as a config error, else a wrong path would
    // silently add no trust anchor and fail TLS with a confusing message.
    if certs.is_empty() {
        anyhow::bail!(
            "no PEM certificates found in CA bundle at {}",
            path.display()
        );
    }
    let mut builder = builder;
    for cert in certs {
        builder = builder.add_root_certificate(cert);
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── Custom CA trust (SPELUNK_SERVER_CA / config `server_ca`) ─────────────

    /// A throwaway self-signed CA used only to prove the PEM is parsed and
    /// accepted as a trust anchor. Not trusted by anything real.
    const TEST_CA_PEM: &[u8] = b"-----BEGIN CERTIFICATE-----\n\
MIIDFTCCAf2gAwIBAgIUdz5ZLoL+3T+MwWN0dJjElxlwsRwwDQYJKoZIhvcNAQEL\n\
BQAwGjEYMBYGA1UEAwwPc3BlbHVuay10ZXN0LWNhMB4XDTI2MDcxMzE3MjkyMFoX\n\
DTM2MDcxMDE3MjkyMFowGjEYMBYGA1UEAwwPc3BlbHVuay10ZXN0LWNhMIIBIjAN\n\
BgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAz/BAzTJJbgWUWnUqV0qFJHT+TIDT\n\
WQJbIRVBb9MezLblAGun2RG22U47jubOKoSa4DrenrEJIafd74IR9aLUdcRp6lyN\n\
WsuzY6P26ntZ1epHUjYeBgqpu71v3FK2pBvQ9PP//AhQN7apE6V4UocKd7OxbSk7\n\
g1bZSYSXoFQtSZzV9KCWNpuqUMNdaMIoy1EYY86t55jeDdpFRkiO3W5jZ6M37ekg\n\
mDq5wIOC1QHziDLWFkpBbuOxsN/admbwbsDH5301H3P25RBY12Guqsz4/lgsEuN9\n\
L+RJfs/Vdmen5wKhbPDkr8EYx7hLF0T2ZKOf0TrJojrqHkO5n4+7ESeaUwIDAQAB\n\
o1MwUTAdBgNVHQ4EFgQUJsLeVcwx4exuV//vdoLfqb5H3ZQwHwYDVR0jBBgwFoAU\n\
JsLeVcwx4exuV//vdoLfqb5H3ZQwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0B\n\
AQsFAAOCAQEAT5lW043iyZlbYM0372z/Ec8Z3VYDZ3bvryKN+6kGYuZJJnCep2c/\n\
QX2iPx+HRWx0rz+QcnNrOdetr2KAac6ODxU2LVzjehac5wUVWm6uICzojjy84Ztn\n\
1t5Ori6kvPSbOxJbznQuC7FILxpZswOBh6qfOHNgKeGVK4OkG2069YiFI+kwMdkI\n\
d9qQF0w9nfELOC5M+ZxwP4vE/QkXLG57ZrOvKl2V4pthKSBv3LBAnh/C7X7/KC+f\n\
iwNpumIaYRGylEbxW2WVv9YsWDmTBFqEkgrmx1QPJr3FtA6eeWmZ+EJIr3ImOv/d\n\
CPBfHwWj/FUeFj+csF5QpOj+u/D1F1Kh5w==\n\
-----END CERTIFICATE-----\n";

    #[test]
    fn apply_server_ca_none_is_noop() {
        // No path → builder unchanged and still buildable.
        let client = apply_server_ca(reqwest::Client::builder(), None)
            .unwrap()
            .build();
        assert!(client.is_ok());
    }

    #[test]
    fn apply_server_ca_adds_valid_bundle() {
        let tmp = TempDir::new().unwrap();
        let ca = tmp.path().join("ca.pem");
        std::fs::write(&ca, TEST_CA_PEM).unwrap();
        // A valid PEM bundle must parse and be accepted as a trust anchor; the
        // client (verification still on) builds successfully.
        let client = apply_server_ca(reqwest::Client::builder(), Some(&ca))
            .expect("valid CA bundle should be accepted")
            .build();
        assert!(client.is_ok());
    }

    #[test]
    fn apply_server_ca_missing_file_errors() {
        let missing = Path::new("/nonexistent/spelunk-server-ca.pem");
        let err = apply_server_ca(reqwest::Client::builder(), Some(missing)).unwrap_err();
        assert!(err.to_string().contains("SPELUNK_SERVER_CA"), "got: {err}");
    }

    #[test]
    fn apply_server_ca_malformed_pem_errors() {
        let tmp = TempDir::new().unwrap();
        let ca = tmp.path().join("bad.pem");
        std::fs::write(&ca, b"not a certificate").unwrap();
        assert!(apply_server_ca(reqwest::Client::builder(), Some(&ca)).is_err());
    }
}
