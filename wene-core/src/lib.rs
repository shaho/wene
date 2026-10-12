//! Portable core: file scanning, sorted insertion, decode scheduling.
//! No AppKit, no objc2. The shell supplies the decoder and a wakeup
//! callback; the core owns all worker threads.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::SystemTime;

/// Platform hook that reads (EXIF date, date added) for one file.
type DatesFn = Arc<dyn Fn(&Path) -> (Option<SystemTime>, Option<SystemTime>) + Send + Sync>;

use lexical_sort::natural_lexical_cmp;

/// Platform decoder. The macOS shell implements this with Image I/O.
/// `max_px` caps the long edge; EXIF orientation must be applied.
pub trait ImageDecoder: Send + Sync + 'static {
    type Image: Send + 'static;
    fn decode(&self, path: &Path, max_px: i32) -> Option<Self::Image>;

    /// Thumbnail decode. Platforms with a fast path (a preview
    /// already embedded in the file) override this; the default is a
    /// full decode.
    fn decode_thumb(&self, path: &Path, max_px: i32) -> Option<Self::Image> {
        self.decode(path, max_px)
    }

    /// Extra sort metadata: (EXIF capture date, date added to its
    /// folder). Read during the scan; None falls back to the
    /// modified date when sorting.
    fn file_dates(
        &self,
        _path: &Path,
    ) -> (Option<std::time::SystemTime>, Option<std::time::SystemTime>) {
        (None, None)
    }

    /// All frames of an animated image with per-frame delays in
    /// seconds. None means "not animated": the slide falls back to a
    /// normal decode.
    fn decode_frames(&self, _path: &Path) -> Option<Vec<(Self::Image, f64)>> {
        None
    }
}

/// The `scan` in a file event is the number of the scan it belongs
/// to. A new scan makes every event of the old one stale, including
/// those already in the channel: ask `Engine::is_stale` and drop them.
pub enum Event<I> {
    /// Files inserted into the sorted-by-name list. Each entry is the
    /// index the file was inserted at (indices are valid when applied
    /// in order) plus its info.
    FilesInserted {
        scan: u64,
        files: Vec<(usize, FileInfo)>,
    },
    /// Files that disappeared from disk (watcher).
    FilesRemoved {
        scan: u64,
        paths: Vec<PathBuf>,
    },
    /// A file's contents changed on disk; caches for it are stale.
    FileChanged {
        scan: u64,
        info: FileInfo,
    },
    ScanDone {
        scan: u64,
        total: usize,
    },
    ThumbReady {
        path: PathBuf,
        image: I,
    },
    SlideReady {
        path: PathBuf,
        image: I,
    },
    /// An animated slide: every frame plus its delay in seconds.
    SlideFramesReady {
        path: PathBuf,
        frames: Vec<(I, f64)>,
    },
    DecodeFailed {
        path: PathBuf,
    },
}

/// A found image with the metadata the sort orders need, captured
/// during the scan so re-sorting later does no disk I/O.
#[derive(Clone, Debug)]
pub struct FileInfo {
    pub path: PathBuf,
    pub modified: std::time::SystemTime,
    pub size: u64,
    /// EXIF capture date, when the file has one.
    pub exif_date: Option<std::time::SystemTime>,
    /// When the file was added to its folder (macOS attribute).
    pub added: Option<std::time::SystemTime>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortOrder {
    Name,
    Modified,
    Size,
    Path,
    ExifDate,
    Added,
}

/// Compare two files under an order. Ties always fall back to the
/// natural name order so every order is stable and total.
pub fn file_info_cmp(
    a: &FileInfo,
    b: &FileInfo,
    order: SortOrder,
    descending: bool,
) -> std::cmp::Ordering {
    let primary = match order {
        SortOrder::Name => file_cmp(&a.path, &b.path),
        SortOrder::Modified => a.modified.cmp(&b.modified),
        SortOrder::Size => a.size.cmp(&b.size),
        SortOrder::Path => {
            natural_lexical_cmp(&a.path.to_string_lossy(), &b.path.to_string_lossy())
        }
        // Missing dates fall back to the modified date, like the
        // original app's EXIF/creation-date order.
        SortOrder::ExifDate => a
            .exif_date
            .unwrap_or(a.modified)
            .cmp(&b.exif_date.unwrap_or(b.modified)),
        SortOrder::Added => a
            .added
            .unwrap_or(a.modified)
            .cmp(&b.added.unwrap_or(b.modified)),
    };
    let primary = if descending {
        primary.reverse()
    } else {
        primary
    };
    primary.then_with(|| file_cmp(&a.path, &b.path))
}

const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "heic", "heif", "webp", "tif", "tiff", "bmp", "avif", "jxl",
];

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Natural string order for folder names in the shell's browser.
pub fn natural_str_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    natural_lexical_cmp(a, b)
}

