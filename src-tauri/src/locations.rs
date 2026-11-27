use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SavedLocation {
    Portable(PathBuf),
    Absolute(PathBuf),
}

impl SavedLocation {
    pub fn new(path: &Path, root: &Path) -> Result<Self> {
        if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
            return Err(AppError::invalid(
                "settings",
                "Choose an absolute path without parent directory segments.",
            ));
        }
        let path = if path.exists() {
            let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_owned());
            // Resolve links before deciding whether a location travels with the portable folder.
            if canonical.starts_with(root) {
                canonical
            } else {
                path.to_owned()
            }
        } else {
            path.to_owned()
        };
        Ok(match path.strip_prefix(root) {
            Ok(relative) => Self::Portable(relative.to_owned()),
            Err(_) => Self::Absolute(path),
        })
    }

    pub fn resolve(&self, root: &Path) -> PathBuf {
        match self {
            Self::Portable(path) => root.join(path),
            Self::Absolute(path) => path.clone(),
        }
    }
}
