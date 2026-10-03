//! Shared protocol and rules that do not depend on the desktop toolkit.

use std::path::{Path, PathBuf};

pub const BUS_NAME: &str = "io.github.onedrive_linux.Daemon";
pub const OBJECT_PATH: &str = "/io/github/onedrive_linux/Daemon";
pub const INTERFACE_NAME: &str = "io.github.onedrive_linux.Daemon";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    NeedsLogin,
    Error,
    Offline,
    Paused,
    Transferring,
    Checking,
    Synced,
}

impl EngineState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NeedsLogin => "needs-login",
            Self::Error => "error",
            Self::Offline => "offline",
            Self::Paused => "paused",
            Self::Transferring => "transferring",
            Self::Checking => "checking",
            Self::Synced => "synced",
        }
    }

    pub const fn priority(self) -> u8 {
        match self {
            Self::NeedsLogin | Self::Error => 1,
            Self::Offline => 2,
            Self::Paused => 3,
            Self::Transferring => 4,
            Self::Checking => 5,
            Self::Synced => 6,
        }
    }
}

/// Select the most urgent state for the window and tray.
pub fn overall_state(states: impl IntoIterator<Item = EngineState>) -> EngineState {
    states
        .into_iter()
        .min_by_key(|state| state.priority())
        .unwrap_or(EngineState::NeedsLogin)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    Overlap(PathBuf, PathBuf),
    Io(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "path must not be empty"),
            Self::Overlap(a, b) => {
                write!(f, "paths overlap: {} and {}", a.display(), b.display())
            }
            Self::Io(message) => write!(f, "cannot resolve path: {message}"),
        }
    }
}

impl std::error::Error for PathError {}

/// Resolve existing ancestors so symlinks cannot hide an overlap. Paths that do
/// not exist yet are resolved against their nearest existing ancestor.
pub fn resolve_path(path: &Path) -> Result<PathBuf, PathError> {
    if path.as_os_str().is_empty() {
        return Err(PathError::Empty);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| PathError::Io(error.to_string()))?
            .join(path)
    };
    let mut missing = Vec::new();
    let mut ancestor = absolute.as_path();
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .ok_or_else(|| PathError::Io("no existing ancestor".into()))?;
        missing.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| PathError::Io("no existing ancestor".into()))?;
    }
    let mut resolved = ancestor
        .canonicalize()
        .map_err(|error| PathError::Io(error.to_string()))?;
    for part in missing.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

pub fn validate_distinct_paths(paths: &[&Path]) -> Result<(), PathError> {
    let resolved = paths
        .iter()
        .map(|path| resolve_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    for (index, first) in resolved.iter().enumerate() {
        for second in &resolved[index + 1..] {
            if first.starts_with(second) || second.starts_with(first) {
                return Err(PathError::Overlap(first.clone(), second.clone()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn urgent_state_wins_independently_of_input_order() {
        assert_eq!(
            overall_state([
                EngineState::Synced,
                EngineState::Offline,
                EngineState::Checking
            ]),
            EngineState::Offline
        );
        assert_eq!(
            overall_state([EngineState::Paused, EngineState::Transferring]),
            EngineState::Paused
        );
        assert_eq!(overall_state([]), EngineState::NeedsLogin);
    }

    #[test]
    fn path_validation_rejects_nested_and_equal_paths() {
        let root = std::env::temp_dir().join(format!(
            "onedrive-path-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let sync = root.join("sync");
        let cache = root.join("cache");
        assert!(validate_distinct_paths(&[&sync, &cache]).is_ok());
        assert!(matches!(
            validate_distinct_paths(&[&sync, &sync.join("mount")]),
            Err(PathError::Overlap(_, _))
        ));
        assert!(matches!(
            validate_distinct_paths(&[&sync, &sync]),
            Err(PathError::Overlap(_, _))
        ));
        fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn path_validation_resolves_symlinked_ancestor() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "onedrive-link-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("real")).unwrap();
        symlink(root.join("real"), root.join("alias")).unwrap();
        assert!(matches!(
            validate_distinct_paths(&[&root.join("real/sync"), &root.join("alias/sync/cache")]),
            Err(PathError::Overlap(_, _))
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
