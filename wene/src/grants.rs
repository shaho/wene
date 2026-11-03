//! Keeping folders readable inside the App Sandbox.
//!
//! The sandbox lets the app read only what the user handed it: a
//! folder from an open panel, a drop, an open from Finder, plus the
//! Pictures folder its entitlements name. Every grant ends at quit. A
//! security-scoped bookmark brings it back, so every folder the app is
//! handed is remembered here, and all of them are opened again at
//! launch. Outside the sandbox (`cargo run`) nothing here does
//! anything.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::MainThreadOnly;
use objc2_app_kit::NSOpenPanel;
use objc2_foundation::{
    ns_string, NSArray, NSData, NSString, NSURLBookmarkCreationOptions,
    NSURLBookmarkResolutionOptions, NSUserDefaults, NSURL,
};
use wene_core::messages::folder_label;

use crate::{e2e, AppDelegate};

thread_local! {
    /// The folders readable this launch. Each holds its URL open:
    /// access lasts while the URL lives.
    static OPEN: RefCell<Vec<(PathBuf, Retained<NSURL>)>> = const { RefCell::new(Vec::new()) };
}

pub fn sandboxed() -> bool {
    std::env::var_os("APP_SANDBOX_CONTAINER_ID").is_some()
}

/// The user's own home folder. Inside the sandbox `HOME`,
/// `NSHomeDirectory` and `NSHomeDirectoryForUser` all point at the
/// container, so the answer comes from the user database.
pub fn home() -> PathBuf {
    #[repr(C)]
    struct Passwd {
        name: *const std::ffi::c_char,
        passwd: *const std::ffi::c_char,
        uid: u32,
        gid: u32,
        change: i64,
        class: *const std::ffi::c_char,
        gecos: *const std::ffi::c_char,
        dir: *const std::ffi::c_char,
        shell: *const std::ffi::c_char,
        expire: i64,
    }
    extern "C" {
        fn getuid() -> u32;
        fn getpwuid(uid: u32) -> *const Passwd;
    }
    // SAFETY: getpwuid returns a static record or null, and the
    // layout above is macOS's struct passwd.
    let dir = unsafe {
        let entry = getpwuid(getuid());
        if entry.is_null() || (*entry).dir.is_null() {
            return PathBuf::new();
        }
        std::ffi::CStr::from_ptr((*entry).dir)
            .to_string_lossy()
            .into_owned()
    };
    PathBuf::from(dir)
}

/// Whether the app can list a folder now.
pub fn readable(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok()
}

/// Keep `path` readable after the next launch. A folder inside one
/// already remembered needs nothing.
pub fn remember(path: &Path) {
    if !sandboxed() || e2e::enabled() {
        return;
    }
    let covered = OPEN.with(|open| open.borrow().iter().any(|(held, _)| path.starts_with(held)));
    if covered {
        return;
    }
    let Some(url) = NSURL::from_directory_path(path) else {
        return;
    };
    let Ok(data) = url.bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
        NSURLBookmarkCreationOptions::WithSecurityScope,
        None,
        None,
    ) else {
        return;
    };
    let defaults = NSUserDefaults::standardUserDefaults();
    let mut all = stored(&defaults);
    all.push(data);
    save(&defaults, &all);
    OPEN.with(|open| open.borrow_mut().push((path.to_path_buf(), url)));
}

/// At launch, open every remembered folder again. A bookmark that no
/// longer resolves (the folder is gone) is dropped, and a stale one
/// is written afresh.
pub fn restore() {
    if !sandboxed() || e2e::enabled() {
        return;
    }
    let defaults = NSUserDefaults::standardUserDefaults();
    let mut kept: Vec<Retained<NSData>> = Vec::new();
    for data in stored(&defaults) {
        let mut stale = objc2::runtime::Bool::NO;
        let resolved = unsafe {
            NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
                &data,
                NSURLBookmarkResolutionOptions::WithSecurityScope,
                None,
                &mut stale,
            )
        };
        let Ok(url) = resolved else { continue };
        if !unsafe { url.startAccessingSecurityScopedResource() } {
            continue;
        }
        let Some(path) = url.to_file_path() else {
            continue;
        };
        let data = if stale.as_bool() {
            url.bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                NSURLBookmarkCreationOptions::WithSecurityScope,
                None,
                None,
            )
            .unwrap_or(data)
        } else {
            data
        };
        kept.push(data);
        OPEN.with(|open| open.borrow_mut().push((path, url)));
    }
    save(&defaults, &kept);
}

fn stored(defaults: &NSUserDefaults) -> Vec<Retained<NSData>> {
    defaults
        .arrayForKey(ns_string!("grants"))
        .map(|array| {
            array
                .iter()
                .filter_map(|item| item.downcast::<NSData>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn save(defaults: &NSUserDefaults, all: &[Retained<NSData>]) {
    unsafe {
        defaults.setObject_forKey(
            Some(&NSArray::from_retained_slice(all)),
            ns_string!("grants"),
        )
    };
}

impl AppDelegate {
    /// Ask for a folder the sandbox refuses, with the panel open on
    /// it. The folder picked, which may be a wider one, is remembered.
    /// Cancel returns None.
    pub(crate) fn ask_access(&self, path: &Path) -> Option<PathBuf> {
        let panel = NSOpenPanel::openPanel(self.mtm());
        panel.setCanChooseDirectories(true);
        panel.setCanChooseFiles(false);
        panel.setAllowsMultipleSelection(false);
        panel.setDirectoryURL(NSURL::from_directory_path(path).as_deref());
        panel.setMessage(Some(&NSString::from_str(&format!(
            "Wêne needs your permission to read {}.",
            folder_label(path)
        ))));
        panel.setPrompt(Some(ns_string!("Grant access")));
        if !self.run_panel(&panel) {
            return None;
        }
        let granted = PathBuf::from(panel.URL()?.path()?.to_string());
        remember(&granted);
        Some(granted)
    }
}
