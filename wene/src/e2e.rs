//! In-app end-to-end test harness. Runs when WENE_E2E=1: after the
//! scan finishes it drives the delegate directly (no synthetic OS
//! events), asserts state between steps, saves a slide snapshot, and
//! exits 0/1 with a report. Run:
//!
//!     WENE_E2E=1 cargo run -p wene -- <folder-with-3+-images>

use std::cell::RefCell;

use objc2_app_kit::{NSBitmapImageFileType, NSView};
use objc2_foundation::NSDictionary;

use crate::AppDelegate;

pub struct E2eState {
    pub step: usize,
    pub failures: Vec<String>,
    pub started: bool,
}

impl Default for E2eState {
    fn default() -> Self {
        E2eState {
            step: 0,
            failures: Vec::new(),
            started: false,
        }
    }
}

/// Minimal 1×1 two-frame animated GIF (black frame, white frame,
/// 0.1 s delays) for the animation tests.
pub const ANIMATED_GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, // GIF89a
    0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, // 1x1, global color table
    0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, // palette: black, white
    0x21, 0xF9, 0x04, 0x00, 0x0A, 0x00, 0x00, 0x00, // GCE: 0.1 s
    0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, // frame 1
    0x02, 0x02, 0x44, 0x01, 0x00, // pixel 0
    0x21, 0xF9, 0x04, 0x00, 0x0A, 0x00, 0x00, 0x00, // GCE: 0.1 s
    0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, // frame 2
    0x02, 0x02, 0x4C, 0x01, 0x00, // pixel 1
    0x3B, // trailer
];

pub fn enabled() -> bool {
    std::env::var("WENE_E2E").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn check(state: &RefCell<E2eState>, ok: bool, what: &str) {
    let mut state = state.borrow_mut();
    let step = state.step;
    if ok {
        println!("e2e step {step}: ok: {what}");
    } else {
        println!("e2e step {step}: FAIL: {what}");
        state.failures.push(format!("step {step}: {what}"));
    }
}

/// Fire a menu item through NSMenu's own action dispatch, the same
/// target and responder resolution as a user click. The item is found
/// by what it says, so a new entry above it changes nothing here.
fn perform_menu_item(menu_title: &str, item_title: &str) -> bool {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return false;
    };
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    let Some(menubar) = app.mainMenu() else { return false };
    for item in menubar.itemArray() {
        let Some(submenu) = item.submenu() else { continue };
        if submenu.title().to_string() != menu_title {
            continue;
        }
        for (index, entry) in submenu.itemArray().iter().enumerate() {
            if entry.title().to_string().starts_with(item_title) {
                submenu.performActionForItemAtIndex(index as isize);
                return true;
            }
        }
    }
    false
}

/// Name of a cull test file. The process id keeps it unique, so the
/// trash can never rename it and the cleanup can never hit a file
/// that was already there.
fn cull_name(tag: &str) -> String {
    format!("zzz-cull-{}-{tag}.heic", std::process::id())
}

/// Take a cull test file back out of the trash, so a run leaves
/// nothing behind.
fn empty_from_trash(name: &str) {
    let Ok(home) = std::env::var("HOME") else { return };
    let _ = std::fs::remove_file(format!("{home}/.Trash/{name}"));
}

