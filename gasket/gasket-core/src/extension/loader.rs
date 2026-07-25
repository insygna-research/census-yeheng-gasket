//! cdylib plugin loader with ABI version checking.
//!
//! See `gasket-refactor-plan.md` §5.1 / §5.1.1.
//!
//! **ABI honesty**: Rust cdylibs have no stable ABI. A plugin must be compiled
//! against the same host toolchain and dependency versions. `GASKET_ABI_VERSION`
//! is independent of the crate semantic version and is bumped manually whenever
//! a struct layout / enum discriminant / trait vtable changes.

use std::path::{Path, PathBuf};

use libloading::Library;

use crate::extension::api::ExtensionApi;

/// The host's current ABI version. Bumped on any breaking layout change.
pub const GASKET_ABI_VERSION: u32 = 1;

/// String form for `ExtensionApi::api_version()`.
pub const GASKET_ABI_VERSION_STR: &str = "1";

/// Plugin metadata, loaded from the `manifest.toml` beside the `.so`/`.dylib`.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub gasket_abi_version: u32,
    pub description: String,
}

impl PluginManifest {
    /// Parse a minimal TOML manifest:
    /// `name = "x"`, `version = "0.1.0"`, `gasket_abi_version = 1`,
    /// `description = "..."`.
    pub fn parse(toml: &str) -> Result<Self, PluginError> {
        let mut name = String::new();
        let mut version = String::new();
        let mut abi = None;
        let mut description = String::new();
        for line in toml.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("name") {
                name = take_str(rest)?;
            } else if let Some(rest) = line.strip_prefix("version") {
                version = take_str(rest)?;
            } else if let Some(rest) = line.strip_prefix("description") {
                description = take_str(rest)?;
            } else if let Some(rest) = line.strip_prefix("gasket_abi_version") {
                abi = Some(take_u32(rest)?);
            }
        }
        if name.is_empty() {
            return Err(PluginError::BadManifest("missing name".into()));
        }
        Ok(Self {
            name,
            version,
            gasket_abi_version: abi.unwrap_or(0),
            description,
        })
    }
}

fn take_str(after_key: &str) -> Result<String, PluginError> {
    let v = after_key
        .trim_start()
        .strip_prefix('=')
        .ok_or_else(|| PluginError::BadManifest("expected '='".into()))?
        .trim();
    let v = v.trim_matches('"');
    Ok(v.to_string())
}

fn take_u32(after_key: &str) -> Result<u32, PluginError> {
    let v = after_key
        .trim_start()
        .strip_prefix('=')
        .ok_or_else(|| PluginError::BadManifest("expected '='".into()))?
        .trim();
    v.parse::<u32>()
        .map_err(|_| PluginError::BadManifest(format!("bad abi version: {v}")))
}

/// A loaded plugin. `_lib` is held to keep the cdylib resident.
pub struct Plugin {
    pub name: String,
    pub path: PathBuf,
    pub manifest: PluginManifest,
    _lib: Library,
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("incompatible ABI: plugin={plugin} host={host}")]
    IncompatibleAbi { plugin: u32, host: u32 },
    #[error("bad manifest: {0}")]
    BadManifest(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("library load: {0}")]
    Load(String),
}

/// Load a single plugin from `path` (a `.so`/`.dylib`/`.dll`). Calls its
/// `register` symbol, which is expected to populate `api` via the
/// `ExtensionApi` methods.
pub fn load_plugin(path: &Path, api: &mut dyn ExtensionApi) -> Result<Plugin, PluginError> {
    let manifest_path = path.with_extension("toml");
    let manifest_text = std::fs::read_to_string(&manifest_path).unwrap_or_default();
    let manifest = PluginManifest::parse(&manifest_text)?;

    if manifest.gasket_abi_version != GASKET_ABI_VERSION {
        return Err(PluginError::IncompatibleAbi {
            plugin: manifest.gasket_abi_version,
            host: GASKET_ABI_VERSION,
        });
    }

    let lib = unsafe { Library::new(path) }.map_err(|e| PluginError::Load(e.to_string()))?;
    let register: libloading::Symbol<extern "C" fn(&mut dyn ExtensionApi)> =
        unsafe { lib.get(b"register") }.map_err(|e| PluginError::Load(e.to_string()))?;
    register(api);

    Ok(Plugin {
        name: manifest.name.clone(),
        path: path.to_path_buf(),
        manifest,
        _lib: lib,
    })
}

/// Discover candidate plugin files in `dir` (`.so`/`.dylib`/`.dll`).
pub fn discover_plugins(dir: &Path) -> Vec<PathBuf> {
    let exts = if cfg!(target_os = "macos") {
        &["dylib"][..]
    } else if cfg!(target_os = "windows") {
        &["dll"][..]
    } else {
        &["so"][..]
    };

    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| exts.contains(&e))
        {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest() {
        let m = PluginManifest::parse(
            r#"name = "hello"
version = "0.1.0"
gasket_abi_version = 1
description = "a plugin""#,
        )
        .unwrap();
        assert_eq!(m.name, "hello");
        assert_eq!(m.gasket_abi_version, 1);
        assert_eq!(m.description, "a plugin");
    }

    #[test]
    fn rejects_missing_name() {
        let r = PluginManifest::parse("version = \"0.1.0\"");
        assert!(r.is_err());
    }
}
