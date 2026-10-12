//! The info panel (cmd-I), and the file header rows it shows.

use super::*;

impl AppDelegate {
    /// cmd-I: the info panel, a floating window that follows whatever
    /// an action would work on. A second press puts it away.
    pub(crate) fn toggle_info(&self) {
        let mtm = self.mtm();
        let window = self.ivars().info_window.get().cloned().unwrap_or_else(|| {
            let (window, label) = build_info_window(mtm);
            let _ = self.ivars().info_window.set(window.clone());
            let _ = self.ivars().info_label.set(label);
            window
        });
        if window.isVisible() {
            window.orderOut(None);
            return;
        }
        // Show it first: the refresh fills only a panel that is up,
        // so filling it before would leave it blank.
        window.makeKeyAndOrderFront(None);
        self.refresh_info();
    }

    /// Fill the panel, if it is up. Cheap enough to run on every
    /// selection change: the header read is not a decode.
    pub(crate) fn refresh_info(&self) {
        let Some(window) = self.ivars().info_window.get() else {
            return;
        };
        let Some(label) = self.ivars().info_label.get() else {
            return;
        };
        if !window.isVisible() {
            return;
        }
        label.setStringValue(&NSString::from_str(&self.info_text()));
        label.sizeToFit();
    }

    /// What the panel says: one image in full, several as a summary,
    /// none as a line saying so.
    pub(crate) fn info_text(&self) -> String {
        let paths = self.action_targets();
        match paths.as_slice() {
            [] => "No image selected.".to_owned(),
            [path] => {
                let mut rows = vec![("Name".to_owned(), file_name(path))];
                if let Some(kind) = path.extension() {
                    rows.push((
                        "Kind".to_owned(),
                        format!("{} image", kind.to_string_lossy().to_uppercase()),
                    ));
                }
                if let Ok(meta) = std::fs::metadata(path) {
                    rows.push(("Size".to_owned(), format_bytes(meta.len())));
                    if let Ok(modified) = meta.modified() {
                        rows.push(("Modified".to_owned(), date_text(modified)));
                    }
                }
                rows.extend(self.header_rows(path));
                if let Some(folder) = path.parent() {
                    rows.push(("Folder".to_owned(), folder.to_string_lossy().into_owned()));
                }
                rows_text(&rows)
            }
            many => {
                // Several images are the grid's selection, and the scan
                // already knows every size: no stat per file.
                let bytes: u64 = self
                    .ivars()
                    .grid
                    .get()
                    .map(|grid| grid.selection_info().1.iter().map(|info| info.size).sum())
                    .unwrap_or(0);
                let folder = many[0]
                    .parent()
                    .map(|folder| folder.to_string_lossy().into_owned())
                    .unwrap_or_default();
                rows_text(&[
                    ("Selected".to_owned(), format!("{} images", many.len())),
                    ("Size".to_owned(), format_bytes(bytes)),
                    ("Folder".to_owned(), folder),
                ])
            }
        }
    }

    /// The header rows of one file, read once until another file asks
    /// or this one changes on disk.
    pub(crate) fn header_rows(&self, path: &Path) -> HeaderRows {
        if let Some((cached, rows)) = self.ivars().header_rows.borrow().as_ref() {
            if cached == path {
                return rows.clone();
            }
        }
        let rows = decoder::image_info(path);
        self.ivars()
            .header_reads
            .set(self.ivars().header_reads.get() + 1);
        *self.ivars().header_rows.borrow_mut() = Some((path.to_path_buf(), rows.clone()));
        rows
    }
}

/// Label and value, one pair per line, labels padded so the values
/// line up in the panel's fixed-width layout.
pub(crate) fn rows_text(rows: &[(String, String)]) -> String {
    let width = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);
    rows.iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(label, value)| format!("{label:<width$}   {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A date the way the user's own settings write it.
fn date_text(time: std::time::SystemTime) -> String {
    let Ok(since) = time.duration_since(std::time::UNIX_EPOCH) else {
        return String::new();
    };
    let date = objc2_foundation::NSDate::dateWithTimeIntervalSince1970(since.as_secs_f64());
    objc2_foundation::NSDateFormatter::localizedStringFromDate_dateStyle_timeStyle(
        &date,
        objc2_foundation::NSDateFormatterStyle::MediumStyle,
        objc2_foundation::NSDateFormatterStyle::ShortStyle,
    )
    .to_string()
}

/// An EXIF capture time, printed the way the user's settings write
/// dates. EXIF holds the camera's own local time with no zone, so it
/// is read in the local zone and written back in it: the two cancel
/// and the digits stay the ones the camera recorded.
pub fn exif_date_text(raw: &str) -> Option<String> {
    let formatter = objc2_foundation::NSDateFormatter::new();
    formatter.setDateFormat(Some(ns_string!("yyyy:MM:dd HH:mm:ss")));
    let date = formatter.dateFromString(&NSString::from_str(raw))?;
    Some(
        objc2_foundation::NSDateFormatter::localizedStringFromDate_dateStyle_timeStyle(
            &date,
            objc2_foundation::NSDateFormatterStyle::MediumStyle,
            objc2_foundation::NSDateFormatterStyle::ShortStyle,
        )
        .to_string(),
    )
}

/// The info panel: a floating utility window holding one label. It
/// floats so it can sit beside a full-screen slideshow.
fn build_info_window(mtm: MainThreadMarker) -> (Retained<NSWindow>, Retained<NSTextField>) {
    let frame = NSRect::new(CGPoint::new(120.0, 400.0), CGSize::new(380.0, 360.0));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::UtilityWindow
                | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(ns_string!("Info"));
    unsafe { window.setReleasedWhenClosed(false) };
    window.setLevel(objc2_app_kit::NSFloatingWindowLevel as isize);

    let label = NSTextField::labelWithString(ns_string!(""), mtm);
    label.setSelectable(true);
    label.setUsesSingleLineMode(false);
    label.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(11.0, 0.0)));
    label.setFrame(NSRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(frame.size.width - 24.0, frame.size.height - 24.0),
    ));

    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        NSRect::new(
            CGPoint::new(12.0, 12.0),
            CGSize::new(frame.size.width - 24.0, frame.size.height - 24.0),
        ),
    );
    scroll.setHasVerticalScroller(true);
    scroll.setDrawsBackground(false);
    scroll.setDocumentView(Some(&label));
    window.setContentView(Some(&scroll));
    (window, label)
}
