//! TLS certificate management for HyprKVM
//!
//! Handles certificate generation, loading, and fingerprint calculation.

use std::fs;
use std::io::{self, BufReader};
use std::path::Path;
use std::sync::Arc;

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, ServerConfig};
use rustls_pemfile::{certs, private_key};
use sha2::{Digest, Sha256};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// TLS-related errors
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Certificate generation error: {0}")]
    CertGen(String),

    #[error("Certificate loading error: {0}")]
    CertLoad(String),

    #[error("TLS configuration error: {0}")]
    Config(String),

    #[error("Certificate verification failed: {0}")]
    Verification(String),
}

/// Certificate fingerprint (SHA-256)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    /// Calculate fingerprint from DER-encoded certificate
    pub fn from_der(der: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(der);
        let result = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&result);
        Self(bytes)
    }

    /// Convert to hex string (colon-separated, uppercase)
    pub fn to_hex(&self) -> String {
        self.0
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Parse from hex string (colon-separated or continuous)
    pub fn from_hex(s: &str) -> Result<Self, TlsError> {
        let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if clean.len() != 64 {
            return Err(TlsError::Verification(format!(
                "Invalid fingerprint length: expected 64 hex chars, got {}",
                clean.len()
            )));
        }

        let mut bytes = [0u8; 32];
        for (i, chunk) in clean.as_bytes().chunks(2).enumerate() {
            let hex = std::str::from_utf8(chunk).unwrap();
            bytes[i] = u8::from_str_radix(hex, 16)
                .map_err(|e| TlsError::Verification(format!("Invalid hex: {}", e)))?;
        }

        Ok(Self(bytes))
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// Generate a self-signed ECDSA P-256 certificate
pub fn generate_certificate(machine_name: &str) -> Result<(String, String), TlsError> {
    tracing::info!("Generating new self-signed certificate for '{}'", machine_name);

    // Generate ECDSA P-256 key pair
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|e| TlsError::CertGen(format!("Failed to generate key pair: {}", e)))?;

    // Build certificate parameters
    let mut params = CertificateParams::default();

    // Set distinguished name
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, machine_name);
    dn.push(DnType::OrganizationName, "HyprKVM");
    params.distinguished_name = dn;

    // Set validity (10 years)
    params.not_before = time::OffsetDateTime::now_utc();
    params.not_after = params.not_before + time::Duration::days(3650);

    // Add Subject Alternative Names
    params.subject_alt_names = vec![
        rcgen::SanType::DnsName(machine_name.try_into().map_err(|e| {
            TlsError::CertGen(format!("Invalid machine name for SAN: {}", e))
        })?),
    ];

    // Generate certificate
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| TlsError::CertGen(format!("Failed to generate certificate: {}", e)))?;

    // Serialize to PEM
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    tracing::info!("Certificate generated successfully");

    Ok((cert_pem, key_pem))
}

/// Ensure certificate exists, generating if needed
pub fn ensure_certificate(
    cert_path: &Path,
    key_path: &Path,
    machine_name: &str,
) -> Result<(), TlsError> {
    if cert_path.exists() && key_path.exists() {
        tracing::debug!("Certificate files exist: {:?}, {:?}", cert_path, key_path);
        return Ok(());
    }

    tracing::info!("Certificate files not found, generating new certificate");

    let (cert_pem, key_pem) = generate_certificate(machine_name)?;

    // Ensure parent directories exist
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Write certificate
    fs::write(cert_path, &cert_pem)?;
    tracing::info!("Wrote certificate to {:?}", cert_path);

    // Write private key with restrictive permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true).mode(0o600);
        let mut file = opts.open(key_path)?;
        std::io::Write::write_all(&mut file, key_pem.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        fs::write(key_path, &key_pem)?;
    }
    tracing::info!("Wrote private key to {:?}", key_path);

    Ok(())
}

/// Load certificate chain from PEM file
pub fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let file = fs::File::open(path)
        .map_err(|e| TlsError::CertLoad(format!("Failed to open {:?}: {}", path, e)))?;
    let mut reader = BufReader::new(file);

    let certs: Vec<CertificateDer<'static>> = certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::CertLoad(format!("Failed to parse certificates: {}", e)))?;

    if certs.is_empty() {
        return Err(TlsError::CertLoad("No certificates found in file".into()));
    }

    Ok(certs)
}

/// Load private key from PEM file
pub fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let file = fs::File::open(path)
        .map_err(|e| TlsError::CertLoad(format!("Failed to open {:?}: {}", path, e)))?;
    let mut reader = BufReader::new(file);

    private_key(&mut reader)
        .map_err(|e| TlsError::CertLoad(format!("Failed to parse private key: {}", e)))?
        .ok_or_else(|| TlsError::CertLoad("No private key found in file".into()))
}

