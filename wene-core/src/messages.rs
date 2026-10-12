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

/// What the status bar and the slide overlay say after a move or a
/// copy. A single file is named by the name it has now, which is the
/// only record of a rename.
pub fn transfer_message(
    kind: Transfer,
    done: &[(PathBuf, PathBuf)],
    folder: &Path,
    renamed: usize,
    failed: usize,
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
    if failed > 0 {
        text.push_str(&format!(", {failed} failed"));
    }
    text
}

/// What the status bar and the slide overlay say after a delete.
pub fn trash_message(moved: &[PathBuf], failed: usize) -> String {
    let total = moved.len() + failed;
    if moved.is_empty() {
        return format!("{failed} of {total} could not be moved to trash");
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
    if failed == 0 {
        moved_text
    } else {
        format!("{moved_text} — {failed} of {total} could not be moved to trash")
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
            transfer_message(Transfer::Move, &one, folder, 1, 0),
            "Moved a 2.heic to Keep, 1 renamed"
        );
        let two = vec![
            (PathBuf::from("/a/a.heic"), PathBuf::from("/x/Keep/a.heic")),
            (PathBuf::from("/a/b.heic"), PathBuf::from("/x/Keep/b.heic")),
        ];
        assert_eq!(
            transfer_message(Transfer::Copy, &two, folder, 0, 1),
            "Copied 2 images to Keep, 1 failed"
        );
        assert_eq!(
            transfer_message(Transfer::Move, &[], folder, 0, 2),
            "Nothing moved to Keep, 2 failed"
        );
    }

    #[test]
    fn a_delete_names_three_files_and_counts_the_rest() {
        assert_eq!(
            trash_message(&paths(&["a.heic"]), 0),
            "Moved to trash: a.heic"
        );
        assert_eq!(
            trash_message(&paths(&["a", "b", "c", "d", "e"]), 0),
            "Moved to trash: a, b, c and 2 more"
        );
        assert_eq!(
            trash_message(&paths(&["a"]), 1),
            "Moved to trash: a — 1 of 2 could not be moved to trash"
        );
        assert_eq!(trash_message(&[], 3), "3 of 3 could not be moved to trash");
    }
}
