//! Portable core: file scanning, sorted insertion, decode scheduling.
//! No AppKit, no objc2. The shell supplies the decoder and a wakeup
//! callback; the core owns all worker threads.

use std::path::{Path, PathBuf};
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
    fn file_dates(&self, _path: &Path) -> (Option<std::time::SystemTime>, Option<std::time::SystemTime>) {
        (None, None)
    }
}

pub enum Event<I> {
    /// Files inserted into the sorted-by-name list. Each entry is the
    /// index the file was inserted at (indices are valid when applied
    /// in order) plus its info.
    FilesInserted(Vec<(usize, FileInfo)>),
    /// Files that disappeared from disk (watcher).
    FilesRemoved(Vec<PathBuf>),
    /// A file's contents changed on disk; caches for it are stale.
    FileChanged(FileInfo),
    ScanDone { total: usize },
    ThumbReady { path: PathBuf, image: I },
    SlideReady { path: PathBuf, image: I },
    DecodeFailed { path: PathBuf },
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
    let primary = if descending { primary.reverse() } else { primary };
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
    Slide(PathBuf),
}

/// Decode and scan engine. One scanner thread per scan, a two-thread
/// thumbnail pool, and one dedicated slide thread so slideshow
/// decodes never wait behind queued thumbnails.
pub struct Engine<I> {
    thumb_tx: Sender<Job>,
    slide_tx: Sender<Job>,
    events_tx: Sender<Event<I>>,
    wakeup: Arc<dyn Fn() + Send + Sync>,
    max_thumb_px: i32,
    /// Canonical name-sorted list; shared by the scan walk and the
    /// file watcher so insert indices stay consistent.
    files: Arc<Mutex<Vec<FileInfo>>>,
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

        let (thumb_tx, thumb_rx) = channel::<Job>();
        let thumb_rx = Arc::new(Mutex::new(thumb_rx));
        for _ in 0..2 {
            spawn_decode_worker(
                Arc::clone(&thumb_rx),
                Arc::clone(&decoder),
                max_thumb_px,
                max_slide_px,
                events_tx.clone(),
                Arc::clone(&wakeup),
            );
        }

        let (slide_tx, slide_rx) = channel::<Job>();
        spawn_decode_worker(
            Arc::new(Mutex::new(slide_rx)),
            Arc::clone(&decoder),
            max_thumb_px,
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
                watcher: Mutex::new(None),
                dates,
            },
            events_rx,
        )
    }

    /// Walk `root` recursively on a fresh thread. Batches of found
    /// files stream out as FilesInserted events with sorted-insert
    /// indices; the shell mirrors the inserts to keep the same order.
    /// The folder is then watched: adds, removals, and edits under it
    /// keep flowing as events until the next scan.
    pub fn scan(&self, root: PathBuf) {
        self.files.lock().unwrap().clear();
        *self.watcher.lock().unwrap() = None;
        self.start_watcher(&root);

        let events_tx = self.events_tx.clone();
        let wakeup = Arc::clone(&self.wakeup);
        let files = Arc::clone(&self.files);
        let dates = Arc::clone(&self.dates);
        thread::spawn(move || {
            let mut pending: Vec<FileInfo> = Vec::new();
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
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
                        stack.push(path);
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
                        if pending.len() >= SCAN_BATCH {
                            flush(&files, &mut pending, &events_tx, &wakeup);
                        }
                    }
                }
            }
            flush(&files, &mut pending, &events_tx, &wakeup);
            let total = files.lock().unwrap().len();
            let _ = events_tx.send(Event::ScanDone { total });
            wakeup();
        });
    }

    fn start_watcher(&self, root: &Path) {
        use notify::Watcher;
        let events_tx = self.events_tx.clone();
        let wakeup = Arc::clone(&self.wakeup);
        let files = Arc::clone(&self.files);
        let dates = Arc::clone(&self.dates);
        let handler = move |result: Result<notify::Event, notify::Error>| {
            let Ok(event) = result else { return };
            for path in event.paths {
                apply_fs_change(&path, &files, &events_tx, &wakeup, &dates);
            }
        };
        let Ok(mut watcher) = notify::recommended_watcher(handler) else {
            return;
        };
        if watcher.watch(root, notify::RecursiveMode::Recursive).is_ok() {
            *self.watcher.lock().unwrap() = Some(watcher);
        }
    }

    pub fn request_thumb(&self, path: PathBuf) {
        let _ = self.thumb_tx.send(Job::Thumb(path));
    }

    pub fn request_slide(&self, path: PathBuf) {
        let _ = self.slide_tx.send(Job::Slide(path));
    }

    pub fn max_thumb_px(&self) -> i32 {
        self.max_thumb_px
    }
}

fn flush<I>(
    files: &Arc<Mutex<Vec<FileInfo>>>,
    pending: &mut Vec<FileInfo>,
    events_tx: &Sender<Event<I>>,
    wakeup: &Arc<dyn Fn() + Send + Sync>,
) {
    if pending.is_empty() {
        return;
    }
    let mut inserts = Vec::with_capacity(pending.len());
    {
        let mut sorted = files.lock().unwrap();
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
    if !inserts.is_empty() && events_tx.send(Event::FilesInserted(inserts)).is_ok() {
        wakeup();
    }
}

/// Watcher callback: reconcile one path against the canonical list.
/// Existence on disk decides between insert, change, and removal,
/// which absorbs create/modify/rename event ambiguity.
fn apply_fs_change<I>(
    path: &Path,
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
        let existing = sorted.binary_search_by(|p| file_cmp(&p.path, path));
        match (meta, existing) {
            (Some(meta), Err(index)) => {
                let info = make_info(&meta);
                sorted.insert(index, info.clone());
                Some(Event::FilesInserted(vec![(index, info)]))
            }
            (Some(meta), Ok(index)) => {
                let info = make_info(&meta);
                sorted[index] = info.clone();
                Some(Event::FileChanged(info))
            }
            (None, Ok(index)) => {
                sorted.remove(index);
                Some(Event::FilesRemoved(vec![path.to_path_buf()]))
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
    max_thumb_px: i32,
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
        let (path, decoded, is_slide) = match job {
            Job::Thumb(p) => {
                let image = decoder.decode_thumb(&p, max_thumb_px);
                (p, image, false)
            }
            Job::Slide(p) => {
                let image = decoder.decode(&p, max_slide_px);
                (p, image, true)
            }
        };
        let event = match decoded {
            Some(image) if is_slide => Event::SlideReady { path, image },
            Some(image) => Event::ThumbReady { path, image },
            None => Event::DecodeFailed { path },
        };
        if events_tx.send(event).is_err() {
            return;
        }
        wakeup();
    });
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
}

pub mod cache;
pub mod playlist;
pub use cache::LruCache;
pub use playlist::Playlist;