/// Get fingerprint of certificate file
pub fn get_cert_fingerprint(path: &Path) -> Result<Fingerprint, TlsError> {
    let certs = load_certs(path)?;
    if let Some(cert) = certs.first() {
        Ok(Fingerprint::from_der(cert.as_ref()))
    } else {
        Err(TlsError::CertLoad("No certificate in file".into()))
    }
}

/// Create a TLS acceptor for server-side connections
pub fn create_tls_acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor, TlsError> {
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| TlsError::Config(format!("Failed to create server config: {}", e)))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Custom certificate verifier that supports TOFU and fingerprint pinning
#[derive(Debug)]
pub struct HyprkvmCertVerifier {
    /// Expected fingerprint (if pinned)
    expected_fingerprint: Option<Fingerprint>,
    /// Callback to handle TOFU (returns true to accept, false to reject)
    /// If None, TOFU is disabled
    tofu_enabled: bool,
}

impl HyprkvmCertVerifier {
    /// Create verifier with pinned fingerprint
    pub fn with_fingerprint(fingerprint: Fingerprint) -> Self {
        Self {
            expected_fingerprint: Some(fingerprint),
            tofu_enabled: false,
        }
    }

    /// Create verifier with TOFU enabled
    pub fn with_tofu() -> Self {
        Self {
            expected_fingerprint: None,
            tofu_enabled: true,
        }
    }

    /// Create verifier with fingerprint, falling back to TOFU if not set
    pub fn new(fingerprint: Option<Fingerprint>, tofu_enabled: bool) -> Self {
        Self {
            expected_fingerprint: fingerprint,
            tofu_enabled,
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for HyprkvmCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fingerprint = Fingerprint::from_der(end_entity.as_ref());

        if let Some(ref expected) = self.expected_fingerprint {
            // Fingerprint pinning mode
            if fingerprint == *expected {
                tracing::debug!("Certificate fingerprint matches pinned value");
                return Ok(rustls::client::danger::ServerCertVerified::assertion());
            } else {
                tracing::error!(
                    "Certificate fingerprint mismatch! Expected: {}, Got: {}",
                    expected,
                    fingerprint
                );
                return Err(rustls::Error::General(
                    "Certificate fingerprint mismatch".into(),
                ));
            }
        }

        if self.tofu_enabled {
            // TOFU mode - accept any certificate but log for manual verification
            tracing::warn!(
                "TOFU: Accepting certificate with fingerprint: {}",
                fingerprint
            );
            tracing::warn!("TOFU: Add this fingerprint to config to pin the certificate");
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        }

        Err(rustls::Error::General(
            "No verification method configured".into(),
        ))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Create a TLS connector for client-side connections
pub fn create_tls_connector(
    expected_fingerprint: Option<&str>,
    tofu_enabled: bool,
) -> Result<TlsConnector, TlsError> {
    let fingerprint = match expected_fingerprint {
        Some(fp) => Some(Fingerprint::from_hex(fp)?),
        None => None,
    };

    let verifier = HyprkvmCertVerifier::new(fingerprint, tofu_enabled);

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();

    Ok(TlsConnector::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_fingerprint_roundtrip() {
        let bytes = [0xABu8; 32];
        let fp = Fingerprint(bytes);
        let hex = fp.to_hex();
        let parsed = Fingerprint::from_hex(&hex).unwrap();
        assert_eq!(fp, parsed);
    }

    #[test]
    fn test_generate_certificate() {
        let (cert_pem, key_pem) = generate_certificate("test-machine").unwrap();
        assert!(cert_pem.contains("-----BEGIN CERTIFICATE-----"));
        assert!(key_pem.contains("-----BEGIN PRIVATE KEY-----"));
    }

    #[test]
    fn test_ensure_certificate() {
        let dir = tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");

        ensure_certificate(&cert_path, &key_path, "test-machine").unwrap();

        assert!(cert_path.exists());
        assert!(key_path.exists());

        // Second call should not regenerate
        let cert_content = fs::read_to_string(&cert_path).unwrap();
        ensure_certificate(&cert_path, &key_path, "test-machine").unwrap();
        let cert_content2 = fs::read_to_string(&cert_path).unwrap();
        assert_eq!(cert_content, cert_content2);
    }

    #[test]
    fn test_fingerprint_calculation() {
        let dir = tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");

        ensure_certificate(&cert_path, &key_path, "test-machine").unwrap();

        let fp = get_cert_fingerprint(&cert_path).unwrap();
        assert_eq!(fp.to_hex().len(), 95); // 32 bytes * 2 + 31 colons
    }
}