/// Finder-like order: natural comparison of the filename, full path
/// as the tie break.
fn file_cmp(a: &Path, b: &Path) -> std::cmp::Ordering {
    let name = |p: &Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    natural_lexical_cmp(&name(a), &name(b))
        .then_with(|| natural_lexical_cmp(&a.to_string_lossy(), &b.to_string_lossy()))
}

const SCAN_BATCH: usize = 64;

enum Job {
    Thumb(PathBuf),
    /// `max_px` None takes the default slide cap. A zoomed slide
    /// names a bigger one, and skips the animation path: a still
    /// frame at full size is what zoom asks for.
    Slide {
        path: PathBuf,
        max_px: Option<i32>,
    },
}

/// Decode and scan engine. One scanner thread per scan, a two-thread
/// thumbnail pool, and one dedicated slide thread so slideshow
/// decodes never wait behind queued thumbnails.
pub struct Engine<I> {
    thumb_tx: Sender<Job>,
    slide_tx: Sender<Job>,
    events_tx: Sender<Event<I>>,
    wakeup: Arc<dyn Fn() + Send + Sync>,
    /// The longest edge a thumbnail is decoded to. Shared with the
    /// workers so the preference can change it while the app runs.
    max_thumb_px: Arc<AtomicI32>,
    /// Canonical name-sorted list; shared by the scan walk and the
    /// file watcher so insert indices stay consistent.
    files: Arc<Mutex<Vec<FileInfo>>>,
    /// The number of the scan `files` belongs to. It changes under
    /// the `files` lock, and every writer checks it under that lock,
    /// so an old scan's walk or watcher never reaches the new list.
    scan: Arc<AtomicU64>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    dates: DatesFn,
}

