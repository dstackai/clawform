use std::path::Path;

pub fn to_slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
