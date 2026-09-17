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
            delegate.start_slideshow(0);
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
            delegate.start_slideshow(0);
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