impl<I: Send + 'static> Engine<I> {
    pub fn start<D: ImageDecoder<Image = I>>(
        decoder: D,
        max_thumb_px: i32,
        max_slide_px: i32,
        wakeup: impl Fn() + Send + Sync + 'static,
    ) -> (Self, Receiver<Event<I>>) {
        let decoder = Arc::new(decoder);
        let dates: DatesFn = {
            let decoder = Arc::clone(&decoder);
            Arc::new(move |path: &Path| decoder.file_dates(path))
        };
        let wakeup: Arc<dyn Fn() + Send + Sync> = Arc::new(wakeup);
        let (events_tx, events_rx) = channel::<Event<I>>();

        let max_thumb_px = Arc::new(AtomicI32::new(max_thumb_px));

        let (thumb_tx, thumb_rx) = channel::<Job>();
        let thumb_rx = Arc::new(Mutex::new(thumb_rx));
        for _ in 0..2 {
            spawn_decode_worker(
                Arc::clone(&thumb_rx),
                Arc::clone(&decoder),
                Arc::clone(&max_thumb_px),
                max_slide_px,
                events_tx.clone(),
                Arc::clone(&wakeup),
            );
        }

        let (slide_tx, slide_rx) = channel::<Job>();
        spawn_decode_worker(
            Arc::new(Mutex::new(slide_rx)),
            Arc::clone(&decoder),
            Arc::clone(&max_thumb_px),
            max_slide_px,
            events_tx.clone(),
            Arc::clone(&wakeup),
        );

        (
            Engine {
                thumb_tx,
                slide_tx,
                events_tx,
                wakeup,
                max_thumb_px,
                files: Arc::new(Mutex::new(Vec::new())),
                scan: Arc::new(AtomicU64::new(0)),
                watcher: Mutex::new(None),
                dates,
            },
            events_rx,
        )
    }

    /// Tell the engine a file appeared or vanished, without waiting
    /// for the watcher to notice. An undo brings a file back within
    /// the same breath as the delete that took it away, and the
    /// watcher can still be carrying the delete; going through the
    /// same code the watcher uses keeps one list and one order, and
    /// the watcher's own event a moment later then finds nothing to
    /// do.
    pub fn notice(&self, paths: &[PathBuf]) {
        let scan = self.scan.load(Ordering::SeqCst);
        for path in paths {
            apply_fs_change(
                path,
                scan,
                &self.scan,
                &self.files,
                &self.events_tx,
                &self.wakeup,
                &self.dates,
            );
        }
    }

    /// True for a file event of a scan that a newer scan replaced.
    /// Its indices and paths belong to the old folder.
    pub fn is_stale(&self, event: &Event<I>) -> bool {
        let scan = match event {
            Event::FilesInserted { scan, .. }
            | Event::FilesRemoved { scan, .. }
            | Event::FileChanged { scan, .. }
            | Event::ScanDone { scan, .. } => *scan,
            _ => return false,
        };
        scan != self.scan.load(Ordering::SeqCst)
    }

    /// Walk `root` on a fresh thread, recursively or one level deep.
    /// Batches of found files stream out as FilesInserted events with
    /// sorted-insert indices; the shell mirrors the inserts to keep
    /// the same order. The folder is then watched (matching depth):
    /// adds, removals, and edits keep flowing as events until the
    /// next scan. The next scan stops this one's walk and watcher.
    pub fn scan(&self, root: PathBuf, recursive: bool) {
        let scan = {
            let mut files = self.files.lock().unwrap();
            files.clear();
            self.scan.fetch_add(1, Ordering::SeqCst) + 1
        };
        *self.watcher.lock().unwrap() = None;
        self.start_watcher(&root, recursive, scan);

        let current = Arc::clone(&self.scan);
        let events_tx = self.events_tx.clone();
        let wakeup = Arc::clone(&self.wakeup);
        let files = Arc::clone(&self.files);
        let dates = Arc::clone(&self.dates);
        thread::spawn(move || {
            let mut pending: Vec<FileInfo> = Vec::new();
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                // A deep tree with few images flushes rarely, so the
                // walk also looks between folders.
                if current.load(Ordering::SeqCst) != scan {
                    return;
                }
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let hidden = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with('.'))
                        .unwrap_or(true);
                    if hidden {
                        continue;
                    }
                    if path.is_dir() {
                        if recursive {
                            stack.push(path);
                        }
                    } else if is_image(&path) {
                        let meta = entry.metadata().ok();
                        // ponytail: dates read eagerly per file (one
                        // header read each); go lazy if huge folders
                        // make the scan crawl.
                        let (exif_date, added) = dates(&path);
                        pending.push(FileInfo {
                            modified: meta
                                .as_ref()
                                .and_then(|m| m.modified().ok())
                                .unwrap_or(std::time::UNIX_EPOCH),
                            size: meta.map(|m| m.len()).unwrap_or(0),
                            path,
                            exif_date,
                            added,
                        });
                        if pending.len() >= SCAN_BATCH
                            && !flush(scan, &current, &files, &mut pending, &events_tx, &wakeup)
                        {
                            return;
                        }
                    }
                }
            }
            if !flush(scan, &current, &files, &mut pending, &events_tx, &wakeup) {
                return;
            }
            let total = files.lock().unwrap().len();
            let _ = events_tx.send(Event::ScanDone { scan, total });
            wakeup();
        });
    }

    fn start_watcher(&self, root: &Path, recursive: bool, scan: u64) {
        use notify::Watcher;
        let current = Arc::clone(&self.scan);
        let events_tx = self.events_tx.clone();
        let wakeup = Arc::clone(&self.wakeup);
        let files = Arc::clone(&self.files);
        let dates = Arc::clone(&self.dates);
        // Dropping the watcher stops new calls, not one already
        // running, so the handler carries its scan's number too.
        let handler = move |result: Result<notify::Event, notify::Error>| {
            let Ok(event) = result else { return };
            for path in event.paths {
                apply_fs_change(&path, scan, &current, &files, &events_tx, &wakeup, &dates);
            }
        };
        let Ok(mut watcher) = notify::recommended_watcher(handler) else {
            return;
        };
        let mode = if recursive {
            notify::RecursiveMode::Recursive
        } else {
            notify::RecursiveMode::NonRecursive
        };
        if watcher.watch(root, mode).is_ok() {
            *self.watcher.lock().unwrap() = Some(watcher);
        }
    }

    pub fn request_thumb(&self, path: PathBuf) {
        let _ = self.thumb_tx.send(Job::Thumb(path));
    }

    pub fn request_slide(&self, path: PathBuf) {
        let _ = self.slide_tx.send(Job::Slide { path, max_px: None });
    }

    /// Decode a slide to `max_px` on its longest edge, for a zoom
    /// that has outgrown the decode on hand.
    pub fn request_slide_at(&self, path: PathBuf, max_px: i32) {
        let _ = self.slide_tx.send(Job::Slide {
            path,
            max_px: Some(max_px),
        });
    }

    pub fn max_thumb_px(&self) -> i32 {
        self.max_thumb_px.load(Ordering::Relaxed)
    }

    /// Change the size thumbnails are decoded to. Thumbnails already
    /// decoded stay as they are until something asks for them again.
    pub fn set_max_thumb_px(&self, pixels: i32) {
        self.max_thumb_px.store(pixels.max(1), Ordering::Relaxed);
    }
}

