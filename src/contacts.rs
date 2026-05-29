//! Persistent address book: a `name -> onion` map so users dial friends by memorable names
//! instead of pasting 56-character `.onion` addresses every time.
//!
//! Stored as JSON next to the identity key (default `$HOME/.config/hop6/contacts.json`,
//! overridable with `$HOP6_CONTACTS`). The file is your social graph — who you talk to — which
//! is sensitive metadata, so it is written `0600` in a `0700` directory like the identity key.
//!
//! This module is the *only* place contacts touch the filesystem; [`crate::app::App`] keeps the
//! map in memory and signals [`crate::app::AppAction::PersistContacts`] when it should be saved.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// `name -> bare onion service id` (without the `.onion` suffix).
pub type Book = HashMap<String, String>;

/// Resolve the contacts file path: `$HOP6_CONTACTS` if set, else
/// `$HOME/.config/hop6/contacts.json`.
pub fn path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("HOP6_CONTACTS") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("$HOME is not set — set $HOP6_CONTACTS to choose a contacts file"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("hop6")
        .join("contacts.json"))
}

/// Load the address book. A missing file yields an empty book; a corrupt file is an error.
pub fn load(path: &Path) -> Result<Book> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("contacts file {} is not valid JSON", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Book::new()),
        Err(e) => Err(anyhow::Error::from(e)
            .context(format!("reading contacts file {}", path.display()))),
    }
}

/// Persist the address book as an owner-only file (`0600` in a `0700` dir) — it reveals your
/// social graph.
pub fn save(path: &Path, book: &Book) -> Result<()> {
    let json = serde_json::to_vec_pretty(book).context("serializing contacts")?;
    crate::fsutil::write_private(path, &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_missing_file_is_empty() {
        let path = std::env::temp_dir().join("hop6_test_contacts.json");
        let _ = fs::remove_file(&path);

        // Missing file → empty book.
        assert!(load(&path).unwrap().is_empty());

        let mut book = Book::new();
        book.insert("alice".to_string(), "abc".to_string());
        save(&path, &book).unwrap();

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.get("alice").map(String::as_str), Some("abc"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_is_rejected() {
        let path = std::env::temp_dir().join("hop6_test_contacts_corrupt.json");
        fs::write(&path, b"not json").unwrap();
        assert!(load(&path).is_err());
        let _ = fs::remove_file(&path);
    }
}
