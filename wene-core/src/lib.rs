//! Portable core: file scanning, sorted insertion, decode scheduling.
//! No AppKit, no objc2. The shell supplies the decoder and a wakeup
//! callback; the core owns all worker threads.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use lexical_sort::natural_lexical_cmp;

/// Platform decoder. The macOS shell implements this with Image I/O.
/// `max_px` caps the long edge; EXIF orientation must be applied.
pub trait ImageDecoder: Send + Sync + 'static {
    type Image: Send + 'static;
    fn decode(&self, path: &Path, max_px: i32) -> Option<Self::Image>;
}

pub enum Event<I> {
    /// Files inserted into the sorted list. Each entry is the index
    /// the file was inserted at (indices are valid when applied in
    /// order) plus its path.
    FilesInserted(Vec<(usize, PathBuf)>),
    ScanDone { total: usize },
    ThumbReady { path: PathBuf, image: I },
    SlideReady { path: PathBuf, image: I },
    DecodeFailed { path: PathBuf },
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
}

impl<I: Send + 'static> Engine<I> {
    pub fn start<D: ImageDecoder<Image = I>>(
        decoder: D,
        max_thumb_px: i32,
        max_slide_px: i32,
        wakeup: impl Fn() + Send + Sync + 'static,
    ) -> (Self, Receiver<Event<I>>) {
        let decoder = Arc::new(decoder);
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
            },
            events_rx,
        )
    }

    /// Walk `root` recursively on a fresh thread. Batches of found
    /// files stream out as FilesInserted events with sorted-insert
    /// indices; the shell mirrors the inserts to keep the same order.
    pub fn scan(&self, root: PathBuf) {
        let events_tx = self.events_tx.clone();
        let wakeup = Arc::clone(&self.wakeup);
        thread::spawn(move || {
            let mut sorted: Vec<PathBuf> = Vec::new();
            let mut pending: Vec<PathBuf> = Vec::new();
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
                        pending.push(path);
                        if pending.len() >= SCAN_BATCH {
                            flush(&mut sorted, &mut pending, &events_tx, &wakeup);
                        }
                    }
                }
            }
            flush(&mut sorted, &mut pending, &events_tx, &wakeup);
            let _ = events_tx.send(Event::ScanDone {
                total: sorted.len(),
            });
            wakeup();
        });
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
    sorted: &mut Vec<PathBuf>,
    pending: &mut Vec<PathBuf>,
    events_tx: &Sender<Event<I>>,
    wakeup: &Arc<dyn Fn() + Send + Sync>,
) {
    if pending.is_empty() {
        return;
    }
    let mut inserts = Vec::with_capacity(pending.len());
    for path in pending.drain(..) {
        let index = match sorted.binary_search_by(|p| file_cmp(p, &path)) {
            Ok(i) | Err(i) => i,
        };
        sorted.insert(index, path.clone());
        inserts.push((index, path));
    }
    if events_tx.send(Event::FilesInserted(inserts)).is_ok() {
        wakeup();
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
        let (path, max_px, is_slide) = match job {
            Job::Thumb(p) => (p, max_thumb_px, false),
            Job::Slide(p) => (p, max_slide_px, true),
        };
        let event = match decoder.decode(&path, max_px) {
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
}

pub mod playlist;
pub use playlist::Playlist;
