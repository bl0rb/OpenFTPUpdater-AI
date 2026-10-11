//! SFTP upload. The server's host key must match the fingerprint the user trusted once
//! (trust on first use); a changed key stops every connection.

use crate::config::{AuthKind, SftpConfig};
use crate::paths::expand_home;
use base64::Engine;
use sha2::{Digest, Sha256};
use ssh2::{OpenFlags, OpenType, RenameFlags, Session, Sftp};
use std::fmt;
use std::fs;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

#[derive(Debug)]
pub enum ConnectError {
    /// First connection: the user must confirm this fingerprint.
    UnknownHostKey(String),
    /// The server presents a different key than the trusted one.
    HostKeyChanged { trusted: String, presented: String },
    Other(String),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::UnknownHostKey(fp) => write!(f, "Unbekannter Server-Schlüssel {fp}"),
            ConnectError::HostKeyChanged { trusted, presented } => write!(
                f,
                "Der Server-Schlüssel hat sich geändert (vertraut: {trusted}, jetzt: {presented}). Verbindung abgebrochen."
            ),
            ConnectError::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ConnectError {}

fn other(context: &str, e: impl fmt::Display) -> ConnectError {
    ConnectError::Other(format!("{context}: {e}"))
}

pub struct Connection {
    _session: Session,
    sftp: Sftp,
    root: String,
}

/// `SHA256:<base64 without padding>`, as `ssh-keygen -lf` shows it.
fn fingerprint(key: &[u8]) -> String {
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(key)))
}

pub fn connect(cfg: &SftpConfig, secret: Option<&str>) -> Result<Connection, ConnectError> {
    let host = cfg.host.trim();
    let addr = (host, cfg.port)
        .to_socket_addrs()
        .map_err(|e| other(&format!("{host} nicht gefunden"), e))?
        .next()
        .ok_or_else(|| ConnectError::Other(format!("{host} nicht gefunden")))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(15)).map_err(|e| other(&format!("Verbindung zu {host}:{}", cfg.port), e))?;
    let mut session = Session::new().map_err(|e| other("SSH", e))?;
    session.set_tcp_stream(tcp);
    session.set_timeout(60_000);
    session.handshake().map_err(|e| other("SSH-Handshake", e))?;

    let (key, _) = session.host_key().ok_or_else(|| ConnectError::Other("Server liefert keinen Host-Key".into()))?;
    let presented = fingerprint(key);
    if cfg.host_key.is_empty() {
        return Err(ConnectError::UnknownHostKey(presented));
    }
    if cfg.host_key != presented {
        return Err(ConnectError::HostKeyChanged { trusted: cfg.host_key.clone(), presented });
    }

    let user = cfg.user.trim();
    match cfg.auth {
        AuthKind::Password => {
            let password = secret.ok_or_else(|| ConnectError::Other("Kein Passwort gespeichert".into()))?;
            session.userauth_password(user, password).map_err(|e| other("Anmeldung fehlgeschlagen", e))?;
        }
        AuthKind::Key => {
            let key_path = expand_home(cfg.key_path.trim());
            session
                .userauth_pubkey_file(user, None, &key_path, secret.filter(|s| !s.is_empty()))
                .map_err(|e| other("Anmeldung mit Schlüssel fehlgeschlagen", e))?;
        }
    }
    if !session.authenticated() {
        return Err(ConnectError::Other("Anmeldung fehlgeschlagen".into()));
    }
    let sftp = session.sftp().map_err(|e| other("SFTP nicht verfügbar", e))?;
    let root = cfg.remote_dir.trim().trim_end_matches('/').to_owned();
    Ok(Connection { _session: session, sftp, root })
}

impl Connection {
    fn remote(&self, relative: &str) -> String {
        if self.root.is_empty() {
            relative.to_owned()
        } else {
            format!("{}/{}", self.root, relative)
        }
    }

    /// Names in the target folder (for the precheck), sorted; folders end with `/`.
    pub fn list_root(&self) -> anyhow::Result<Vec<String>> {
        let root = if self.root.is_empty() { "." } else { self.root.as_str() };
        let mut names: Vec<String> = self
            .sftp
            .readdir(Path::new(root))
            .map_err(|e| anyhow::anyhow!("Zielordner {root} nicht lesbar: {e}"))?
            .into_iter()
            .filter_map(|(path, stat)| {
                let name = path.file_name()?.to_string_lossy().into_owned();
                Some(if stat.is_dir() { format!("{name}/") } else { name })
            })
            .collect();
        names.sort();
        Ok(names)
    }

    fn ensure_dirs(&self, relative_file: &str) -> anyhow::Result<()> {
        let mut current = self.root.clone();
        let parts: Vec<&str> = relative_file.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            current = if current.is_empty() { (*part).to_owned() } else { format!("{current}/{part}") };
            if self.sftp.stat(Path::new(&current)).is_err() {
                self.sftp
                    .mkdir(Path::new(&current), 0o755)
                    .map_err(|e| anyhow::anyhow!("Ordner {current} anlegen: {e}"))?;
            }
        }
        Ok(())
    }

    /// Writes to a temporary name and renames, so visitors never get a half-written file.
    pub fn upload(&self, local: &Path, relative: &str) -> anyhow::Result<()> {
        self.ensure_dirs(relative)?;
        let target = self.remote(relative);
        let temp = format!("{target}.orp-upload");
        let data = fs::read(local)?;
        {
            let mut file = self
                .sftp
                .open_mode(Path::new(&temp), OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE, 0o644, OpenType::File)
                .map_err(|e| anyhow::anyhow!("{relative} hochladen: {e}"))?;
            file.write_all(&data).map_err(|e| anyhow::anyhow!("{relative} hochladen: {e}"))?;
        }
        let flags = Some(RenameFlags::OVERWRITE | RenameFlags::ATOMIC | RenameFlags::NATIVE);
        if self.sftp.rename(Path::new(&temp), Path::new(&target), flags).is_err() {
            // SFTP v3 servers refuse to rename onto an existing file.
            let _ = self.sftp.unlink(Path::new(&target));
            self.sftp
                .rename(Path::new(&temp), Path::new(&target), None)
                .map_err(|e| anyhow::anyhow!("{relative} ersetzen: {e}"))?;
        }
        Ok(())
    }

    pub fn delete(&self, relative: &str) -> anyhow::Result<()> {
        match self.sftp.unlink(Path::new(&self.remote(relative))) {
            Ok(()) => Ok(()),
            // Already gone is fine.
            Err(e) if e.code() == ssh2::ErrorCode::SFTP(2) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("{relative} entfernen: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fingerprint;

    #[test]
    fn fingerprint_matches_openssh_format() {
        assert_eq!(fingerprint(b"abc"), "SHA256:ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0");
    }
}