/// Insert a batch from scan number `scan`. False when a newer scan
/// has replaced it: nothing is inserted, and the walk must stop.
fn flush<I>(
    scan: u64,
    current: &AtomicU64,
    files: &Arc<Mutex<Vec<FileInfo>>>,
    pending: &mut Vec<FileInfo>,
    events_tx: &Sender<Event<I>>,
    wakeup: &Arc<dyn Fn() + Send + Sync>,
) -> bool {
    let mut inserts = Vec::with_capacity(pending.len());
    {
        let mut sorted = files.lock().unwrap();
        if current.load(Ordering::SeqCst) != scan {
            return false;
        }
        for info in pending.drain(..) {
            match sorted.binary_search_by(|p| file_cmp(&p.path, &info.path)) {
                // Already present (watcher raced the walk): skip.
                Ok(_) => {}
                Err(index) => {
                    sorted.insert(index, info.clone());
                    inserts.push((index, info));
                }
            }
        }
    }
    if !inserts.is_empty()
        && events_tx
            .send(Event::FilesInserted {
                scan,
                files: inserts,
            })
            .is_ok()
    {
        wakeup();
    }
    true
}

/// Watcher callback: reconcile one path against the canonical list.
/// Existence on disk decides between insert, change, and removal,
/// which absorbs create/modify/rename event ambiguity. A change seen
/// by scan number `scan` is dropped once a newer scan has replaced it.
fn apply_fs_change<I>(
    path: &Path,
    scan: u64,
    current: &AtomicU64,
    files: &Arc<Mutex<Vec<FileInfo>>>,
    events_tx: &Sender<Event<I>>,
    wakeup: &Arc<dyn Fn() + Send + Sync>,
    dates: &DatesFn,
) {
    if !is_image(path) {
        return;
    }
    let hidden = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(true);
    if hidden {
        return;
    }
    let meta = std::fs::metadata(path).ok();
    let make_info = |meta: &std::fs::Metadata| {
        let (exif_date, added) = dates(path);
        FileInfo {
            path: path.to_path_buf(),
            modified: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            size: meta.len(),
            exif_date,
            added,
        }
    };
    let event = {
        let mut sorted = files.lock().unwrap();
        if current.load(Ordering::SeqCst) != scan {
            return;
        }
        let existing = sorted.binary_search_by(|p| file_cmp(&p.path, path));
        match (meta, existing) {
            (Some(meta), Err(index)) => {
                let info = make_info(&meta);
                sorted.insert(index, info.clone());
                Some(Event::FilesInserted {
                    scan,
                    files: vec![(index, info)],
                })
            }
            (Some(meta), Ok(index)) => {
                let info = make_info(&meta);
                sorted[index] = info.clone();
                Some(Event::FileChanged { scan, info })
            }
            (None, Ok(index)) => {
                sorted.remove(index);
                Some(Event::FilesRemoved {
                    scan,
                    paths: vec![path.to_path_buf()],
                })
            }
            (None, Err(_)) => None,
        }
    };
    if let Some(event) = event {
        if events_tx.send(event).is_ok() {
            wakeup();
        }
    }
}

