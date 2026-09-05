//! The image worker: one background thread that decodes and composes
//! wallpapers so the event loop never blocks on a large file.
//!
//! Jobs go in over a plain `mpsc` channel; [`JobResult`]s come back over a
//! `calloop::channel` so the loop is woken to apply them. Colour-only
//! wallpapers do not come here — the loop fills those inline with
//! [`render::solid`].

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use anyhow::{Context, Result, bail};

use super::render::{self, Size};
use crate::color::Color;
use crate::config::Mode;

/// Hard ceiling on decoded image area (width × height). Larger images are
/// rejected rather than risking an out-of-memory abort.
pub const MAX_PIXELS: u64 = 100_000_000;

/// Render `source` for one output at `target` physical pixels.
pub struct Job {
    pub token: u64,
    pub output: String,
    pub target: Size,
    pub path: PathBuf,
    pub mode: Mode,
    /// Letterbox / transparency fill.
    pub fill: Color,
}

/// A finished render: `pixels` is `Xrgb8888` BGRA at `size`, stride
/// `size.w * 4`.
#[derive(Debug)]
pub struct Rendered {
    pub size: Size,
    pub pixels: Vec<u8>,
}

/// The outcome of one [`Job`], tagged with its identifiers so the loop can
/// route it back to the right output and pending `set`.
pub struct JobResult {
    pub token: u64,
    pub output: String,
    pub outcome: Result<Rendered, String>,
}

/// Handle to the worker thread. Dropping it closes the job channel; the
/// thread then finishes its current job and exits, and drop joins it.
pub struct Worker {
    tx: Option<Sender<Job>>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    /// Spawn the worker. Results are delivered on `results`.
    #[must_use]
    pub fn spawn(results: calloop::channel::Sender<JobResult>) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let handle = std::thread::Builder::new()
            .name("niribg-render".to_owned())
            .spawn(move || worker_loop(&rx, &results))
            .expect("spawning the render worker thread");
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    /// Queue a job. A dropped worker (should not happen while the daemon
    /// runs) logs and discards.
    pub fn submit(&self, job: Job) {
        if let Some(tx) = &self.tx {
            if tx.send(job).is_err() {
                tracing::error!("render worker has exited; job dropped");
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.tx.take(); // close the channel so worker_loop's recv() ends
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn worker_loop(rx: &Receiver<Job>, results: &calloop::channel::Sender<JobResult>) {
    while let Ok(job) = rx.recv() {
        let Job {
            token,
            output,
            target,
            path,
            mode,
            fill,
        } = job;
        let outcome = render_image(&path, mode, fill, target).map_err(|e| format!("{e:#}"));
        if results
            .send(JobResult {
                token,
                output,
                outcome,
            })
            .is_err()
        {
            break; // the event loop is gone
        }
    }
}

/// Reject an image whose pixel area exceeds [`MAX_PIXELS`].
fn check_area(w: u32, h: u32) -> Result<()> {
    let area = u64::from(w) * u64::from(h);
    if area > MAX_PIXELS {
        bail!("image is {w}x{h} ({area} pixels); the limit is {MAX_PIXELS}");
    }
    Ok(())
}

/// Decode `path`, reject anything over [`MAX_PIXELS`], and compose it for
/// `target` under `mode` over `fill`.
fn render_image(path: &PathBuf, mode: Mode, fill: Color, target: Size) -> Result<Rendered> {
    let (w, h) = image::image_dimensions(path)
        .with_context(|| format!("reading image dimensions of {}", path.display()))?;
    check_area(w, h).with_context(|| format!("{}", path.display()))?;

    let image = image::ImageReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("sniffing the format of {}", path.display()))?
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?
        .into_rgba8();

    let pixels = render::compose(&image, mode, fill, target);
    Ok(Rendered {
        size: target,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch(name: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "niribg-worker-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn write_png(path: &PathBuf, w: u32, h: u32, px: Rgba<u8>) {
        RgbaImage::from_pixel(w, h, px).save(path).unwrap();
    }

    #[test]
    fn renders_a_png_to_target_size() {
        let png = scratch("wall.png");
        write_png(&png, 8, 8, Rgba([0, 128, 255, 255]));

        let (tx, chan) = calloop::channel::channel::<JobResult>();
        let worker = Worker::spawn(tx);
        worker.submit(Job {
            token: 7,
            output: "eDP-1".into(),
            target: Size::new(4, 3),
            path: png,
            mode: Mode::Stretch,
            fill: Color::BLACK,
        });

        let res = chan.recv().expect("a result");
        assert_eq!(res.token, 7);
        assert_eq!(res.output, "eDP-1");
        let rendered = res.outcome.expect("ok");
        assert_eq!(rendered.size, Size::new(4, 3));
        assert_eq!(rendered.pixels.len(), 4 * 3 * 4);
        // stretch of a solid image → every pixel is that colour in BGRA
        assert_eq!(&rendered.pixels[0..4], &[255, 128, 0, 255]);
    }

    #[test]
    fn missing_file_is_an_err_result_not_a_panic() {
        let (tx, chan) = calloop::channel::channel::<JobResult>();
        let worker = Worker::spawn(tx);
        worker.submit(Job {
            token: 1,
            output: "x".into(),
            target: Size::new(2, 2),
            path: PathBuf::from("/no/such/wallpaper.png"),
            mode: Mode::Fill,
            fill: Color::BLACK,
        });
        let res = chan.recv().unwrap();
        let err = res.outcome.unwrap_err();
        assert!(err.contains("/no/such/wallpaper.png"), "{err}");
    }

    #[test]
    fn area_guard() {
        assert!(check_area(10_000, 10_000).is_ok()); // exactly 100 MP
        assert!(check_area(10_001, 10_000).is_err());
        assert!(check_area(u32::MAX, u32::MAX).is_err());
        let msg = check_area(20_000, 20_000).unwrap_err().to_string();
        assert!(msg.contains("400000000"), "{msg}");
    }

    #[test]
    fn a_normal_image_passes_and_composes() {
        let png = scratch("ok.png");
        write_png(&png, 64, 64, Rgba([1, 2, 3, 255]));
        let r = render_image(&png, Mode::Fit, Color::BLACK, Size::new(10, 10)).unwrap();
        assert_eq!(r.pixels.len(), 10 * 10 * 4);
    }

    #[test]
    fn worker_joins_cleanly_on_drop() {
        let (tx, _chan) = calloop::channel::channel::<JobResult>();
        let worker = Worker::spawn(tx);
        drop(worker); // must not hang
    }

    #[test]
    fn processes_jobs_in_order() {
        let (tx, chan) = calloop::channel::channel::<JobResult>();
        let worker = Worker::spawn(tx);
        for token in 0..5 {
            let png = scratch(&format!("j{token}.png"));
            write_png(&png, 4, 4, Rgba([token as u8, 0, 0, 255]));
            worker.submit(Job {
                token,
                output: "o".into(),
                target: Size::new(2, 2),
                path: png,
                mode: Mode::Stretch,
                fill: Color::BLACK,
            });
        }
        for expected in 0..5 {
            assert_eq!(chan.recv().unwrap().token, expected);
        }
    }
}
