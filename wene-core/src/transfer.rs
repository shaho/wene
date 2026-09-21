//! Moving and copying image files to a target folder.
//!
//! Nothing is ever replaced. A name already taken in the target
//! folder gets Finder's treatment: "IMG_1234.heic" arrives as
//! "IMG_1234 2.heic". The new name is claimed by creating the file
//! first, so two batches running at once cannot pick the same one.
//! A move across disks is a copy and then a delete, which is why a
//! batch runs off the main thread.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

/// rename(2) across mount points. `ErrorKind::CrossesDevices` is
/// still unstable, so the raw number stands in for it.
const EXDEV: i32 = 18;

/// Move or copy: the same job apart from what happens to the
/// original.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    Move,
    Copy,
}

/// What one file's transfer did.
pub struct Transferred {
    /// Where the file ended up.
    pub target: PathBuf,
    /// True when the target folder already held that name.
    pub renamed: bool,
}

impl Transfer {
    /// "Moving" or "Copying", for a line reporting work in progress.
    pub fn present(self) -> &'static str {
        match self {
            Transfer::Move => "Moving",
            Transfer::Copy => "Copying",
        }
    }

    /// "Moved" or "Copied", for a line reporting work that is done.
    pub fn past(self) -> &'static str {
        match self {
            Transfer::Move => "Moved",
            Transfer::Copy => "Copied",
        }
    }

    /// Send one file to `folder`, keeping both files on a collision.
    pub fn apply(self, source: &Path, folder: &Path) -> std::io::Result<Transferred> {
        // A move into the folder the file is already in is nothing at
        // all. Without this it would rename the file beside itself.
        if self == Transfer::Move && source.parent() == Some(folder) {
            return Ok(Transferred {
                target: source.to_path_buf(),
                renamed: false,
            });
        }
        let (target, renamed) = claim_name(source, folder)?;
        let result = match self {
            Transfer::Move => move_onto(source, &target),
            Transfer::Copy => std::fs::copy(source, &target).map(|_| ()),
        };
        match result {
            Ok(()) => Ok(Transferred { target, renamed }),
            Err(error) => {
                // The claim was ours, so take it back.
                let _ = std::fs::remove_file(&target);
                Err(error)
            }
        }
    }
}

/// rename replaces the claimed file. Another disk cannot be renamed
/// onto, so the file is copied and then deleted.
fn move_onto(source: &Path, target: &Path) -> std::io::Result<()> {
    match std::fs::rename(source, target) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(EXDEV) => {
            std::fs::copy(source, target)?;
            std::fs::remove_file(source)
        }
        Err(error) => Err(error),
    }
}

/// Claim a free name in `folder` by creating the file, and say
/// whether the file's own name was taken. The caller writes over the
/// claim, or removes it if the transfer fails.
fn claim_name(source: &Path, folder: &Path) -> std::io::Result<(PathBuf, bool)> {
    let name = source.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source has no file name")
    })?;
    if claim(&folder.join(name))? {
        return Ok((folder.join(name), false));
    }
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = source
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    // Finder counts from 2 and keeps counting.
    for copy in 2..1000 {
        let candidate = folder.join(format!("{stem} {copy}{extension}"));
        if claim(&candidate)? {
            return Ok((candidate, true));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free name in the target folder",
    ))
}

/// Create `path` and nothing else. False means something already
/// holds that name, a dangling symlink included.
fn claim(path: &Path) -> std::io::Result<bool> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder of this test's own.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wene-transfer-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn move_takes_the_file_across() {
        let from = scratch("move-from");
        let to = scratch("move-to");
        let source = from.join("a.heic");
        write(&source, "one");

        let done = Transfer::Move.apply(&source, &to).unwrap();

        assert_eq!(done.target, to.join("a.heic"));
        assert!(!done.renamed);
        assert!(!source.exists());
        assert_eq!(std::fs::read_to_string(done.target).unwrap(), "one");
    }

    #[test]
    fn copy_leaves_the_original() {
        let from = scratch("copy-from");
        let to = scratch("copy-to");
        let source = from.join("a.heic");
        write(&source, "one");

        let done = Transfer::Copy.apply(&source, &to).unwrap();

        assert!(source.exists());
        assert_eq!(std::fs::read_to_string(done.target).unwrap(), "one");
    }

    #[test]
    fn a_taken_name_keeps_both_files() {
        let from = scratch("clash-from");
        let to = scratch("clash-to");
        write(&to.join("a.heic"), "theirs");
        let source = from.join("a.heic");
        write(&source, "mine");

        let done = Transfer::Move.apply(&source, &to).unwrap();

        assert!(done.renamed);
        assert_eq!(done.target, to.join("a 2.heic"));
        assert_eq!(std::fs::read_to_string(to.join("a.heic")).unwrap(), "theirs");
        assert_eq!(std::fs::read_to_string(to.join("a 2.heic")).unwrap(), "mine");
    }

    #[test]
    fn the_counter_keeps_counting() {
        let from = scratch("count-from");
        let to = scratch("count-to");
        write(&to.join("a.heic"), "one");
        write(&to.join("a 2.heic"), "two");
        let source = from.join("a.heic");
        write(&source, "three");

        let done = Transfer::Copy.apply(&source, &to).unwrap();

        assert_eq!(done.target, to.join("a 3.heic"));
    }

    #[test]
    fn a_file_with_no_extension_still_gets_a_number() {
        let from = scratch("bare-from");
        let to = scratch("bare-to");
        write(&to.join("a"), "theirs");
        let source = from.join("a");
        write(&source, "mine");

        let done = Transfer::Copy.apply(&source, &to).unwrap();

        assert_eq!(done.target, to.join("a 2"));
    }

    #[test]
    fn moving_a_file_into_its_own_folder_changes_nothing() {
        let here = scratch("same-folder");
        let source = here.join("a.heic");
        write(&source, "one");

        let done = Transfer::Move.apply(&source, &here).unwrap();

        assert_eq!(done.target, source);
        assert!(!done.renamed);
        assert_eq!(std::fs::read_dir(&here).unwrap().count(), 1);
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "one");
    }

    #[test]
    fn a_failed_transfer_leaves_no_empty_file_behind() {
        let from = scratch("fail-from");
        let to = scratch("fail-to");
        let missing = from.join("gone.heic");

        assert!(Transfer::Copy.apply(&missing, &to).is_err());
        assert!(!to.join("gone.heic").exists());
    }
}
