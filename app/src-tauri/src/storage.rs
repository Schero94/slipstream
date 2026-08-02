use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StorageDecision {
    admitted: bool,
    placement_ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageReport {
    pub path: String,
    pub resolved_path: String,
    pub role: String,
    pub exists: bool,
    pub is_symlink: bool,
    pub available_bytes: u64,
    pub planned_bytes: u64,
    pub reserve_bytes: u64,
    pub internal: Option<bool>,
    pub solid_state: Option<bool>,
    pub admitted: bool,
    pub placement_ok: bool,
    pub detail: String,
}

fn decide_storage(
    available_bytes: u64,
    planned_bytes: u64,
    reserve_bytes: u64,
    role: &str,
    internal: Option<bool>,
) -> StorageDecision {
    let admitted = planned_bytes
        .checked_add(reserve_bytes)
        .is_some_and(|required| required <= available_bytes);
    let placement_ok = !(role == "pgrn" && internal == Some(false));
    StorageDecision {
        admitted,
        placement_ok,
    }
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.exists() {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

#[derive(Debug)]
struct FilesystemFacts {
    device: String,
    available_bytes: u64,
}

fn filesystem_facts(path: &Path) -> Result<FilesystemFacts, String> {
    let output = Command::new("/bin/df")
        .arg("-Pk")
        .arg(path)
        .output()
        .map_err(|e| format!("df failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "df failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .next_back()
        .ok_or("df returned no filesystem row")?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 6 {
        return Err(format!("unexpected df output: {line}"));
    }
    let available_kib = fields[3]
        .parse::<u64>()
        .map_err(|e| format!("invalid df available bytes: {e}"))?;
    Ok(FilesystemFacts {
        device: fields[0].to_string(),
        available_bytes: available_kib.saturating_mul(1024),
    })
}

#[cfg(target_os = "macos")]
fn device_traits(device: &str) -> (Option<bool>, Option<bool>) {
    let output = match Command::new("/usr/sbin/diskutil")
        .args(["info", "-plist", device])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return (None, None),
    };
    let value = match plist::Value::from_reader_xml(output.stdout.as_slice()) {
        Ok(value) => value,
        Err(_) => return (None, None),
    };
    let Some(dict) = value.as_dictionary() else {
        return (None, None);
    };
    (
        dict.get("Internal").and_then(plist::Value::as_boolean),
        dict.get("SolidState").and_then(plist::Value::as_boolean),
    )
}

#[cfg(not(target_os = "macos"))]
fn device_traits(_device: &str) -> (Option<bool>, Option<bool>) {
    // Linux filesystems can sit on dm-crypt, LVM, RAID, network storage, or a
    // container overlay. Until that mapping is implemented, stay honest.
    (None, None)
}

pub fn inspect_storage(
    path: &Path,
    role: &str,
    planned_bytes: u64,
    reserve_bytes: u64,
) -> Result<StorageReport, String> {
    if path.as_os_str().is_empty() {
        return Err("storage path is empty".into());
    }
    if !matches!(role, "model" | "pgrn") {
        return Err(format!("unsupported storage role: {role}"));
    }

    let link_metadata = std::fs::symlink_metadata(path);
    let exists = link_metadata.is_ok();
    let is_symlink = link_metadata
        .as_ref()
        .is_ok_and(|meta| meta.file_type().is_symlink());
    let anchor = nearest_existing_ancestor(path)
        .ok_or_else(|| format!("no existing ancestor for {}", path.display()))?;
    let resolved = if exists {
        path.canonicalize()
            .map_err(|e| format!("resolve {}: {e}", path.display()))?
    } else {
        anchor
            .canonicalize()
            .map_err(|e| format!("resolve {}: {e}", anchor.display()))?
    };
    let facts = filesystem_facts(&resolved)?;
    let (internal, solid_state) = device_traits(&facts.device);
    let decision = decide_storage(
        facts.available_bytes,
        planned_bytes,
        reserve_bytes,
        role,
        internal,
    );
    let detail = if !decision.admitted {
        "insufficient disk headroom for planned bytes plus reserve"
    } else if !decision.placement_ok {
        "streamed PGRN is on an external device"
    } else if internal.is_none() {
        "device type unavailable; internal placement is not claimed"
    } else if internal == Some(true) && solid_state == Some(true) {
        "internal solid-state storage"
    } else if role == "model" && internal == Some(false) {
        "external load-once model storage is allowed"
    } else {
        "storage admitted"
    };

    Ok(StorageReport {
        path: path.display().to_string(),
        resolved_path: resolved.display().to_string(),
        role: role.to_string(),
        exists,
        is_symlink,
        available_bytes: facts.available_bytes,
        planned_bytes,
        reserve_bytes,
        internal,
        solid_state,
        admitted: decision.admitted,
        placement_ok: decision.placement_ok,
        detail: detail.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn admits_only_when_plan_and_reserve_fit() {
        let exact = decide_storage(10, 7, 3, "model", Some(true));
        assert!(exact.admitted);
        let short = decide_storage(9, 7, 3, "model", Some(true));
        assert!(!short.admitted);
    }

    #[test]
    fn external_pgrn_is_a_placement_warning_not_a_space_refusal() {
        let d = decide_storage(100, 10, 10, "pgrn", Some(false));
        assert!(d.admitted);
        assert!(!d.placement_ok);
    }

    #[test]
    fn external_load_once_model_is_allowed() {
        let d = decide_storage(100, 10, 10, "model", Some(false));
        assert!(d.admitted);
        assert!(d.placement_ok);
    }

    #[test]
    fn unknown_device_never_claims_external_or_internal() {
        let d = decide_storage(100, 10, 10, "pgrn", None);
        assert!(d.admitted);
        assert!(d.placement_ok);
    }

    #[test]
    fn finds_existing_parent_for_a_planned_output() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("slipstream-storage-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let planned = root.join("models/new/model.pgrn");
        assert_eq!(nearest_existing_ancestor(&planned), Some(root.clone()));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn reports_symlink_without_treating_its_name_as_device_evidence() {
        use std::os::unix::fs::symlink;

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "slipstream-storage-link-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target");
        fs::write(&target, b"pgrn").unwrap();
        let link = root.join("internal-looking.pgrn");
        symlink(&target, &link).unwrap();
        let report = inspect_storage(&link, "pgrn", 0, 0).unwrap();
        assert!(report.is_symlink);
        assert_eq!(
            report.resolved_path,
            target.canonicalize().unwrap().display().to_string()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
