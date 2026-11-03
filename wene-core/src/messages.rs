//! What the status bar and the slide overlay say. Plain text from
//! plain values, so the shell only shows it.

use std::path::{Path, PathBuf};

use crate::transfer::Transfer;

/// A size for people: decimal units, as Finder counts them.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A folder's own name, for menu titles and status lines.
pub fn folder_label(folder: &Path) -> String {
    folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| folder.to_string_lossy().into_owned())
}

/// An OS error, for people: "Permission denied (os error 13)" loses
/// its number, and a sentence from the system loses its full stop.
pub fn reason(error: &str) -> String {
    let text = match error.find(" (os error") {
        Some(at) => &error[..at],
        None => error,
    };
    text.trim_end_matches('.').to_owned()
}

/// ", 2 failed: Permission denied". The first reason stands for the
/// batch: one failure usually explains the rest.
fn failed_suffix(failed: &[String]) -> String {
    match failed {
        [] => String::new(),
        [first, ..] => format!(", {} failed: {first}", failed.len()),
    }
}

/// What the status bar and the slide overlay say after a move or a
/// copy. A single file is named by the name it has now, which is the
/// only record of a rename. `failed` holds one reason per file that
/// would not go.
pub fn transfer_message(
    kind: Transfer,
    done: &[(PathBuf, PathBuf)],
    folder: &Path,
    renamed: usize,
    failed: &[String],
) -> String {
    let verb = kind.past();
    let target = folder_label(folder);
    let mut text = match done {
        [] => format!("Nothing {} to {target}", verb.to_lowercase()),
        [(_, one)] => format!(
            "{verb} {} to {target}",
            one.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
        many => format!("{verb} {} images to {target}", many.len()),
    };
    if renamed > 0 {
        text.push_str(&format!(", {renamed} renamed"));
    }
    text.push_str(&failed_suffix(failed));
    text
}

/// What the status bar and the slide overlay say after an undo or a
/// redo that could not put every file back.
pub fn put_back_message(total: usize, failed: &[String]) -> String {
    match failed {
        [] => String::new(),
        [first, ..] => format!("{} of {total} could not be put back: {first}", failed.len()),
    }
}

/// What the status bar and the slide overlay say after a delete.
/// `failed` holds one reason per file the trash would not take.
pub fn trash_message(moved: &[PathBuf], failed: &[String]) -> String {
    let total = moved.len() + failed.len();
    let refused = |first: &str| {
        format!(
            "{} of {total} could not be moved to trash: {first}",
            failed.len()
        )
    };
    if moved.is_empty() {
        return refused(failed.first().map(String::as_str).unwrap_or_default());
    }
    // The line is the only record of what went, so it names files,
    // not just a count.
    let names: Vec<String> = moved
        .iter()
        .take(3)
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();
    let mut moved_text = format!("Moved to trash: {}", names.join(", "));
    if moved.len() > names.len() {
        moved_text.push_str(&format!(" and {} more", moved.len() - names.len()));
    }
    match failed.first() {
        None => moved_text,
        Some(first) => format!("{moved_text} — {}", refused(first)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from(format!("/x/{n}")))
            .collect()
    }

    #[test]
    fn sizes_use_decimal_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1000), "1.0 KB");
        assert_eq!(format_bytes(1_500_000), "1.5 MB");
        assert_eq!(format_bytes(2_000_000_000_000_000), "2000.0 TB");
    }

    #[test]
    fn a_folder_is_named_by_its_own_name() {
        assert_eq!(folder_label(Path::new("/Users/me/Pictures")), "Pictures");
        assert_eq!(folder_label(Path::new("/")), "/");
    }

    #[test]
    fn a_transfer_names_one_file_and_counts_many() {
        let folder = Path::new("/x/Keep");
        let one = vec![(
            PathBuf::from("/a/a.heic"),
            PathBuf::from("/x/Keep/a 2.heic"),
        )];
        assert_eq!(
            transfer_message(Transfer::Move, &one, folder, 1, &[]),
            "Moved a 2.heic to Keep, 1 renamed"
        );
        let two = vec![
            (PathBuf::from("/a/a.heic"), PathBuf::from("/x/Keep/a.heic")),
            (PathBuf::from("/a/b.heic"), PathBuf::from("/x/Keep/b.heic")),
        ];
        let denied = ["Permission denied".to_owned()];
        assert_eq!(
            transfer_message(Transfer::Copy, &two, folder, 0, &denied),
            "Copied 2 images to Keep, 1 failed: Permission denied"
        );
        let two_reasons = ["Read-only file system".to_owned(), "No space".to_owned()];
        assert_eq!(
            transfer_message(Transfer::Move, &[], folder, 0, &two_reasons),
            "Nothing moved to Keep, 2 failed: Read-only file system"
        );
    }

    #[test]
    fn a_reason_loses_the_error_number_and_the_full_stop() {
        assert_eq!(
            reason("Permission denied (os error 13)"),
            "Permission denied"
        );
        assert_eq!(
            reason("The file couldn’t be moved because you don’t have permission."),
            "The file couldn’t be moved because you don’t have permission"
        );
        assert_eq!(reason("No space"), "No space");
    }

    #[test]
    fn an_undo_that_falls_short_says_why() {
        assert_eq!(put_back_message(3, &[]), "");
        assert_eq!(
            put_back_message(3, &["another file now has that name".to_owned()]),
            "1 of 3 could not be put back: another file now has that name"
        );
    }

    #[test]
    fn a_delete_names_three_files_and_counts_the_rest() {
        assert_eq!(
            trash_message(&paths(&["a.heic"]), &[]),
            "Moved to trash: a.heic"
        );
        assert_eq!(
            trash_message(&paths(&["a", "b", "c", "d", "e"]), &[]),
            "Moved to trash: a, b, c and 2 more"
        );
        let locked = ["The file is locked".to_owned()];
        assert_eq!(
            trash_message(&paths(&["a"]), &locked),
            "Moved to trash: a — 1 of 2 could not be moved to trash: The file is locked"
        );
        let three = vec!["No trash on this volume".to_owned(); 3];
        assert_eq!(
            trash_message(&[], &three),
            "3 of 3 could not be moved to trash: No trash on this volume"
        );
    }
}
