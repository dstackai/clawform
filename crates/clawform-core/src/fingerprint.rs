use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};

pub fn hash_str(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn hash_json<T: Serialize>(value: &T) -> Result<String> {
    let serialized = serde_json::to_vec(value)?;
    let mut hasher = Sha256::new();
    hasher.update(serialized);
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn hash_file_or_missing(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok("__MISSING__".to_string());
    }

    let bytes = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn hash_files(paths: &[std::path::PathBuf], root: &Path) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for rel in paths {
        let abs = root.join(rel);
        out.insert(
            rel.to_string_lossy().replace('\\', "/"),
            hash_file_or_missing(&abs)?,
        );
    }
    Ok(out)
}
