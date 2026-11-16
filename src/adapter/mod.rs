pub mod health;
pub mod manifest;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

pub use manifest::{Manifest, ManifestError};

pub const MANIFEST_FILE: &str = "adapter.toml";

/// A valid adapter and the folder it lives in.
#[derive(Clone, Debug, Serialize)]
pub struct Adapter {
    #[serde(flatten)]
    pub manifest: Manifest,
    pub dir: PathBuf,
}

/// A folder under `adapters/` that could not be loaded.
#[derive(Clone, Debug, Serialize)]
pub struct LoadError {
    pub path: PathBuf,
    pub error: String,
}

/// All adapters of this server. Invalid ones are listed in `errors`, never dropped silently.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Catalog {
    pub adapters: BTreeMap<String, Adapter>,
    pub errors: Vec<LoadError>,
}

impl Catalog {
    pub fn load(adapters_dir: &Path) -> Self {
        let mut catalog = Self::default();
        let entries = match std::fs::read_dir(adapters_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return catalog,
            Err(error) => {
                catalog.push_error(adapters_dir, error.to_string());
                return catalog;
            }
        };

        let mut dirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            match load_adapter(&dir) {
                Ok(adapter) => {
                    catalog
                        .adapters
                        .insert(adapter.manifest.name.clone(), adapter);
                }
                Err(error) => catalog.push_error(&dir.join(MANIFEST_FILE), error),
            }
        }
        catalog
    }

    pub fn get(&self, name: &str) -> Option<&Adapter> {
        self.adapters.get(name)
    }

    pub fn writers(&self) -> impl Iterator<Item = &Adapter> {
        self.adapters
            .values()
            .filter(|adapter| adapter.manifest.writes_data)
    }

    pub fn error_for(&self, name: &str) -> Option<&LoadError> {
        self.errors
            .iter()
            .find(|error| error.path.parent().and_then(Path::file_name) == Some(name.as_ref()))
    }

    fn push_error(&mut self, path: &Path, error: String) {
        self.errors.push(LoadError {
            path: path.to_path_buf(),
            error,
        });
    }
}

fn load_adapter(dir: &Path) -> Result<Adapter, String> {
    let folder = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let text =
        std::fs::read_to_string(dir.join(MANIFEST_FILE)).map_err(|error| error.to_string())?;
    let manifest = Manifest::parse(&text, folder).map_err(|error| error.to_string())?;
    Ok(Adapter {
        manifest,
        dir: dir.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_adapter(root: &Path, folder: &str, text: &str) {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANIFEST_FILE), text).unwrap();
    }

    #[test]
    fn reports_bad_and_missing_manifests_next_to_good_ones() {
        let root = tempfile::tempdir().unwrap();
        write_adapter(
            root.path(),
            "good",
            "name = \"good\"\nrun = \"./a\"\n[actions.verify]\n",
        );
        write_adapter(root.path(), "bad", "name = \"bad\"\nrun = \"./a\"\n");
        std::fs::create_dir(root.path().join("empty")).unwrap();

        let catalog = Catalog::load(root.path());

        assert_eq!(catalog.adapters.keys().collect::<Vec<_>>(), ["good"]);
        assert_eq!(catalog.errors.len(), 2);
        assert!(
            catalog
                .error_for("bad")
                .unwrap()
                .error
                .contains("at least one action")
        );
        assert!(catalog.error_for("empty").is_some());
    }

    #[test]
    fn missing_adapters_dir_is_an_empty_catalog() {
        let catalog = Catalog::load(Path::new("/nonexistent/upkeep/adapters"));
        assert!(catalog.adapters.is_empty() && catalog.errors.is_empty());
    }
}
