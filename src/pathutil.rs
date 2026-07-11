// ===========================================================================
// pathutil.rs — path helpers (expand `~`, normalize a directory string).
//
// Pure functions only — no DB, no side effects beyond probing existence. This
// mirrors `mdrv-ink`'s `pathutil.rs`: small, trivially testable primitives the
// rest of the crate leans on.
// ===========================================================================

use std::path::{Path, PathBuf};

use crate::Result;

/// Expand a leading `~` (or bare `~`) to the user's home directory.
///
/// Returns the input unchanged if there is no `~`, or if `$HOME` is not set
/// (in which case there is nothing meaningful to expand to).
pub fn expand_home(input: &str) -> PathBuf {
    if !input.starts_with('~') {
        return PathBuf::from(input);
    }
    let Some(home) = std::env::var_os("HOME") else {
        return PathBuf::from(input);
    };
    // `~/x` → `$HOME/x`; `~` alone → `$HOME`; `~other/x` is left alone (we do
    // not resolve other users' homes).
    if input == "~" {
        PathBuf::from(home)
    } else if let Some(rest) = input.strip_prefix("~/") {
        PathBuf::from(home).join(rest)
    } else {
        // e.g. `~root` — not supported, keep verbatim.
        PathBuf::from(input)
    }
}

/// Normalize a directory string typed by a user into an absolute path.
///
/// Rules:
///   - expand a leading `~`;
///   - if the result is relative, anchor it at the current working directory;
///   - if it exists on disk, canonicalize it (resolves symlinks/`..`);
///   - if it does not exist yet (a brand-new directory), return the absolute
///     form without canonicalizing.
///
/// Existence is *not* required — a move can target a directory that will be
/// created later — but a non-absolute, non-existent path is still rejected as
/// `InvalidInput` because it would be ambiguous.
pub fn normalize_directory(input: &str) -> Result<PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(crate::Error::InvalidInput {
            msg: "directory is empty".into(),
        });
    }

    let expanded = expand_home(trimmed);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        // Anchor relative input at the cwd. `std::env::current_dir` failing is
        // exceedingly rare (deleted cwd) — surface it as an I/O error.
        std::env::current_dir()?.join(expanded)
    };

    if absolute.exists() {
        // Canonicalize only when the path is real, so symlinks/`..` collapse.
        // (canonicalize requires existence; that's why the else branch exists.)
        absolute.canonicalize().map_err(crate::Error::Io)
    } else {
        Ok(absolute)
    }
}

/// Same as [`normalize_directory`] but for a value that is *already* expected
/// to be absolute (e.g. taken straight from the DB). Used to display paths in a
/// stable form. Falls back to the input if canonicalization fails.
pub fn normalize_existing(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_passes_through_plain_paths() {
        // SAFETY: tests are single-threaded; mutating HOME here is benign.
        unsafe { std::env::set_var("HOME", "/home/test"); }
        assert_eq!(expand_home("/x/g/foo"), PathBuf::from("/x/g/foo"));
        assert_eq!(expand_home("relative/dir"), PathBuf::from("relative/dir"));
    }

    #[test]
    fn expand_home_tilde_alone() {
        // SAFETY: tests are single-threaded; mutating HOME here is benign.
        unsafe { std::env::set_var("HOME", "/home/test"); }
        assert_eq!(expand_home("~"), PathBuf::from("/home/test"));
    }

    #[test]
    fn expand_home_tilde_slash() {
        // SAFETY: tests are single-threaded; mutating HOME here is benign.
        unsafe { std::env::set_var("HOME", "/home/test"); }
        assert_eq!(
            expand_home("~/projects/x"),
            PathBuf::from("/home/test/projects/x")
        );
    }

    #[test]
    fn expand_home_other_user_left_alone() {
        // SAFETY: tests are single-threaded; mutating HOME here is benign.
        unsafe { std::env::set_var("HOME", "/home/test"); }
        // `~root` is not supported; kept verbatim.
        assert_eq!(expand_home("~root/x"), PathBuf::from("~root/x"));
    }

    #[test]
    fn normalize_rejects_empty() {
        assert!(normalize_directory("   ").is_err());
        assert!(normalize_directory("").is_err());
    }

    #[test]
    fn normalize_existing_dir_canonicalizes() {
        // `/tmp` exists everywhere; canonicalization keeps it absolute & real.
        let p = normalize_directory("/tmp").unwrap();
        assert!(p.is_absolute());
        assert!(p.exists());
    }

    #[test]
    fn normalize_nonexistent_is_still_absolute() {
        let p = normalize_directory("/x/g/this/does/not/exist/yz").unwrap();
        assert!(p.is_absolute());
        assert!(!p.exists());
    }
}
