use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize)]
pub struct VolumeState {
    pub volume: u8,
    pub muted: bool,
}

impl Default for VolumeState {
    fn default() -> Self {
        Self {
            volume: 100,
            muted: false,
        }
    }
}

pub fn load(path: &Path) -> VolumeState {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!("no volume state file at {}, using defaults", path.display());
            return VolumeState::default();
        }
        Err(e) => {
            warn!(
                "failed to read volume state from {}: {e}, using defaults",
                path.display()
            );
            return VolumeState::default();
        }
    };

    match serde_json::from_str::<VolumeState>(&data) {
        Ok(mut vs) => {
            if vs.volume > 100 {
                warn!("clamping restored volume {} to 100", vs.volume);
                vs.volume = 100;
            }
            vs
        }
        Err(e) => {
            warn!(
                "malformed volume state in {}: {e}, using defaults",
                path.display()
            );
            VolumeState::default()
        }
    }
}

pub fn save(path: &Path, volume: u8, muted: bool) {
    let state = VolumeState { volume, muted };
    let data = match serde_json::to_string(&state) {
        Ok(d) => d,
        Err(e) => {
            warn!("failed to serialize volume state: {e}");
            return;
        }
    };

    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, &data) {
        warn!("failed to write volume state to {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        warn!(
            "failed to rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("spin2dante_test_{name}"))
    }

    #[test]
    fn missing_file_returns_defaults() {
        let path = test_path("missing");
        let _ = std::fs::remove_file(&path);
        let vs = load(&path);
        assert_eq!(vs.volume, 100);
        assert!(!vs.muted);
    }

    #[test]
    fn round_trip() {
        let path = test_path("round_trip");
        save(&path, 42, true);
        let vs = load(&path);
        assert_eq!(vs.volume, 42);
        assert!(vs.muted);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_json_returns_defaults() {
        let path = test_path("malformed");
        std::fs::write(&path, "not json at all").unwrap();
        let vs = load(&path);
        assert_eq!(vs.volume, 100);
        assert!(!vs.muted);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn volume_clamped_to_100() {
        let path = test_path("clamp");
        std::fs::write(&path, r#"{"volume":150,"muted":false}"#).unwrap();
        let vs = load(&path);
        assert_eq!(vs.volume, 100);
        assert!(!vs.muted);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_file_returns_defaults() {
        let path = test_path("empty");
        std::fs::write(&path, "").unwrap();
        let vs = load(&path);
        assert_eq!(vs.volume, 100);
        assert!(!vs.muted);
        let _ = std::fs::remove_file(&path);
    }
}
