//! API keys: the OS keychain first, then a clearly labelled owner-only fallback file, then the
//! usual environment variables. Keys are never logged or written to project files.

use crate::BackendKind;
use craft_anything_engine::tools::asset_tools::AssetKeys;
use craft_anything_engines::assets::{AssetProvider, AssetProviders};
use craft_anything_evidence::{ErrorCode, ToolError};
use std::path::PathBuf;
use std::time::Duration;

/// Service name in the OS keychain.
pub const SERVICE: &str = "ai.storyteller.craft-anything";
/// Set to `1` to skip the OS keychain (tests, headless servers).
pub const NO_KEYCHAIN_ENV: &str = "CRAFT_ANYTHING_NO_KEYCHAIN";
/// The keychain can block (a locked desktop keyring); never wait longer than this.
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(3);

const FILE_HEADER: &str = "# Craft Anything AI keys: FALLBACK STORE.\n# These keys are here because no OS keychain was available when they were saved.\n# This file is readable only by you (0600). Delete a line to forget a key.\n";

/// Where the fal.ai key for generated game assets is stored.
pub const FAL_KEY: &str = "assets:fal";

/// Where an asset provider's key is stored (`assets:<provider id>`).
pub fn asset_key_name(provider: &str) -> String {
    format!("assets:{provider}")
}

/// An asset provider's key: stored, else the provider's own environment variable.
pub fn asset_key(store: &KeyStore, provider: &dyn AssetProvider) -> Option<String> {
    store.get(&asset_key_name(provider.id())).or_else(|| provider.key_env().and_then(|v| std::env::var(v).ok())).filter(|k| !k.trim().is_empty())
}

/// Every provider's key that is set up, for the builder's tool server.
pub fn asset_keys(store: &KeyStore, providers: &AssetProviders) -> AssetKeys {
    AssetKeys(
        providers.ids().into_iter().filter_map(|id| providers.get(id)).filter_map(|p| asset_key(store, p.as_ref()).map(|k| (p.id().to_string(), k))).collect(),
    )
}

/// The fal.ai key for game assets: stored, else fal's environment variable.
pub fn fal_key(store: &KeyStore) -> Option<String> {
    asset_key(store, &craft_anything_engines::assets::fal::Fal::default())
}

/// The environment variable conventionally holding a key for this kind.
pub fn env_var(kind: BackendKind) -> Option<&'static str> {
    match kind {
        BackendKind::Anthropic => Some("ANTHROPIC_API_KEY"),
        BackendKind::Openai => Some("OPENAI_API_KEY"),
        BackendKind::Openrouter => Some("OPENROUTER_API_KEY"),
        BackendKind::OpenaiCompatible | BackendKind::AgentCli => None,
    }
}

/// Outcome of trying to delete a key from the OS keychain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
    /// No usable keychain (locked, missing, or no storage access).
    Unavailable,
    /// The keychain was reached but refused the operation, or the call timed out.
    Failed,
}

/// Where keys live.
#[derive(Debug, Clone)]
pub struct KeyStore {
    file: PathBuf,
    use_keychain: bool,
}

impl KeyStore {
    /// The default store: keychain + `<config dir>/ai-keys.toml`.
    pub fn system() -> Self {
        let no_chain = std::env::var(NO_KEYCHAIN_ENV).is_ok_and(|v| v == "1");
        KeyStore { file: craft_anything_engine::settings::config_dir().join("ai-keys.toml"), use_keychain: !no_chain }
    }

    /// A file-only store (tests).
    pub fn file_only(file: PathBuf) -> Self {
        KeyStore { file, use_keychain: false }
    }

