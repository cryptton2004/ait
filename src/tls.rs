//! TLS for the LAN mode (rustls, ring backend, TLS 1.3 only).
//!
//! The host reads a self-signed certificate and key from PEM files (made by
//! `scripts/make-cert.*`; ait never creates certificates). Clients trust no
//! certificate authority: they accept exactly one certificate, pinned by its
//! SHA-256 fingerprint, and still check the TLS 1.3 handshake signature with
//! that certificate's key.

use crate::error::{AitError, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// SHA-256 (ring's, through the TLS 1.3 cipher suite's hash).
pub fn sha256(data: &[u8]) -> Result<Vec<u8>> {
    let suite = rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256
        .tls13()
        .ok_or_else(|| AitError::new("INTERNAL", "SHA-256 is not available"))?;
    Ok(suite.common.hash_provider.hash(data).as_ref().to_vec())
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `n` bytes from the operating system's secure random source, as hex.
pub fn random_hex(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut buf)
        .map_err(|_| AitError::new("INTERNAL", "no secure random source"))?;
    Ok(hex(&buf))
}

/// `sha256:<64 lowercase hex>` of a certificate (DER bytes).
pub fn fingerprint(der: &[u8]) -> Result<String> {
    Ok(format!("sha256:{}", hex(&sha256(der)?)))
}

/// Accepts `sha256:<hex>`, plain hex, any case, with or without `:`
/// between bytes. Returns the 64 lowercase hex characters.
pub fn parse_fingerprint(v: &str) -> std::result::Result<String, String> {
    let v = v.trim();
    let body = v
        .strip_prefix("sha256:")
        .or_else(|| v.strip_prefix("SHA256:"))
        .unwrap_or(v);
    let h: String = body
        .chars()
        .filter(|c| *c != ':')
        .collect::<String>()
        .to_ascii_lowercase();
    if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(h)
    } else {
        Err("remote_fingerprint must be sha256:<64 hex characters>".to_string())
    }
}

/// Host side: the server configuration plus the certificate's fingerprint.
pub fn server_config(cert: &Path, key: &Path) -> Result<(Arc<rustls::ServerConfig>, String)> {
    let missing = |p: &Path| {
        AitError::new(
            "CONFIG",
            format!(
                "ait serve --lan needs a TLS certificate and key; {} does not exist (make them with scripts/make-cert.ps1 or scripts/make-cert.sh, or set tls_cert / tls_key in default.cfg)",
                p.display()
            ),
        )
    };
    for p in [cert, key] {
        if !p.is_file() {
            return Err(missing(p));
        }
    }
    let bad = |what: &str, p: &Path, e: String| {
        AitError::new(
            "CONFIG",
            format!("cannot read the TLS {what} {}: {e}", p.display()),
        )
    };
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| bad("certificate", cert, e.to_string()))?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| bad("certificate", cert, e.to_string()))?;
    let Some(first) = certs.first() else {
        return Err(bad(
            "certificate",
            cert,
            "no certificate in the file".to_string(),
        ));
    };
    let fp = fingerprint(first.as_ref())?;
    let key_der = PrivateKeyDer::from_pem_file(key).map_err(|e| bad("key", key, e.to_string()))?;
    let cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| AitError::new("CONFIG", format!("TLS setup failed: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs, key_der)
        .map_err(|e| {
            AitError::new(
                "CONFIG",
                format!(
                    "the TLS certificate {} and key {} do not work together: {e}",
                    cert.display(),
                    key.display()
                ),
            )
        })?;
    Ok((Arc::new(cfg), fp))
}

/// Accepts only the certificate whose SHA-256 matches the pin; remembers
/// what the host presented so a mismatch can be reported clearly.
#[derive(Debug)]
pub struct Pinned {
    want: String,
    algs: WebPkiSupportedAlgorithms,
    pub seen: Mutex<Option<String>>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let got = sha256(end_entity.as_ref())
            .map(|h| hex(&h))
            .map_err(|e| rustls::Error::General(e.message))?;
        if let Ok(mut s) = self.seen.lock() {
            *s = Some(got.clone());
        }
        if got == self.want {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Client side: a configuration that trusts only the pinned certificate.
pub fn client_config(fingerprint_hex: &str) -> Result<(Arc<rustls::ClientConfig>, Arc<Pinned>)> {
    let p = provider();
    let pinned = Arc::new(Pinned {
        want: fingerprint_hex.to_string(),
        algs: p.signature_verification_algorithms,
        seen: Mutex::new(None),
    });
    let cfg = rustls::ClientConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| AitError::new("REMOTE_TLS", format!("TLS setup failed: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(pinned.clone())
        .with_no_client_auth();
    Ok((Arc::new(cfg), pinned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            hex(&sha256(b"abc").unwrap()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn fingerprints_parse_in_common_spellings() {
        let h = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(parse_fingerprint(&format!("sha256:{h}")).unwrap(), h);
        assert_eq!(
            parse_fingerprint(&h.to_ascii_uppercase()).unwrap(),
            h,
            "plain upper-case hex"
        );
        let colons: Vec<String> = h
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).to_string())
            .collect();
        assert_eq!(
            parse_fingerprint(&format!("SHA256:{}", colons.join(":"))).unwrap(),
            h
        );
        assert!(parse_fingerprint("sha256:abc").is_err());
        assert!(parse_fingerprint(&format!("sha256:{}", "z".repeat(64))).is_err());
    }

    #[test]
    fn random_tokens_differ() {
        let a = random_hex(16).unwrap();
        assert_eq!(a.len(), 32);
        assert_ne!(a, random_hex(16).unwrap());
    }
}
