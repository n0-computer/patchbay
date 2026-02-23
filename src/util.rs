/// Normalizes strings for filenames and environment variable suffixes.
fn sanitize_with(name: &str, allow_dash: bool) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || (allow_dash && c == '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Produces a safe path component (alphanumeric, `_`, and `-`).
pub fn sanitize_for_path_component(name: &str) -> String {
    sanitize_with(name, true)
}

/// Produces a safe environment variable suffix (alphanumeric + `_`).
pub fn sanitize_for_env_key(name: &str) -> String {
    sanitize_with(name, false)
}