fn spawn_decode_worker<I: Send + 'static, D: ImageDecoder<Image = I>>(
    jobs: Arc<Mutex<Receiver<Job>>>,
    decoder: Arc<D>,
    max_thumb_px: Arc<AtomicI32>,
    max_slide_px: i32,
    events_tx: Sender<Event<I>>,
    wakeup: Arc<dyn Fn() + Send + Sync>,
) {
    thread::spawn(move || loop {
        let job = {
            let rx = jobs.lock().unwrap();
            rx.recv()
        };
        let Ok(job) = job else { return };
        let event = match job {
            Job::Thumb(path) => {
                match decoder.decode_thumb(&path, max_thumb_px.load(Ordering::Relaxed)) {
                    Some(image) => Event::ThumbReady { path, image },
                    None => Event::DecodeFailed { path },
                }
            }
            Job::Slide {
                path,
                max_px: Some(max_px),
            } => match decoder.decode(&path, max_px) {
                Some(image) => Event::SlideReady { path, image },
                None => Event::DecodeFailed { path },
            },
            Job::Slide { path, max_px: None } => match decoder.decode_frames(&path) {
                Some(frames) => Event::SlideFramesReady { path, frames },
                None => match decoder.decode(&path, max_slide_px) {
                    Some(image) => Event::SlideReady { path, image },
                    None => Event::DecodeFailed { path },
                },
            },
        };
        if events_tx.send(event).is_err() {
            return;
        }
        wakeup();
    });
}

/// A zoomed slide needs more pixels than the decode it is drawn
/// from. Returns the longest edge to decode to, or None when the
/// decode on hand is good enough.
///
/// `natural_long` is the file's own longest edge, `decoded_long` the
/// decode being drawn, `zoom` the scale against the natural size,
/// and `backing` the screen's pixel-per-point factor. The answer
/// never asks for more than the file holds, and asks only when the
/// gain is worth a decode, which keeps a pinch from queueing one
/// decode per frame.
pub fn zoom_decode_request(
    natural_long: i32,
    decoded_long: i32,
    zoom: f64,
    backing: f64,
) -> Option<i32> {
    if natural_long <= 0 || decoded_long <= 0 || !zoom.is_finite() || zoom <= 0.0 {
        return None;
    }
    // The decode already holds everything the file has.
    if decoded_long >= natural_long {
        return None;
    }
    let wanted = (natural_long as f64 * zoom * backing.max(1.0)).ceil();
    let wanted = (wanted as i32).min(natural_long);
    // A decode costs a full read; a fifth more pixels does not pay
    // for one.
    if wanted as f64 <= decoded_long as f64 * 1.2 {
        return None;
    }
    Some(wanted)
}

