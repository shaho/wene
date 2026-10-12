//! Moving files to the system trash.
//!
//! The trash is the safety net for culling, so nothing here ever
//! deletes for real: a file that cannot be trashed (read-only volume,
//! a share with no trash, permissions) stays exactly where it is and
//! is reported back to the caller.

use std::path::PathBuf;

use objc2_foundation::{NSFileManager, NSURL};

/// What one run of the trash did: where each file landed inside the
/// trash, and which files would not go.
pub struct Trashed {
    /// Original path and the path it now has in the trash. The trash
    /// renames on a collision, so the reported URL is the only handle
    /// that can bring a file back.
    pub moved: Vec<(PathBuf, PathBuf)>,
    pub failed: Vec<PathBuf>,
}

/// Move each path to the trash.
pub fn move_to_trash(paths: &[PathBuf]) -> Trashed {
    let manager = NSFileManager::defaultManager();
    let mut result = Trashed {
        moved: Vec::with_capacity(paths.len()),
        failed: Vec::new(),
    };
    for path in paths {
        let Some(url) = NSURL::from_file_path(path) else {
            result.failed.push(path.clone());
            continue;
        };
        let mut landed: Option<objc2::rc::Retained<NSURL>> = None;
        match manager.trashItemAtURL_resultingItemURL_error(&url, Some(&mut landed)) {
            Ok(()) => {
                let landed = landed.and_then(|url| url.to_file_path());
                match landed {
                    Some(landed) => result.moved.push((path.clone(), landed)),
                    // Trashed, but nowhere we can point at; it counts
                    // as gone, and it cannot be brought back.
                    None => result.moved.push((path.clone(), PathBuf::new())),
                }
            }
            Err(error) => {
                eprintln!(
                    "trash failed: {}: {}",
                    path.display(),
                    error.localizedDescription()
                );
                result.failed.push(path.clone());
            }
        }
    }
    result
}
