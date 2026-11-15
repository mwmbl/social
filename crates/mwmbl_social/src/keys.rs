//! Our actor's RSA key: the one `apub::fetch` signs with and the actor document publishes.

use std::io::Write;
use std::path::{Path, PathBuf};

use apub::signatures::keys::{KeyError, RsaSigningKey};

#[derive(Debug, thiserror::Error)]
pub enum KeyFileError {
    #[error("failed to {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("key file {path} is unusable: {source}")]
    Key { path: PathBuf, source: KeyError },
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> KeyFileError {
    let path = path.to_path_buf();
    move |source| KeyFileError::Io {
        action,
        path,
        source,
    }
}

/// The path of the actor key under `keys_dir`.
#[must_use]
pub fn actor_key_path(keys_dir: &Path) -> PathBuf {
    keys_dir.join("actor.pem")
}

fn create_private_dir(dir: &Path) -> Result<(), KeyFileError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).map_err(io("create key directory", dir))
}

fn create_new_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn read_key(path: &Path) -> Result<Option<RsaSigningKey>, KeyFileError> {
    match std::fs::read_to_string(path) {
        Ok(pem) => {
            #[cfg(unix)]
            warn_if_loose(path);
            RsaSigningKey::from_pem(&pem)
                .map(Some)
                .map_err(|source| KeyFileError::Key {
                    path: path.to_path_buf(),
                    source,
                })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io("read key file", path)(e)),
    }
}

#[cfg(unix)]
fn warn_if_loose(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(path = %path.display(), "actor key file is readable by others; run chmod 600");
    }
}

/// Load the actor key from `{keys_dir}/actor.pem`, generating it (PKCS#8 PEM, mode 0600, in a
/// 0700 directory) if there is none.
pub fn load_or_generate(keys_dir: &Path) -> Result<RsaSigningKey, KeyFileError> {
    let path = actor_key_path(keys_dir);
    if let Some(key) = read_key(&path)? {
        return Ok(key);
    }
    create_private_dir(keys_dir)?;
    let key = RsaSigningKey::generate().map_err(|source| KeyFileError::Key {
        path: path.clone(),
        source,
    })?;
    let pem = key.to_pkcs8_pem().map_err(|source| KeyFileError::Key {
        path: path.clone(),
        source,
    })?;
    match create_new_private_file(&path) {
        Ok(mut file) => {
            file.write_all(pem.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(io("write key file", &path))?;
            tracing::info!(path = %path.display(), "generated a new actor key");
            Ok(key)
        }
        // Another process generated one while we did; use theirs.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_key(&path)?
            .ok_or_else(|| io("read key file", &path)(std::io::ErrorKind::NotFound.into())),
        Err(e) => Err(io("create key file", &path)(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn generates_once_then_loads_the_same_key() -> TestResult {
        let dir = tempfile::tempdir()?;
        let keys = dir.path().join("keys");
        let first = load_or_generate(&keys)?;
        let path = actor_key_path(&keys);
        let pem = std::fs::read_to_string(&path)?;
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        let second = load_or_generate(&keys)?;
        assert_eq!(
            first.verifying_key().to_spki_pem()?,
            second.verifying_key().to_spki_pem()?
        );
        assert_eq!(std::fs::read_to_string(&path)?, pem, "not rewritten");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn permissions_are_private() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let keys = dir.path().join("keys");
        load_or_generate(&keys)?;
        let mode = |p: &Path| std::fs::metadata(p).map(|m| m.permissions().mode() & 0o777);
        assert_eq!(mode(&keys)?, 0o700);
        assert_eq!(mode(&actor_key_path(&keys))?, 0o600);
        Ok(())
    }

    #[test]
    fn a_garbage_key_file_is_an_error_naming_the_path() -> TestResult {
        let dir = tempfile::tempdir()?;
        let keys = dir.path().join("keys");
        std::fs::create_dir_all(&keys)?;
        std::fs::write(actor_key_path(&keys), "not a key")?;
        let err = load_or_generate(&keys).err().ok_or("expected an error")?;
        assert!(err.to_string().contains("actor.pem"));
        Ok(())
    }
}
