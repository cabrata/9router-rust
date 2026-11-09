//! Install/uninstall the Root CA in the OS trust store. Port of src/mitm/cert/install.js.
//!
//! Trust is what makes the proxy usable by IDEs: without it every intercepted host
//! is a certificate error. All commands go through `dns::exec_with_password` so the
//! sudo password never reaches a shell argument or a log line.

use std::path::Path;

use nr_core::Result;
use sha2::Digest;

use super::{root_ca_cert_path, ROOT_CA_CN};
use crate::mitm::dns;
use crate::mitm::logger;

/// Lower-case hex SHA-256 of the certificate DER (used to detect a *stale* cert:
/// same CN but a different key must not count as installed).
pub fn cert_fingerprint(cert_path: &Path) -> Result<String> {
    let pem = std::fs::read_to_string(cert_path)
        .map_err(|e| nr_core::Error::other(format!("read {}: {e}", cert_path.display())))?;
    let der = decode_pem_body(&pem)
        .ok_or_else(|| nr_core::Error::other("certificate has no PEM body"))?;
    Ok(hex::encode(sha2::Sha256::digest(&der)))
}

/// Strip PEM armor and decode base64 to DER.
fn decode_pem_body(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    if body.is_empty() {
        return None;
    }
    base64_decode(&body)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// Linux trust-store layouts, in probe order (Debian, Arch, Fedora, openSUSE).
const LINUX_CERT_DIRS: &[(&str, &str)] = &[
    ("/usr/local/share/ca-certificates", "update-ca-certificates"),
    ("/etc/ca-certificates/trust-source/anchors", "update-ca-trust"),
    ("/etc/pki/ca-trust/source/anchors", "update-ca-trust"),
    ("/etc/pki/trust/anchors", "update-ca-certificates"),
];

fn linux_cert_config() -> (&'static str, &'static str) {
    LINUX_CERT_DIRS
        .iter()
        .find(|(dir, _)| Path::new(dir).exists())
        .copied()
        // Debian is the most common default when none of the dirs exist yet.
        .unwrap_or(LINUX_CERT_DIRS[0])
}

/// Whether the Root CA is already trusted by the system store.
pub async fn check_cert_installed(cert_path: &Path) -> bool {
    if cfg!(windows) {
        let Ok(fp) = cert_fingerprint(cert_path) else {
            return false;
        };
        return tokio::process::Command::new("certutil")
            .args(["-store", "Root", &fp])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
    }

    if cfg!(target_os = "macos") {
        let Ok(fp) = cert_fingerprint(cert_path) else {
            return false;
        };
        // Compare the exact cert, not just the CN: a regenerated CA with the same
        // name has a different fingerprint and must be re-installed.
        let Ok(out) = tokio::process::Command::new("security")
            .args([
                "find-certificate",
                "-a",
                "-c",
                ROOT_CA_CN,
                "-Z",
                "/Library/Keychains/System.keychain",
            ])
            .output()
            .await
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
        return out.status.success() && text.contains(&fp);
    }

    // Linux: our copied anchor file is the marker.
    let (dir, _) = linux_cert_config();
    Path::new(dir).join("9router-root-ca.crt").exists()
}

/// Install the Root CA into the system trust store (no-op if already there).
pub async fn install_cert(cert_path: &Path, password: &str) -> Result<()> {
    if !cert_path.exists() {
        return Err(nr_core::Error::other(format!(
            "Certificate file not found: {}",
            cert_path.display()
        )));
    }
    if check_cert_installed(cert_path).await {
        logger::log("Cert: already trusted");
        return Ok(());
    }

    if cfg!(windows) {
        // Delete a stale cert with the same CN first, then add ours.
        let script = format!(
            "certutil -delstore Root '{ROOT_CA_CN}' 2>$null | Out-Null; \
             certutil -addstore Root '{}' | Out-Null",
            cert_path.display()
        );
        dns::exec_with_password(&format!("powershell -NoProfile -Command \"{script}\""), password)
            .await
            .map_err(|e| nr_core::Error::other(format!("Failed to install certificate: {e}")))?;
        logger::log("Cert: installed to Windows Root store");
        return Ok(());
    }

    if cfg!(target_os = "macos") {
        let cmd = format!(
            "security delete-certificate -c '{ROOT_CA_CN}' /Library/Keychains/System.keychain 2>/dev/null || true; \
             security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain '{}'",
            cert_path.display()
        );
        dns::exec_with_password(&cmd, password)
            .await
            .map_err(|_| nr_core::Error::other("Certificate install failed"))?;
        logger::log("Cert: installed to system keychain");
        return Ok(());
    }

    install_linux(cert_path, password).await
}

async fn install_linux(cert_path: &Path, password: &str) -> Result<()> {
    if !dns::is_sudo_available() {
        // Containers: no system store to write. Say where the file is so the user
        // can trust it in whichever client actually makes the requests.
        logger::log(&format!(
            "Cert: cannot install to system store without sudo — trust this file on clients: {}",
            cert_path.display()
        ));
        update_nss_databases(cert_path, true).await;
        return Ok(());
    }

    let (dir, update_cmd) = linux_cert_config();
    let dest = format!("{dir}/9router-root-ca.crt");
    let cmd = format!("cp '{}' '{dest}' && ({update_cmd} 2>/dev/null || true)", cert_path.display());
    dns::exec_with_password(&cmd, password)
        .await
        .map_err(|e| nr_core::Error::other(format!("Certificate install failed: {e}")))?;
    // Chromium/Firefox keep their own NSS stores; the system store alone isn't enough.
    update_nss_databases(cert_path, true).await;
    logger::log(&format!("Cert: installed to Linux trust store ({dir})"));
    Ok(())
}

/// Add/remove the CA in user NSS databases (Chromium, Firefox profiles).
///
/// Best-effort: `certutil` is often absent, and a missing browser profile is not an
/// error. Runs unprivileged because NSS stores live in $HOME.
async fn update_nss_databases(cert_path: &Path, add: bool) {
    let name = ROOT_CA_CN;
    let action = if add {
        format!("certutil -d sql:\"$db\" -A -t 'C,,' -n '{name}' -i '{}'", cert_path.display())
    } else {
        format!("certutil -d sql:\"$db\" -D -n '{name}'")
    };
    let script = format!(
        r#"
if ! command -v certutil > /dev/null 2>&1; then exit 0; fi
DIRS="$HOME/.pki/nssdb $HOME/snap/chromium/current/.pki/nssdb"
for base in "$HOME/.mozilla/firefox" "$HOME/snap/firefox/common/.mozilla/firefox"; do
  [ -d "$base" ] || continue
  for profile in "$base"/*/; do
    if [ -f "${{profile}}cert9.db" ] || [ -f "${{profile}}cert8.db" ]; then
      DIRS="$DIRS $profile"
    fi
  done
done
for db in $DIRS; do
  [ -d "$db" ] || continue
  {action} 2>/dev/null || true
done
"#
    );
    // No password: NSS databases are user-owned, so no elevation is involved.
    let _ = dns::exec_with_password(&script, "").await;
}

/// Remove the Root CA from the system trust store.
pub async fn uninstall_cert(cert_path: &Path, password: &str) -> Result<()> {
    if !check_cert_installed(cert_path).await {
        logger::log("Cert: not found in system store");
        return Ok(());
    }

    if cfg!(windows) {
        let cmd = format!("certutil -delstore Root '{ROOT_CA_CN}'");
        dns::exec_with_password(&cmd, password)
            .await
            .map_err(|e| nr_core::Error::other(format!("Failed to uninstall certificate: {e}")))?;
        logger::log("Cert: uninstalled from Windows Root store");
        return Ok(());
    }

    if cfg!(target_os = "macos") {
        let cmd = format!("security delete-certificate -c '{ROOT_CA_CN}' /Library/Keychains/System.keychain");
        dns::exec_with_password(&cmd, password)
            .await
            .map_err(|_| nr_core::Error::other("Failed to uninstall certificate"))?;
        logger::log("Cert: uninstalled from system keychain");
        return Ok(());
    }

    update_nss_databases(cert_path, false).await;
    if !dns::is_sudo_available() {
        return Ok(());
    }
    let (dir, update_cmd) = linux_cert_config();
    let cmd = format!("rm -f '{dir}/9router-root-ca.crt' && ({update_cmd} 2>/dev/null || true)");
    dns::exec_with_password(&cmd, password)
        .await
        .map_err(|_| nr_core::Error::other("Failed to uninstall certificate"))?;
    logger::log("Cert: uninstalled from Linux trust store");
    Ok(())
}

/// Status helper used by the settings API.
pub struct CertStatus {
    pub exists: bool,
    pub trusted: bool,
}

pub async fn status() -> CertStatus {
    let path = root_ca_cert_path();
    let exists = path.exists();
    let trusted = if exists {
        check_cert_installed(&path).await
    } else {
        false
    };
    CertStatus { exists, trusted }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_pem_body_to_der() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ca.crt");
        let ca = crate::mitm::cert::RootCa::generate().unwrap();
        ca.write_files_to(dir.path()).unwrap();
        let fp = cert_fingerprint(&dir.path().join("rootCA.crt")).unwrap();
        assert_eq!(fp.len(), 64, "sha256 hex");
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        // Round-trip: same cert ⇒ same fingerprint.
        let _ = std::fs::copy(dir.path().join("rootCA.crt"), &path);
        assert_eq!(cert_fingerprint(&path).unwrap(), fp);
    }

    #[test]
    fn missing_cert_is_an_explicit_error() {
        let err = cert_fingerprint(Path::new("/nonexistent/ca.crt"));
        assert!(err.is_err());
    }
}
