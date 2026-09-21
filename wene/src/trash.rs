//! Moving files to the system trash.
//!
//! The trash is the safety net for culling, so nothing here ever
//! deletes for real: a file that cannot be trashed (read-only volume,
//! a share with no trash, permissions) stays exactly where it is and
//! is reported back to the caller.

use std::path::{Path, PathBuf};

use objc2_foundation::{NSFileManager, NSString, NSURL};

/// Move each path to the trash. Returns the paths that failed, in the
/// order they were given.
pub fn move_to_trash(paths: &[PathBuf]) -> Vec<PathBuf> {
    let manager = NSFileManager::defaultManager();
    paths
        .iter()
        .filter(|path| {
            let url = url_for(path);
            match manager.trashItemAtURL_resultingItemURL_error(&url, None) {
                Ok(()) => false,
                Err(error) => {
                    eprintln!("trash failed: {}: {}", path.display(), error.localizedDescription());
                    true
                }
            }
        })
        .cloned()
        .collect()
}

fn url_for(path: &Path) -> objc2::rc::Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}