    fn read_file(&self) -> toml::Table {
        std::fs::read_to_string(&self.file).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    fn write_file(&self, t: &toml::Table) -> Result<(), ToolError> {
        let body = toml::to_string(t).map_err(|e| ToolError::internal(e.to_string()))?;
        craft_anything_platform::write_atomic_private(&self.file, format!("{FILE_HEADER}{body}").as_bytes())
    }

    /// The stored key for a backend (keychain, then fallback file).
    pub fn get(&self, backend: &str) -> Option<String> {
        if self.use_keychain
            && let Some(k) = keychain::get(backend)
        {
            return Some(k);
        }
        self.read_file().get(backend).and_then(|v| v.as_str()).map(str::to_string).filter(|k| !k.is_empty())
    }

    /// The key to use: stored, else the kind's environment variable.
    pub fn resolve(&self, backend: &str, kind: BackendKind) -> Option<String> {
        self.get(backend).or_else(|| env_var(kind).and_then(|v| std::env::var(v).ok()).filter(|k| !k.trim().is_empty()))
    }

    /// Save a key. Returns where it went: `"keychain"` or `"file"`.
    pub fn set(&self, backend: &str, key: &str) -> Result<&'static str, ToolError> {
        if key.trim().is_empty() {
            return Err(ToolError::invalid("The key is empty."));
        }
        if self.use_keychain && keychain::set(backend, key) {
            // Don't leave a stale copy in the fallback file.
            let mut t = self.read_file();
            if t.remove(backend).is_some() {
                self.write_file(&t)?;
            }
            return Ok("keychain");
        }
        let mut t = self.read_file();
        t.insert(backend.to_string(), toml::Value::String(key.to_string()));
        self.write_file(&t).map_err(|e| ToolError::new(ErrorCode::NoPermission, format!("Couldn't save the key: {}", e.message)))?;
        Ok("file")
    }

    pub fn delete(&self, backend: &str) -> Result<(), ToolError> {
        self.delete_inner(backend, |b| if self.use_keychain { keychain::delete(b) } else { DeleteOutcome::NotFound })
    }

    /// The deletion policy with the keychain call injected, so tests can fake a keychain failure.
    fn delete_inner(&self, backend: &str, keychain_delete: impl FnOnce(&str) -> DeleteOutcome) -> Result<(), ToolError> {
        let chain = keychain_delete(backend);
        if chain == DeleteOutcome::Failed {
            return Err(ToolError::new(ErrorCode::NoPermission, "The system keychain refused to remove the key."));
        }
        let mut t = self.read_file();
        let removed_file = t.remove(backend).is_some();
        if removed_file {
            self.write_file(&t)?;
        }
        match chain {
            DeleteOutcome::Deleted | DeleteOutcome::NotFound => Ok(()),
            // No OS keychain: the private fallback file is the store, so removing it is enough.
            DeleteOutcome::Unavailable if removed_file => Ok(()),
            DeleteOutcome::Unavailable => Err(ToolError::new(ErrorCode::NoPermission, "Couldn't reach the system keychain to remove the key.")),
            DeleteOutcome::Failed => Err(ToolError::new(ErrorCode::NoPermission, "The system keychain refused to remove the key.")),
        }
    }
}