/// Parse an EXIF datetime ("2023:07:14 10:30:05") into a SystemTime.
/// EXIF carries no time zone; the value is treated as UTC, which
/// keeps the ordering consistent.
pub fn parse_exif_datetime(s: &str) -> Option<SystemTime> {
    let mut it = s
        .split(|c: char| c == ':' || c == ' ')
        .map(|part| part.trim().parse::<i64>());
    let mut next = || it.next()?.ok();
    let (y, mo, d) = (next()?, next()?, next()?);
    let (h, mi, se) = (next()?, next()?, next()?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    // Howard Hinnant's days-from-civil formula.
    let y_adj = y - i64::from(mo <= 2);
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + se;
    if secs < 0 {
        return None;
    }
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

#[cfg(test)]
mod zoom_tests {
    use super::zoom_decode_request;

    #[test]
    fn fit_needs_nothing() {
        // Fit on a retina screen: the 2560 decode covers it.
        assert_eq!(zoom_decode_request(6000, 2560, 0.2, 2.0), None);
    }

    #[test]
    fn zooming_in_asks_for_more() {
        assert_eq!(zoom_decode_request(6000, 2560, 1.0, 2.0), Some(6000));
        assert_eq!(zoom_decode_request(6000, 1000, 0.25, 2.0), Some(3000));
    }

    #[test]
    fn never_asks_for_more_than_the_file_holds() {
        assert_eq!(zoom_decode_request(4000, 1000, 8.0, 2.0), Some(4000));
        assert_eq!(zoom_decode_request(4000, 4000, 8.0, 2.0), None);
        assert_eq!(zoom_decode_request(800, 800, 4.0, 2.0), None);
    }

    #[test]
    fn a_small_gain_is_not_worth_a_decode() {
        // Wants 2808: more than the 2560 on hand, but under the
        // fifth more that pays for a decode.
        assert_eq!(zoom_decode_request(6000, 2560, 0.234, 2.0), None);
        // Wants 3600: worth it.
        assert_eq!(zoom_decode_request(6000, 2560, 0.3, 2.0), Some(3600));
    }

    #[test]
    fn nonsense_input_asks_for_nothing() {
        assert_eq!(zoom_decode_request(0, 100, 2.0, 2.0), None);
        assert_eq!(zoom_decode_request(100, 0, 2.0, 2.0), None);
        assert_eq!(zoom_decode_request(100, 50, f64::NAN, 2.0), None);
        assert_eq!(zoom_decode_request(100, 50, -1.0, 2.0), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_and_sorted_insert() {
        let mut sorted: Vec<PathBuf> = Vec::new();
        let names = ["img10.jpg", "img2.jpg", "b.png", "img1.jpg"];
        for n in names {
            let p = PathBuf::from(format!("/x/{n}"));
            let i = match sorted.binary_search_by(|q| file_cmp(q, &p)) {
                Ok(i) | Err(i) => i,
            };
            sorted.insert(i, p);
        }
        let got: Vec<_> = sorted
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(got, ["b.png", "img1.jpg", "img2.jpg", "img10.jpg"]);
        assert!(is_image(Path::new("/a/x.HEIC")));
        assert!(!is_image(Path::new("/a/x.txt")));
    }

    fn info(name: &str, secs: u64, size: u64) -> FileInfo {
        FileInfo {
            path: PathBuf::from(format!("/x/{name}")),
            modified: std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs),
            size,
            exif_date: None,
            added: None,
        }
    }

    #[test]
    fn sort_orders() {
        let mut files = vec![
            info("img10.jpg", 30, 5),
            info("img2.jpg", 10, 50),
            info("a.png", 20, 20),
        ];
        let names = |files: &Vec<FileInfo>| -> Vec<String> {
            files
                .iter()
                .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };

        files.sort_by(|a, b| file_info_cmp(a, b, SortOrder::Name, false));
        assert_eq!(names(&files), ["a.png", "img2.jpg", "img10.jpg"]);

        files.sort_by(|a, b| file_info_cmp(a, b, SortOrder::Name, true));
        assert_eq!(names(&files), ["img10.jpg", "img2.jpg", "a.png"]);

        files.sort_by(|a, b| file_info_cmp(a, b, SortOrder::Modified, false));
        assert_eq!(names(&files), ["img2.jpg", "a.png", "img10.jpg"]);

        files.sort_by(|a, b| file_info_cmp(a, b, SortOrder::Size, true));
        assert_eq!(names(&files), ["img2.jpg", "a.png", "img10.jpg"]);

        // Equal keys fall back to name order: stable and total.
        let mut same = vec![info("b.jpg", 5, 9), info("a.jpg", 5, 9)];
        same.sort_by(|a, b| file_info_cmp(a, b, SortOrder::Size, false));
        assert_eq!(names(&same), ["a.jpg", "b.jpg"]);
    }

    #[test]
    fn exif_datetime_parses() {
        let t = parse_exif_datetime("1970:01:01 00:00:00").unwrap();
        assert_eq!(t, std::time::UNIX_EPOCH);
        let t = parse_exif_datetime("2023:07:14 10:30:05").unwrap();
        let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1689330605); // date -u -j -f %Y-%m-%dT%T 2023-07-14T10:30:05 +%s
        assert!(parse_exif_datetime("").is_none());
        assert!(parse_exif_datetime("2023:13:01 00:00:00").is_none());
        assert!(parse_exif_datetime("not a date").is_none());
    }

    #[test]
    fn date_sorts_fall_back_to_modified() {
        let day = 86400u64;
        let mut a = info("a.jpg", 3 * day, 1); // no EXIF: uses modified
        let b = {
            let mut b = info("b.jpg", 9 * day, 1);
            b.exif_date = parse_exif_datetime("1970:01:02 00:00:00"); // day 1
            b
        };
        let mut files = vec![a.clone(), b.clone()];
        files.sort_by(|x, y| file_info_cmp(x, y, SortOrder::ExifDate, false));
        // b's EXIF day 1 sorts before a's modified day 3.
        assert_eq!(files[0].path, b.path);

        a.added = Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(20 * day));
        let mut files = vec![a.clone(), b.clone()];
        files.sort_by(|x, y| file_info_cmp(x, y, SortOrder::Added, false));
        // a added day 20 vs b's modified fallback day 9.
        assert_eq!(files[0].path, b.path);
    }

    /// Decodes nothing, and reads each file's dates slowly, so a walk
    /// of a big folder is still running when the next scan starts.
    struct SlowDates;

    impl ImageDecoder for SlowDates {
        type Image = ();
        fn decode(&self, _path: &Path, _max_px: i32) -> Option<()> {
            None
        }
        fn file_dates(&self, _path: &Path) -> (Option<SystemTime>, Option<SystemTime>) {
            thread::sleep(std::time::Duration::from_millis(2));
            (None, None)
        }
    }

    #[test]
    fn a_new_scan_drops_the_old_folder() {
        let root = std::env::temp_dir().join(format!("wene-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (big, small) = (root.join("big"), root.join("small"));
        std::fs::create_dir_all(&big).unwrap();
        std::fs::create_dir_all(&small).unwrap();
        for n in 0..300 {
            std::fs::write(big.join(format!("{n}.jpg")), "").unwrap();
        }
        for name in ["a.jpg", "b.jpg", "c.jpg"] {
            std::fs::write(small.join(name), "").unwrap();
        }
        let (engine, events) = Engine::start(SlowDates, 100, 100, || {});

        engine.scan(big, false);
        thread::sleep(std::time::Duration::from_millis(200));
        // What the shell does: reset its list, then start the scan.
        let mut shown: Vec<PathBuf> = Vec::new();
        engine.scan(small.clone(), false);
        // Long enough for the old walk to have finished, had it run on.
        while let Ok(event) = events.recv_timeout(std::time::Duration::from_millis(1000)) {
            if engine.is_stale(&event) {
                continue;
            }
            if let Event::FilesInserted { files, .. } = event {
                for (index, info) in files {
                    shown.insert(index, info.path);
                }
            }
        }

        let expected: Vec<PathBuf> = ["a.jpg", "b.jpg", "c.jpg"]
            .iter()
            .map(|name| small.join(name))
            .collect();
        assert_eq!(shown, expected);
        let listed: Vec<PathBuf> = engine
            .files
            .lock()
            .unwrap()
            .iter()
            .map(|f| f.path.clone())
            .collect();
        assert_eq!(listed, expected);
        let _ = std::fs::remove_dir_all(&root);
    }
}

pub mod cache;
pub mod history;
pub mod playlist;
pub mod transfer;
pub use cache::LruCache;
pub use playlist::Playlist;
