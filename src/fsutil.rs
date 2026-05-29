//! Small filesystem helper shared by the modules that persist secrets (the identity key in
//! [`crate::tor`] and the address book in [`crate::contacts`]).

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Write `bytes` to `path` as an **owner-only** file (`0600`), creating the parent directory
/// (`0700`) if needed. The permission bits are enforced on Unix and best-effort elsewhere.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)
                .with_context(|| format!("creating directory {}", dir.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
            }
        }
    }

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(bytes)?;
    Ok(())
}