/// Run a keychain operation on a helper thread with a deadline.
fn bounded<T: Send + 'static>(f: impl FnOnce() -> Option<T> + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("keychain".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    rx.recv_timeout(KEYCHAIN_TIMEOUT).ok().flatten()
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod keychain {
    use super::{SERVICE, bounded};

    pub fn get(backend: &str) -> Option<String> {
        let b = backend.to_string();
        bounded(move || keyring::Entry::new(SERVICE, &b).ok()?.get_password().ok())
    }

    pub fn set(backend: &str, key: &str) -> bool {
        let (b, k) = (backend.to_string(), key.to_string());
        bounded(move || keyring::Entry::new(SERVICE, &b).ok()?.set_password(&k).ok()).is_some()
    }

    pub fn delete(backend: &str) -> super::DeleteOutcome {
        use super::DeleteOutcome;
        let b = backend.to_string();
        let result = bounded(move || match keyring::Entry::new(SERVICE, &b) {
            Ok(entry) => match entry.delete_credential() {
                Ok(()) => Some(DeleteOutcome::Deleted),
                Err(keyring::Error::NoEntry) => Some(DeleteOutcome::NotFound),
                Err(keyring::Error::NoStorageAccess(_)) => Some(DeleteOutcome::Unavailable),
                Err(_) => Some(DeleteOutcome::Failed),
            },
            Err(keyring::Error::NoStorageAccess(_)) => Some(DeleteOutcome::Unavailable),
            Err(_) => Some(DeleteOutcome::Failed),
        });
        // A timed-out or unstartable keychain call is a failure, never a silent success.
        result.unwrap_or(DeleteOutcome::Failed)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod keychain {
    pub fn get(_: &str) -> Option<String> {
        let _ = super::bounded(|| None::<()>);
        None
    }
    pub fn set(_: &str, _: &str) -> bool {
        false
    }
    pub fn delete(_: &str) -> super::DeleteOutcome {
        super::DeleteOutcome::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second provider with no environment variable, so nothing here reads the real one.
    struct Other;

    impl AssetProvider for Other {
        fn id(&self) -> &'static str {
            "other"
        }
        fn name(&self) -> &'static str {
            "Other"
        }
        fn generate(&self, _key: &str, _req: &craft_anything_engines::assets::AssetRequest) -> Result<craft_anything_engines::assets::Generated, ToolError> {
            Err(ToolError::invalid("not here"))
        }
    }

    #[test]
    fn each_asset_provider_has_its_own_key() {
        let d = tempfile::tempdir().unwrap();
        let s = KeyStore::file_only(d.path().join("keys.toml"));
        let mut providers = AssetProviders::built_in();
        providers.register(std::sync::Arc::new(Other));
        s.set(&asset_key_name("other"), "o-1").unwrap();
        let keys = asset_keys(&s, &providers);
        assert_eq!(keys.get("other"), Some("o-1"));
        // fal's is stored where it always was, apart from the other provider's.
        assert_eq!(asset_key_name("fal"), FAL_KEY);
        s.set(FAL_KEY, "f-1").unwrap();
        let keys = asset_keys(&s, &providers);
        assert_eq!((keys.get("fal"), keys.get("other")), (Some("f-1"), Some("o-1")));
        s.delete(&asset_key_name("other")).unwrap();
        assert_eq!(asset_keys(&s, &providers).get("other"), None);
        assert_eq!(fal_key(&s).as_deref(), Some("f-1"));
    }

    #[test]
    fn keychain_delete_failures_are_not_reported_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ai-keys.toml");
        // file_only never touches the machine's keychain; delete_inner injects the keychain result.
        let s = KeyStore::file_only(file.clone());
        s.set("Claude", "sk-test").unwrap();
        let err = s.delete_inner("Claude", |_| DeleteOutcome::Failed).unwrap_err();
        assert_eq!(err.code, ErrorCode::NoPermission);
        assert!(s.get("Claude").is_some(), "a failed keychain delete still reports the fallback copy");
        // A successful delete removes the fallback copy too.
        s.delete_inner("Claude", |_| DeleteOutcome::NotFound).unwrap();
        assert!(s.get("Claude").is_none());
        // With nothing left in the fallback file, an unreachable keychain is an error, not success.
        let err = s.delete_inner("Claude", |_| DeleteOutcome::Unavailable).unwrap_err();
        assert_eq!(err.code, ErrorCode::NoPermission);
    }

    #[test]
    fn file_fallback_is_labelled_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let s = KeyStore::file_only(dir.path().join("ai-keys.toml"));
        assert_eq!(s.set("Claude", "sk-test").unwrap(), "file");
        assert_eq!(s.get("Claude").as_deref(), Some("sk-test"));
        let text = std::fs::read_to_string(dir.path().join("ai-keys.toml")).unwrap();
        assert!(text.starts_with("# Craft Anything AI keys: FALLBACK STORE."));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(dir.path().join("ai-keys.toml")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        s.delete("Claude").unwrap();
        assert!(s.get("Claude").is_none());
        assert!(s.set("x", " ").is_err());
        assert_eq!(env_var(BackendKind::Openrouter), Some("OPENROUTER_API_KEY"));
    }
}
