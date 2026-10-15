use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn home() -> PathBuf {
    std::env::var_os("NANOLB_HOME").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".nanolb"),
        PathBuf::from,
    )
}

pub(crate) fn pid_path() -> PathBuf {
    home().join("nanolb.pid")
}

pub(crate) fn admin_token_path() -> PathBuf {
    home().join("admin_token")
}

pub(crate) fn admin_token() -> io::Result<String> {
    let path = admin_token_path();
    if let Ok(t) = std::fs::read_to_string(&path) {
        return Ok(t.trim().to_owned());
    }
    let t = format!("nlb-admin-{}", crate::id::random_hex(24));
    std::fs::write(&path, &t)?;
    restrict(&path, 0o600);
    Ok(t)
}

pub(crate) fn secure_permissions(dir: &Path, db: &Path) {
    restrict(dir, 0o700);
    restrict(db, 0o600);
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restrict(_: &Path, _: u32) {}
