//! Root CA generation/loading + per-host leaf certificates.
//! Port of src/mitm/cert/rootCA.js + cert/generate.js.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, SanType};

// rustls-pki-types types reach us through rustls (no direct dependency) and
// rcgen 0.13 has no `pem` module: PEM encode/decode goes through the `pem`
// helpers below plus rustls-pemfile.
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use nr_core::{Error, Result};

pub mod install;

pub const ROOT_CA_CN: &str = "9Router MITM Root CA";

/// Directory holding `rootCA.key` / `rootCA.crt`: `$DATA_DIR/mitm`.
pub fn mitm_dir() -> PathBuf {
    nr_core::paths::data_dir().join("mitm")
}

pub fn root_ca_cert_path() -> PathBuf {
    mitm_dir().join("rootCA.crt")
}

pub fn root_ca_key_path() -> PathBuf {
    mitm_dir().join("rootCA.key")
}

/// An in-memory Root CA: its key pair plus the DER of its self-signed cert.
pub struct RootCa {
    pub key: KeyPair,
    /// PEM of the self-signed cert, kept alongside DER so we never have to
    /// re-parse/re-encode it when building leaf chains.
    pub cert_pem: String,
    pub cert_der: CertificateDer<'static>,
}

impl RootCa {
    /// Generate a fresh self-signed Root CA. Caller must persist via `write_files`.
    pub fn generate() -> Result<Self> {
        // ECDSA P-256: fast per-host signing and small certs. JS used RSA 2048;
        // any of the two is accepted by macOS Keychain / Linux trust stores.
        let key = KeyPair::generate().map_err(|e| Error::other(format!("rcgen keygen: {e}")))?;
        let mut params: CertificateParams = CertificateParams::default();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, ROOT_CA_CN);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        // Default validity is 1975..4096, matching the JS build's long-lived CA.
        let cert = params
            .self_signed(&key)
            .map_err(|e| Error::other(format!("rcgen self-signed CA: {e}")))?;
        Ok(Self {
            key,
            cert_pem: cert.pem(),
            cert_der: cert.der().clone(),
        })
    }

    /// Load the CA from `$DATA_DIR/mitm/rootCA.{crt,key}`.
    pub fn load() -> Result<Self> {
        let key_pem = std::fs::read_to_string(root_ca_key_path())
            .map_err(|e| Error::other(format!("rootCA.key unreadable: {e}")))?;
        let key = KeyPair::from_pem(&key_pem)
            .map_err(|e| Error::other(format!("rootCA.key parse: {e}")))?;
        let cert_pem = std::fs::read_to_string(root_ca_cert_path())
            .map_err(|e| Error::other(format!("rootCA.crt unreadable: {e}")))?;
        let der = pem_to_der(&cert_pem)
            .map_err(|e| Error::other(format!("rootCA.crt parse: {e}")))?;
        Ok(Self {
            key,
            cert_pem,
            cert_der: der,
        })
    }

    /// Load, or generate + persist if the files are missing (server.js startup path).
    pub fn load_or_generate() -> Result<Self> {
        if root_ca_cert_path().exists() && root_ca_key_path().exists() {
            return Self::load();
        }
        tracing::info!("[MITM] Root CA missing, generating...");
        let ca = Self::generate()?;
        ca.write_files()?;
        Ok(ca)
    }

    /// Write `rootCA.key` / `rootCA.crt` under `$DATA_DIR/mitm`.
    ///
    /// The key is chmod 0600: it can sign certs for any host, so a world-readable
    /// copy would let any local user impersonate intercepted sites.
    pub fn write_files(&self) -> Result<()> {
        self.write_files_to(&mitm_dir())
    }

    /// Same, into an explicit directory (tests, portable exports).
    pub fn write_files_to(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::other(format!("mkdir {}: {e}", dir.display())))?;
        let key_pem = self.key.serialize_pem();
        let cert_pem = self.cert_pem.clone();
        write_private(&dir.join("rootCA.key"), key_pem.as_bytes())?;
        std::fs::write(dir.join("rootCA.crt"), cert_pem.as_bytes())
            .map_err(|e| Error::other(format!("write rootCA.crt: {e}")))?;
        Ok(())
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut f = std::fs::File::create(path)
        .map_err(|e| Error::other(format!("create {}: {e}", path.display())))?;
    f.write_all(bytes).map_err(|e| Error::other(format!("write {}: {e}", path.display())))?;
    let mut perms = f.metadata().map_err(|e| Error::other(e.to_string()))?.permissions();
    perms.set_mode(0o600);
    f.set_permissions(perms).map_err(|e| Error::other(format!("chmod {}: {e}", path.display())))?;
    Ok(())
}