/// The folder move and copy send files to, outside the fixture so a
/// recursive scan never sees them again.
fn transfer_folder() -> String {
    std::env::temp_dir()
        .join(format!("wene-e2e-transfer-{}", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// A folder of our own, to open from "Finder" and land somewhere
/// other than the fixture.
fn open_folder() -> String {
    std::env::temp_dir()
        .join(format!("wene-e2e-open-{}", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// The file the undo steps push around, named so it sorts last and
/// cannot collide with another run.
fn undo_name() -> String {
    format!("zzz-undo-{}.heic", std::process::id())
}

fn transfer_name() -> String {
    format!("zzz-transfer-{}.heic", std::process::id())
}

fn snapshot(view: &NSView, path: &str) -> bool {
    let bounds = view.bounds();
    let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(bounds) else {
        return false;
    };
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    let props = NSDictionary::new();
    let Some(data) =
        (unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) })
    else {
        return false;
    };
    data.writeToFile_atomically(&objc2_foundation::NSString::from_str(path), true)
}

/// One step per timer tick (0.9 s apart, so decodes can land).
/// Sequence: start show, verify first slide, step, verify, turn on
/// auto-advance, verify it advanced, overlay on, verify text, end
/// show, verify closed, report and exit.
pub fn run_step(delegate: &AppDelegate) {
    let state = delegate.e2e_state();
    let step = state.borrow().step;
    match step {
        0 => {
            check(state, delegate.e2e_file_count() >= 3, "scan found 3+ files");
            delegate.start_slideshow(0, false);
        }
        1 => {
            check(state, delegate.e2e_show_active(), "slideshow window active");
            check(state, delegate.e2e_current_index() == Some(0), "starts at index 0");
            check(state, delegate.e2e_slide_has_image(), "first slide decoded and shown");
        }
        2 => {
            delegate.step_slideshow(1);
            check(state, delegate.e2e_current_index() == Some(1), "arrow step to index 1");
        }
        3 => {
            check(state, delegate.e2e_slide_has_image(), "second slide shown");
            if let Some(view) = delegate.e2e_slide_view() {
                let path = std::env::temp_dir().join("wene-e2e-slide.png");
                let path = path.to_string_lossy().into_owned();
                let ok = snapshot(&view, &path);
                check(state, ok, "slide snapshot written");
                if ok {
                    println!("e2e snapshot: {path}");
                }
            }
        }
        4 => {
            // Own step: the snapshot's PNG encode blocks the main
            // thread and would delay the timer's first fire.
            delegate.set_auto_advance(Some(0.4));
        }
        5 => {} // let the timer run a full harness tick
        6 => {
            check(
                state,
                delegate.e2e_current_index().is_some_and(|i| i >= 2),
                "auto-advance moved past index 1",
            );
            delegate.set_auto_advance(None);
            delegate.toggle_overlay();
        }
        7 => {
            check(
                state,
                delegate.e2e_overlay_text().is_some_and(|t| !t.is_empty()),
                "overlay has text",
            );
            delegate.end_slideshow();
        }
        8 => {
            check(state, !delegate.e2e_show_active(), "slideshow closed");
            // Sort round-trip: size-descending puts the biggest file
            // first, then back to name order.
            delegate.e2e_sort(wene_core::SortOrder::Size, true);
            let first = delegate.e2e_first_file();
            check(
                state,
                first.as_deref() == Some("img10.heic"),
                "size-descending puts biggest first",
            );
            delegate.e2e_sort(wene_core::SortOrder::Name, false);
            let first = delegate.e2e_first_file();
            check(
                state,
                first.as_deref() == Some("apple.heic"),
                "name order restored",
            );
        }
        9 => {
            let before = delegate.e2e_grid_height();
            delegate.e2e_scale_cells(1.5);
            let after = delegate.e2e_grid_height();
            check(state, after > before, "bigger cells grow the grid");
            delegate.e2e_scale_cells(1.0 / 1.5);
        }
        10 => {
            // Multi-select: two files selected play just those two.
            delegate.e2e_select(&[1, 3]);
            delegate.e2e_start_from_selection();
            check(
                state,
                delegate.e2e_playlist_len() == Some(2),
                "selection of 2 plays exactly 2",
            );
            delegate.end_slideshow();
        }
        11 => {
            // Watcher: a new file appears on disk.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let src = format!("{root}/img2.heic");
            let dst = format!("{root}/zzz-watcher-test.heic");
            let copied = std::fs::copy(&src, &dst).is_ok();
            check(state, copied, "test file copied");
        }
        12 => {
            check(
                state,
                delegate.e2e_file_count() == 5,
                "watcher picked up the new file",
            );
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let _ = std::fs::remove_file(format!("{root}/zzz-watcher-test.heic"));
        }
        13 => {
            check(
                state,
                delegate.e2e_file_count() == 4,
                "watcher removed the deleted file",
            );
        }
        14 => {
            // Rotation + per-file state memory.
            delegate.start_slideshow(0, false);
        }
        15 => {
            if let Some(view) = delegate.e2e_slide_view() {
                view.rotate(90);
                check(state, view.state().rotation == 90, "rotate key turns 90");
            }
            delegate.step_slideshow(1);
        }
        16 => {
            if let Some(view) = delegate.e2e_slide_view() {
                check(
                    state,
                    view.state().rotation == 0,
                    "next slide starts unrotated",
                );
            }
            delegate.step_slideshow(-1);
        }
        17 => {
            if let Some(view) = delegate.e2e_slide_view() {
                check(
                    state,
                    view.state().rotation == 90,
                    "rotation remembered per file",
                );
            }
            delegate.end_slideshow();
        }
        18 => {
            check(state, !delegate.e2e_show_active(), "fullscreen show closed");
            // Windowed slideshow.
            delegate.start_slideshow(0, true);
        }
        19 => {
            check(state, delegate.e2e_show_active(), "windowed slideshow active");
            check(
                state,
                delegate.e2e_show_is_windowed() == Some(true),
                "window has a title bar",
            );
            check(state, delegate.e2e_slide_has_image(), "windowed slide shown");
            delegate.end_slideshow();
        }
        20 => {
            check(state, !delegate.e2e_show_active(), "windowed show closed");
            // Real menu dispatch: fire the Slideshow menu items the
            // way a click does (no synthetic OS events).
            check(
                state,
                perform_menu_item("Slideshow", "Start slideshow in window"),
                "menu item 'Start slideshow in window' fired",
            );
        }
        21 => {
            check(state, delegate.e2e_show_active(), "menu started windowed show");
            check(
                state,
                delegate.e2e_show_is_windowed() == Some(true),
                "menu picked windowed mode",
            );
            delegate.end_slideshow();
        }
        22 => {
            check(
                state,
                perform_menu_item("Slideshow", "Start slideshow"),
                "menu item 'Start slideshow' fired",
            );
        }
        23 => {
            check(state, delegate.e2e_show_active(), "menu started fullscreen show");
            check(
                state,
                delegate.e2e_show_is_windowed() == Some(false),
                "menu picked fullscreen mode",
            );
            delegate.end_slideshow();
        }
        24 => {
            check(state, !delegate.e2e_show_active(), "menu-started show closed");
        }
        25 => {
            // Status bar: count, then selection info.
            delegate.e2e_select(&[]);
            check(
                state,
                delegate.e2e_status_text().contains("4 images"),
                "status shows the image count",
            );
            delegate.e2e_select(&[1, 3]);
            check(
                state,
                delegate.e2e_status_text().contains("2 of 4 selected"),
                "status shows the selection",
            );
            delegate.e2e_select(&[1]);
            let text = delegate.e2e_status_text();
            check(
                state,
                text.contains("×") && text.contains("B"),
                "single selection shows dimensions and size",
            );
            delegate.e2e_select(&[]);
        }
        26 => {
            // Filename labels grow the rows.
            let before = delegate.e2e_grid_height();
            delegate.e2e_toggle_labels();
            check(
                state,
                delegate.e2e_grid_height() > before,
                "labels add row height",
            );
            delegate.e2e_toggle_labels();
        }
        28 => {
            // Animated GIF: drop one into the folder for the watcher.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let written =
                std::fs::write(format!("{root}/zzz-anim.gif"), ANIMATED_GIF).is_ok();
            check(state, written, "animated gif written");
        }
        29 => {
            check(
                state,
                delegate.e2e_file_count() == 5,
                "watcher picked up the gif",
            );
            // Name order puts zzz-anim.gif last: index 4.
            delegate.start_slideshow(4, false);
        }
        30 => {
            check(state, delegate.e2e_show_active(), "gif slideshow active");
            check(state, delegate.e2e_slide_animating(), "gif frames are playing");
            delegate.end_slideshow();
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let _ = std::fs::remove_file(format!("{root}/zzz-anim.gif"));
        }
        31 => {
            check(
                state,
                delegate.e2e_file_count() == 4,
                "gif removed after the test",
            );
        }
        32 => {
            // Sidebar: open the tree down to the fixture folder and
            // click it, the same path a user click takes.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            delegate.e2e_sidebar_click(&root);
            check(
                state,
                delegate.e2e_sidebar_path().ends_with(
                    std::path::Path::new(&root)
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap(),
                ),
                "sidebar highlights the folder it opened",
            );
        }
        33 => {
            check(
                state,
                delegate.e2e_file_count() == 4,
                "sidebar click rescanned the folder",
            );
            delegate.e2e_toggle_sidebar();
            check(state, delegate.e2e_sidebar_hidden(), "sidebar collapses");
            delegate.e2e_toggle_sidebar();
            check(state, !delegate.e2e_sidebar_hidden(), "sidebar shows again");
            // Favorites: three seeded, one added, one removed.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let before = delegate.e2e_favorite_count();
            delegate.e2e_add_favorite(&root);
            check(
                state,
                delegate.e2e_favorite_count() == before + 1,
                "folder joins the favorites",
            );
            delegate.e2e_remove_favorite(&root);
            check(
                state,
                delegate.e2e_favorite_count() == before,
                "favorite goes away again",
            );
        }
        34 => {
            // Preferences window and the default slideshow mode.
            delegate.show_prefs();
            check(state, delegate.e2e_prefs_visible(), "prefs window opens");
            delegate.e2e_set_default_windowed(true);
            check(
                state,
                delegate.default_windowed(),
                "default slideshow mode switches to windowed",
            );
            delegate.e2e_set_default_windowed(false);
            delegate.e2e_close_prefs();
            check(state, !delegate.e2e_prefs_visible(), "prefs window closes");
        }
        27 => {
            // Date sort orders: the fixture has no EXIF dates, so
            // both fall back to the modified date without breaking
            // the grid; then name order restores.
            delegate.e2e_sort(wene_core::SortOrder::ExifDate, true);
            check(state, delegate.e2e_file_count() == 4, "EXIF date sort keeps all files");
            delegate.e2e_sort(wene_core::SortOrder::Added, true);
            check(state, delegate.e2e_file_count() == 4, "date added sort keeps all files");
            delegate.e2e_sort(wene_core::SortOrder::Name, false);
            check(
                state,
                delegate.e2e_first_file().as_deref() == Some("apple.heic"),
                "name order restored after date sorts",
            );
        }
        35 => {
            // Trash from the grid: cull a copy, not a fixture file.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let copied = std::fs::copy(
                format!("{root}/img2.heic"),
                format!("{root}/{}", cull_name("a")),
            )
            .is_ok();
            check(state, copied, "cull test file copied");
        }
        36 => {
            check(state, delegate.e2e_file_count() == 5, "watcher saw the cull test file");
            // Name order puts zzz-trash-a.heic last: index 4.
            delegate.e2e_select(&[4]);
            check(
                state,
                perform_menu_item("File", "Move to trash"),
                "menu item 'Move to trash' fired",
            );
            check(state, delegate.e2e_file_count() == 4, "trashed image left the grid");
            check(
                state,
                delegate.e2e_status_text() == format!("Moved to trash: {}", cull_name("a")),
                "status bar names the trashed file",
            );
            check(
                state,
                delegate.e2e_selected_name().as_deref() == Some("img10.heic"),
                "culling the last image steps the selection back",
            );
            empty_from_trash(&cull_name("a"));
        }
        37 => {
            // A step later the watcher has echoed the app's own
            // delete. That echo must change nothing, the status line
            // included: it is the only record of what went.
            check(
                state,
                delegate.e2e_status_text() == format!("Moved to trash: {}", cull_name("a")),
                "watcher echo leaves the status line alone",
            );
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let copied = std::fs::copy(
                format!("{root}/img2.heic"),
                format!("{root}/{}", cull_name("b")),
            )
            .is_ok();
            check(state, copied, "second cull test file copied");
        }
        38 => {
            check(state, delegate.e2e_file_count() == 5, "watcher saw the second file");
            delegate.start_slideshow(4, false);
            check(
                state,
                perform_menu_item("File", "Move to trash"),
                "menu item 'Move to trash' fired with a show up",
            );
            check(state, delegate.e2e_show_active(), "slideshow stays up after a cull");
            check(
                state,
                delegate.e2e_playlist_len() == Some(4),
                "trashed slide left the playlist",
            );
            check(
                state,
                delegate
                    .e2e_overlay_text()
                    .is_some_and(|t| t == format!("Moved to trash: {}", cull_name("b"))),
                "slide overlay names the trashed file",
            );
            delegate.end_slideshow();
            empty_from_trash(&cull_name("b"));
        }
        39 => {
            check(state, delegate.e2e_file_count() == 4, "grid back to the fixture");
        }
        40 => {
            // Move and copy: a test file of our own, into a folder
            // outside the fixture.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let made = std::fs::create_dir_all(transfer_folder()).is_ok();
            let copied = std::fs::copy(
                format!("{root}/img2.heic"),
                format!("{root}/{}", transfer_name()),
            )
            .is_ok();
            check(state, made && copied, "transfer test folder and file ready");
        }
        41 => {
            check(state, delegate.e2e_file_count() == 5, "watcher saw the transfer file");
            // Name order puts the zzz file last: index 4.
            delegate.e2e_select(&[4]);
            delegate.e2e_transfer(&transfer_folder(), true);
        }
        42 => {
            check(state, !delegate.e2e_transfer_busy(), "the batch finished");
            check(state, delegate.e2e_file_count() == 4, "moved image left the grid");
            check(
                state,
                std::path::Path::new(&format!("{}/{}", transfer_folder(), transfer_name()))
                    .exists(),
                "moved image arrived in the target folder",
            );
            let folder_name = std::path::Path::new(&transfer_folder())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            check(
                state,
                delegate.e2e_status_text()
                    == format!("Moved {} to {folder_name}", transfer_name()),
                "status bar names the moved file and its folder",
            );
            check(
                state,
                delegate.e2e_repeat_item_title(true) == format!("Move again to {folder_name}"),
                "the repeat item names the last folder",
            );
            check(
                state,
                delegate.e2e_selected_name().as_deref() == Some("img10.heic"),
                "a move steps the selection like a delete",
            );
            // A second file of the same name, to collide on the move.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let _ = std::fs::copy(
                format!("{root}/img2.heic"),
                format!("{root}/{}", transfer_name()),
            );
        }
        43 => {
            check(state, delegate.e2e_file_count() == 5, "watcher saw the second file");
            delegate.e2e_select(&[4]);
            check(
                state,
                perform_menu_item("File", "Move again"),
                "menu item 'Move again' fired",
            );
        }
        44 => {
            check(state, delegate.e2e_file_count() == 4, "second move left the grid");
            let stem_now = transfer_name().trim_end_matches(".heic").to_string();
            check(
                state,
                delegate.e2e_status_text().contains(&format!("{stem_now} 2.heic"))
                    && delegate.e2e_status_text().ends_with(", 1 renamed"),
                "status bar names the file by its new name and reports the rename",
            );
            let stem = transfer_name().trim_end_matches(".heic").to_string();
            check(
                state,
                std::path::Path::new(&format!("{}/{stem} 2.heic", transfer_folder())).exists(),
                "the collision kept both files",
            );
            // Copy leaves the grid alone.
            delegate.e2e_select(&[0]);
            delegate.e2e_transfer(&transfer_folder(), false);
        }
        45 => {
            check(state, delegate.e2e_file_count() == 4, "a copy leaves the grid alone");
            check(
                state,
                std::path::Path::new(&format!("{}/apple.heic", transfer_folder())).exists(),
                "copied image arrived in the target folder",
            );
            check(
                state,
                delegate.e2e_status_text().starts_with("Copied apple.heic to "),
                "status bar names the copied file",
            );
            // Two files at once, to check the batch line and that
            // the grid loses both.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            for tag in ["m1", "m2"] {
                let _ = std::fs::copy(
                    format!("{root}/img1.heic"),
                    format!("{root}/zzz-{tag}-{}.heic", std::process::id()),
                );
            }
        }
        46 => {
            check(state, delegate.e2e_file_count() == 6, "watcher saw both batch files");
            delegate.e2e_select(&[4, 5]);
            delegate.e2e_transfer(&transfer_folder(), true);
        }
        47 => {
            check(state, !delegate.e2e_transfer_busy(), "the two-file batch finished");
            check(state, delegate.e2e_file_count() == 4, "the batch left the grid");
            let folder_name = std::path::Path::new(&transfer_folder())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            check(
                state,
                delegate.e2e_status_text() == format!("Moved 2 images to {folder_name}"),
                "status bar counts a batch",
            );
            let _ = std::fs::remove_dir_all(transfer_folder());
        }
        48 => {
            // Filter: the bar opens, typing narrows the grid, and the
            // grid that is left is the grid.
            delegate.e2e_open_filter();
            check(state, delegate.e2e_filter_open(), "the filter bar opens");
            delegate.e2e_type_filter("img1");
            check(
                state,
                delegate.e2e_file_count() == 2,
                "the filter narrows the grid to the matches",
            );
            check(
                state,
                delegate.e2e_status_text().ends_with("2 of 4 images"),
                "status bar counts what is hidden",
            );
        }
        49 => {
            // Case is ignored, and select-all takes the matches only.
            delegate.e2e_type_filter("IMG1");
            check(
                state,
                delegate.e2e_file_count() == 2,
                "the filter ignores case",
            );
            delegate.e2e_select_all();
            check(
                state,
                delegate.e2e_status_text().starts_with("2 of 2 selected"),
                "select all takes the matches only",
            );
            check(
                state,
                delegate.e2e_playlist_len().is_none(),
                "no slideshow is running yet",
            );
            delegate.e2e_start_from_selection();
        }
        50 => {
            check(
                state,
                delegate.e2e_playlist_len() == Some(2),
                "a slideshow plays the matches only",
            );
            delegate.end_slideshow();
            delegate.e2e_close_filter();
        }
        51 => {
            check(state, !delegate.e2e_filter_open(), "escape closes the bar");
            check(
                state,
                delegate.e2e_file_count() == 4,
                "closing the bar brings the folder back",
            );
            // A new folder starts unfiltered.
            delegate.e2e_open_filter();
            delegate.e2e_type_filter("img1");
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            delegate.e2e_sidebar_click(&root);
        }
        52 => {
            check(state, !delegate.e2e_filter_open(), "a folder change closes the bar");
            check(
                state,
                delegate.e2e_file_count() == 4,
                "a folder change clears the filter",
            );
        }
        53 => {
            // New files show up only when they match.
            delegate.e2e_open_filter();
            delegate.e2e_type_filter("img1");
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let pid = std::process::id();
            let matching = std::fs::copy(
                format!("{root}/img1.heic"),
                format!("{root}/img1-extra-{pid}.heic"),
            )
            .is_ok();
            let other = std::fs::copy(
                format!("{root}/img2.heic"),
                format!("{root}/zzz-hidden-{pid}.heic"),
            )
            .is_ok();
            check(state, matching && other, "two more files copied in");
        }
        54 => {
            check(
                state,
                delegate.e2e_file_count() == 3,
                "only the matching new file joins the grid",
            );
            check(
                state,
                delegate.e2e_status_text().ends_with("3 of 6 images"),
                "the count knows about the hidden file",
            );
            delegate.e2e_close_filter();
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let pid = std::process::id();
            let _ = std::fs::remove_file(format!("{root}/img1-extra-{pid}.heic"));
            let _ = std::fs::remove_file(format!("{root}/zzz-hidden-{pid}.heic"));
        }
        55 => {
            check(state, delegate.e2e_file_count() == 4, "grid back to the fixture");
        }
        56 => {
            // Finder open: a file in the folder already showing.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            // A show is up and a filter is on, so the open has to
            // clear both to leave the grid visible.
            delegate.start_slideshow(0, false);
            delegate.e2e_open_filter();
            delegate.e2e_type_filter("apple");
            delegate.open_from_finder(vec![std::path::PathBuf::from(format!(
                "{root}/img2.heic"
            ))]);
            check(
                state,
                delegate.e2e_selected_name().as_deref() == Some("img2.heic"),
                "an opened image is selected in the grid",
            );
            check(
                state,
                !delegate.e2e_show_active(),
                "opening an image ends the slideshow instead of starting one",
            );
            check(
                state,
                !delegate.e2e_filter_open() && delegate.e2e_file_count() == 4,
                "opening an image clears a filter that would hide it",
            );
        }
        57 => {
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            delegate.open_from_finder(vec![
                std::path::PathBuf::from(format!("{root}/img1.heic")),
                std::path::PathBuf::from(format!("{root}/img2.heic")),
            ]);
            check(
                state,
                delegate.e2e_status_text().starts_with("2 of 4 selected"),
                "two opened images are both selected",
            );
            // A file in another folder: the grid follows it there.
            let _ = std::fs::create_dir_all(open_folder());
            let _ = std::fs::copy(
                format!("{root}/apple.heic"),
                format!("{}/apple.heic", open_folder()),
            );
            delegate.open_from_finder(vec![std::path::PathBuf::from(format!(
                "{}/apple.heic",
                open_folder()
            ))]);
        }
        58 => {
            check(
                state,
                delegate.e2e_file_count() == 1,
                "opening a file elsewhere scans its folder",
            );
            check(
                state,
                delegate.e2e_selected_name().as_deref() == Some("apple.heic"),
                "the opened image is selected once the scan lands",
            );
            check(
                state,
                delegate.e2e_sidebar_path().ends_with(
                    std::path::Path::new(&open_folder())
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap(),
                ),
                "the sidebar follows the opened image's folder",
            );
            // A folder opens as a folder.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            delegate.open_from_finder(vec![std::path::PathBuf::from(root)]);
        }
        59 => {
            check(
                state,
                delegate.e2e_file_count() == 4,
                "opening a folder shows that folder",
            );
            let _ = std::fs::remove_dir_all(open_folder());
        }
        60 => {
            // Right-click a cell outside the selection: it takes that
            // cell, Finder style.
            delegate.e2e_select(&[0]);
            let first = delegate.e2e_selected_name();
            let menu = delegate.e2e_grid_context_menu(2);
            let taken = delegate.e2e_selected_name();
            check(
                state,
                taken.is_some()
                    && taken != first
                    && delegate
                        .e2e_status_text()
                        .starts_with(taken.as_deref().unwrap_or_default()),
                "right-click outside the selection takes the cell under the pointer",
            );
            let titles: Vec<String> = menu.iter().map(|(title, _)| title.clone()).collect();
            check(
                state,
                titles.iter().any(|t| t == "Start slideshow")
                    && titles.iter().any(|t| t == "Reveal in Finder")
                    && titles.iter().any(|t| t == "Move to…")
                    && titles.iter().any(|t| t == "Copy to…")
                    && titles.iter().any(|t| t == "Move to trash"),
                "the grid menu offers the slideshow, reveal, filing and trash entries",
            );
            check(
                state,
                titles.iter().any(|t| t.starts_with("Move again to ")),
                "the grid menu names the folder a repeat move goes to",
            );
            check(
                state,
                menu.iter()
                    .any(|(title, enabled)| title == "Move to trash" && *enabled),
                "trash is live while images are selected",
            );
        }
        61 => {
            // Right-click inside the selection leaves it alone, so a
            // menu never acts on less than it looks like.
            delegate.e2e_select(&[0, 1]);
            delegate.e2e_grid_context_menu(1);
            check(
                state,
                delegate.e2e_status_text().starts_with("2 of 4 selected"),
                "right-click inside the selection keeps it",
            );
        }
        62 => {
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let titles = delegate.e2e_sidebar_context_menu(&root);
            check(
                state,
                titles.first().map(String::as_str) == Some("Include subfolders")
                    && titles.last().map(String::as_str) == Some("Open in Finder"),
                "the sidebar menu runs from subfolders to Finder",
            );
            check(
                state,
                titles.iter().any(|t| t == "Add to favorites"),
                "a folder outside favorites offers to join them",
            );
            delegate.e2e_add_favorite(&root);
            let titles = delegate.e2e_sidebar_context_menu(&root);
            check(
                state,
                titles.iter().any(|t| t == "Remove from favorites"),
                "a favorite offers to leave instead",
            );
            delegate.e2e_remove_favorite(&root);
        }
        63 => {
            // A folder dragged into Favorites lands in the gap it was
            // dropped in, and dragging one that is already there moves
            // it instead of doubling it.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let before = delegate.e2e_favorite_count();
            delegate.e2e_insert_favorite(&root, 0);
            let names = delegate.e2e_favorite_names();
            check(
                state,
                delegate.e2e_favorite_count() == before + 1
                    && names.first().map(String::as_str)
                        == std::path::Path::new(&root).file_name().and_then(|n| n.to_str()),
                "a folder dropped at the top of Favorites lands there",
            );
            delegate.e2e_insert_favorite(&root, 3);
            let names = delegate.e2e_favorite_names();
            check(
                state,
                delegate.e2e_favorite_count() == before + 1
                    && names.first().map(String::as_str) != std::path::Path::new(&root).file_name().and_then(|n| n.to_str()),
                "dropping it again moves it instead of adding a second row",
            );
            delegate.e2e_remove_favorite(&root);
            check(
                state,
                delegate.e2e_favorite_count() == before,
                "Favorites is back to what it was",
            );
        }
        64 => {
            // The info panel reads the same images an action would
            // work on, and says so in full for one image.
            delegate.e2e_select(&[0]);
            let text = delegate.e2e_info_text();
            let name = delegate.e2e_selected_name().unwrap_or_default();
            check(
                state,
                text.contains(&name) && text.contains("Dimensions") && text.contains("Size"),
                "the info panel names the one selected image and its dimensions",
            );
            delegate.e2e_select(&[0, 1]);
            check(
                state,
                delegate.e2e_info_text().contains("2 images"),
                "several selected images come back as a count",
            );
            delegate.e2e_select(&[]);
            check(
                state,
                delegate.e2e_info_text() == "No image selected.",
                "nothing selected says so",
            );
            delegate.e2e_toggle_info();
            check(state, delegate.e2e_info_open(), "cmd-I opens the panel");
            delegate.e2e_toggle_info();
            check(state, !delegate.e2e_info_open(), "a second cmd-I puts it away");
        }
        65 => {
            // The overlay's extra blocks, on top of a running show.
            delegate.e2e_select(&[0]);
            delegate.start_slideshow(0, true);
        }
        66 => {
            delegate.e2e_toggle_path_overlay();
            let text = delegate.e2e_overlay_text().unwrap_or_default();
            check(
                state,
                text.lines().count() >= 2 && text.contains("wene-e2e-fixture"),
                "p adds the full path under the usual line",
            );
            delegate.e2e_toggle_exif_overlay();
            let text = delegate.e2e_overlay_text().unwrap_or_default();
            check(
                state,
                text.contains("Dimensions"),
                "shift-I adds what the file header holds",
            );
            delegate.e2e_toggle_help_overlay();
            let text = delegate.e2e_overlay_text().unwrap_or_default();
            check(
                state,
                text.starts_with("Slideshow keys"),
                "h takes the overlay over with the cheat sheet",
            );
            delegate.e2e_toggle_help_overlay();
            delegate.e2e_toggle_exif_overlay();
            delegate.e2e_toggle_path_overlay();
            let text = delegate.e2e_overlay_text().unwrap_or_default();
            check(
                state,
                text.lines().count() == 1 && text.contains("/"),
                "turning the blocks off leaves the usual line alone",
            );
            // Page down walks ten slides, or as far as the folder goes.
            delegate.jump_slideshow(10);
            check(
                state,
                delegate.e2e_current_index() == Some(3),
                "a page jump stops at the last slide",
            );
            delegate.end_slideshow();
        }
        67 => {
            // Handing an image to something else: the app list, and
            // the clipboard.
            delegate.e2e_select(&[0]);
            let apps = delegate.e2e_open_with_names();
            check(
                state,
                !apps.is_empty(),
                "the open with menu lists the apps that can open the image",
            );
            delegate.e2e_copy_path(false);
            let copied = delegate.e2e_pasteboard_text();
            check(
                state,
                copied.contains("wene-e2e-fixture") && !copied.starts_with("file:"),
                "copy path puts the plain path on the clipboard",
            );
            check(
                state,
                delegate.e2e_status_text().starts_with("Copied the path"),
                "the status bar says what was copied",
            );
            delegate.e2e_select(&[0, 1]);
            delegate.e2e_copy_path(true);
            let copied = delegate.e2e_pasteboard_text();
            check(
                state,
                copied.lines().count() == 2 && copied.starts_with("file:"),
                "several images copy as one file URL per line",
            );
        }
        68 => {
            // Undo, on a file of our own so the fixture survives.
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            let _ = std::fs::copy(
                format!("{root}/img2.heic"),
                format!("{root}/{}", undo_name()),
            );
        }
        69 => {
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            check(state, delegate.e2e_file_count() == 5, "watcher saw the undo test file");
            // Name order puts zzz-undo last.
            delegate.e2e_select(&[4]);
            check(
                state,
                perform_menu_item("File", "Move to trash"),
                "menu item 'Move to trash' fired",
            );
            check(
                state,
                delegate.e2e_undo_title() == "Undo move to trash",
                "the Edit menu names what cmd-Z would take back",
            );
            check(
                state,
                !std::path::Path::new(&format!("{root}/{}", undo_name())).exists(),
                "the trashed file left the folder",
            );
            delegate.e2e_undo();
            check(
                state,
                std::path::Path::new(&format!("{root}/{}", undo_name())).exists(),
                "undo brought the culled file back",
            );
            check(
                state,
                delegate.e2e_redo_title() == "Redo move to trash",
                "and the batch is waiting to be redone",
            );
        }
        70 => {
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            check(state, delegate.e2e_file_count() == 5, "the restored file is in the grid");
            delegate.e2e_redo();
            check(
                state,
                !std::path::Path::new(&format!("{root}/{}", undo_name())).exists(),
                "redo culled it again",
            );
            // Undo once more, then move it away and undo that too.
            delegate.e2e_undo();
        }
        71 => {
            let _ = std::fs::create_dir_all(transfer_folder());
            delegate.e2e_select(&[4]);
            delegate.e2e_transfer(&transfer_folder(), true);
        }
        72 => {
            let root = std::env::args().nth(1).expect("e2e runs with a folder arg");
            check(
                state,
                std::path::Path::new(&format!("{}/{}", transfer_folder(), undo_name())).exists(),
                "the file moved to the target folder",
            );
            delegate.e2e_undo();
            check(
                state,
                std::path::Path::new(&format!("{root}/{}", undo_name())).exists()
                    && !std::path::Path::new(&format!("{}/{}", transfer_folder(), undo_name()))
                        .exists(),
                "undo walked the move back",
            );
            let _ = std::fs::remove_file(format!("{root}/{}", undo_name()));
            let _ = std::fs::remove_dir_all(transfer_folder());
        }
        73 => {
            // Thumbnail size: the ceiling holds, and the slider reads
            // what the grid settled on.
            delegate.e2e_thumb_cap(160.0);
            delegate.e2e_scale_cells(4.0);
            check(
                state,
                delegate.e2e_thumb_size() == 160.0,
                "cells stop at the size the preference allows",
            );
            check(
                state,
                delegate.e2e_slider_value() == delegate.e2e_thumb_size(),
                "the slider reads what the grid settled on",
            );
            delegate.e2e_thumb_cap(512.0);
            delegate.e2e_scale_cells(4.0);
            check(
                state,
                delegate.e2e_thumb_size() > 160.0,
                "raising the ceiling lets them grow again",
            );
            delegate.e2e_thumb_cap(160.0);
            check(
                state,
                delegate.e2e_thumb_size() == 160.0,
                "lowering it brings oversized cells back down",
            );
            delegate.e2e_thumb_cap(320.0);
            delegate.e2e_scale_cells(0.001);
            check(
                state,
                delegate.e2e_thumb_size() == 60.0,
                "and they stop at the small end too",
            );
        }
        74 => {
            // No shortcut may be bound twice.
            let clashes = delegate.e2e_menu_conflicts();
            if !clashes.is_empty() {
                for clash in &clashes {
                    println!("e2e menu clash: {clash}");
                }
            }
            check(state, clashes.is_empty(), "every menu shortcut is bound once");
        }
        _ => {
            let failures = state.borrow().failures.clone();
            if failures.is_empty() {
                println!("e2e: PASS ({} steps)", step + 1);
                std::process::exit(0);
            }
            println!("e2e: FAIL");
            for f in &failures {
                println!("  {f}");
            }
            std::process::exit(1);
        }
    }
    state.borrow_mut().step += 1;
}