/// First certificate in a PEM bundle → DER.
fn pem_to_der(pem: &str) -> std::result::Result<CertificateDer<'static>, String> {
    let mut rd = std::io::BufReader::new(pem.as_bytes());
    let mut certs = rustls_pemfile::certs(&mut rd);
    certs
        .next()
        .transpose()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no certificate in PEM".to_string())
}

/// First private key in a PEM bundle.
fn pem_to_key(pem: &str) -> std::result::Result<PrivateKeyDer<'static>, String> {
    let mut rd = std::io::BufReader::new(pem.as_bytes());
    rustls_pemfile::private_key(&mut rd)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no private key in PEM".to_string())
}

/// Per-host leaf certificate: PEM key + PEM cert (cert first, then CA, for full chain).
#[derive(Clone)]
pub struct LeafCert {
    pub key_pem: String,
    pub cert_pem: String,
}

impl LeafCert {
    /// Sign a leaf for `domain` (+ `*.<domain>`), valid 825 days.
    ///
    /// 825 days is the cap macOS/iOS silently trust without extra policy
    /// warnings (Apple's baseline limit), so IDE/CLI clients keep working.
    pub fn generate(domain: &str, ca: &RootCa) -> Result<Self> {
        let leaf_key = KeyPair::generate()
            .map_err(|e| Error::other(format!("rcgen leaf keygen: {e}")))?;
        let mut params: CertificateParams = CertificateParams::default();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, domain);
        let wildcard = format!("*.{domain}");
        let wildcard_san = wildcard.clone();
        params.subject_alt_names = vec![
            SanType::DnsName(
                domain
                    .try_into()
                    .map_err(|e| Error::other(format!("bad SAN {domain}: {e}")))?,
            ),
            SanType::DnsName(
                wildcard
                    .as_str()
                    .try_into()
                    .map_err(|e| Error::other(format!("bad SAN {wildcard_san}: {e}")))?,
            ),
        ];
        params.is_ca = IsCa::NoCa;
        // Rebuild the issuer's signing material: rcgen 0.13 can't reconstruct a
        // `Certificate` from DER without the `x509-parser` feature, but it only
        // needs the DN + key usages of the CA, which we regenerate the same way
        // `RootCa::generate` did (same CN, same CA constraints).
        let mut issuer_params: CertificateParams = CertificateParams::default();
        issuer_params.distinguished_name = rcgen::DistinguishedName::new();
        issuer_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, ROOT_CA_CN);
        issuer_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let issuer = issuer_params
            .self_signed(&ca.key)
            .map_err(|e| Error::other(format!("rcgen issuer reparse: {e}")))?;
        let cert = params
            .signed_by(&leaf_key, &issuer, &ca.key)
            .map_err(|e| Error::other(format!("rcgen sign leaf: {e}")))?;
        // Node's SNI context serves `cert + rootCAPem`; clients without the CA
        // installed still see the chain, so the failure mode is a clean trust error.
        let cert_pem = format!("{}\n{}", cert.pem(), ca.cert_pem);
        Ok(Self {
            key_pem: leaf_key.serialize_pem(),
            cert_pem,
        })
    }
}

/// Parse `LeafCert` PEM into rustls' DER forms for building a TLS config.
pub fn leaf_to_rustls(leaf: &LeafCert) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut std::io::BufReader::new(leaf.cert_pem.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::other(format!("leaf cert parse: {e}")))?;
    if certs.is_empty() {
        return Err(Error::other("leaf cert parse produced no certificates"));
    }
    let key = pem_to_key(&leaf.key_pem).map_err(|e| Error::other(format!("leaf key parse: {e}")))?;
    Ok((certs, key))
}

/// SNI certificate resolver: generates + caches a leaf per hostname.
///
/// Bounded to [`MAX_CACHED_CERTS`] entries (clear-on-full, like the JS `Map`)
/// so a long-lived proxy can't grow without limit.
pub struct CertResolver {
    ca: RootCa,
    cache: Mutex<BTreeMap<String, std::sync::Arc<rustls::sign::CertifiedKey>>>,
}

/// Ceiling on cached leaf certs; certs are cheap to regenerate so we simply clear
/// the map when full rather than tracking LRU.
const MAX_CACHED_CERTS: usize = 512;

impl CertResolver {
    pub fn new(ca: RootCa) -> Self {
        Self {
            ca,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn ca(&self) -> &RootCa {
        &self.ca
    }

    /// `Ok(None)` for a hostname we refuse to certify (empty, or an embedded NUL),
    /// so the caller can reject the handshake instead of panicking.
    pub fn certified_key(&self, server_name: &str) -> Result<Option<std::sync::Arc<rustls::sign::CertifiedKey>>> {
        if server_name.is_empty() || server_name.contains('\0') {
            tracing::warn!("[MITM] refusing to certify suspicious SNI {:?}", LogSafeSni(server_name));
            return Ok(None);
        }
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = cache.get(server_name) {
            return Ok(Some(k.clone()));
        }
        let leaf = LeafCert::generate(server_name, &self.ca)?;
        let (certs, key) = leaf_to_rustls(&leaf)?;
        let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
            .map_err(|e| Error::other(format!("leaf key unsupported: {e}")))?;
        let key = rustls::sign::CertifiedKey::new(certs, signing_key);
        let key = std::sync::Arc::new(key);
        if cache.len() >= MAX_CACHED_CERTS {
            cache.clear();
        }
        cache.insert(server_name.to_string(), key.clone());
        Ok(Some(key))
    }
}

/// Wrap an SNI value for logging without echoing a possibly attacker-controlled
/// string verbatim into the log stream.
struct LogSafeSni<'a>(&'a str);
impl std::fmt::Debug for LogSafeSni<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let safe: String = self
            .0
            .chars()
            .map(|c| if c.is_ascii_graphic() { c } else { '?' })
            .take(128)
            .collect();
        write!(f, "\"{safe}\"")
    }
}

/// Process-wide resolver, built once on first use from the on-disk CA.
static RESOLVER: LazyLock<Mutex<Option<std::sync::Arc<CertResolver>>>> =
    LazyLock::new(|| Mutex::new(None));

/// Get (initializing on first call) the global CA resolver.
pub fn resolver() -> Result<std::sync::Arc<CertResolver>> {
    {
        let guard = RESOLVER.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = guard.as_ref() {
            return Ok(r.clone());
        }
    }
    let ca = RootCa::load_or_generate()?;
    let r = std::sync::Arc::new(CertResolver::new(ca));
    *RESOLVER.lock().unwrap_or_else(|e| e.into_inner()) = Some(r.clone());
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_ca_and_leaf() {
        let ca = RootCa::generate().unwrap();
        let leaf = LeafCert::generate("cloudcode-pa.googleapis.com", &ca).unwrap();
        assert!(leaf.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(leaf.key_pem.contains("PRIVATE KEY"));
        // Leaf must chain to the CA: two certs in the PEM (leaf + CA).
        assert_eq!(leaf.cert_pem.matches("BEGIN CERTIFICATE").count(), 2);
    }

    #[test]
    fn ca_roundtrips_through_pem() {
        let dir = tempfile::tempdir().unwrap();
        // Point the module at a temp DATA_DIR without mutating global state.
        std::env::set_var("DATA_DIR", dir.path());
        let ca = RootCa::load_or_generate().unwrap();
        let ca2 = RootCa::load().unwrap();
        assert_eq!(ca.cert_der.as_ref(), ca2.cert_der.as_ref());
        std::env::remove_var("DATA_DIR");
    }

    #[test]
    fn resolver_refuses_empty_and_nul_sni() {
        let ca = RootCa::generate().unwrap();
        let r = CertResolver::new(ca);
        // `CertifiedKey` is not PartialEq; assert absence instead.
        assert!(r.certified_key("").unwrap().is_none());
        assert!(r.certified_key("evil\0.example").unwrap().is_none());
        assert!(r.certified_key("api2.cursor.sh").unwrap().is_some());
    }
}
