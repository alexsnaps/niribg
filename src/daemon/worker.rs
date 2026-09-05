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

/// Render an image wallpaper for one output at `target` physical pixels.
/// Colour-only wallpapers never reach the worker.
pub struct Job {
    pub token: u64,
    /// The output's render generation when this job was queued; a result
    /// whose generation is stale by the time it lands is dropped.
    pub generation: u64,
    pub output: String,
    pub target: Size,
    pub path: PathBuf,
    pub mode: Mode,
    /// Letterbox / transparency fill.
    pub fill: Color,
    /// Box-blur radius (at the downscaled resolution) for the overview
    /// backdrop.
    pub blur_radius: u32,
    /// How much to darken the blurred backdrop, `0.0..=0.5`.
    pub blur_dim: f64,
    /// Crossfade to the result rather than snapping.
    pub fade: bool,
}

/// A finished render. Both buffers are `Xrgb8888` BGRA at `size`, stride
/// `size.w * 4`: `sharp` is the wallpaper, `blurred` its dimmed overview
/// backdrop.
#[derive(Debug)]
pub struct Rendered {
    pub size: Size,
    pub sharp: Vec<u8>,
    pub blurred: Vec<u8>,
}

/// The outcome of one [`Job`], tagged with its identifiers so the loop can
/// route it back to the right output and pending `set`.
pub struct JobResult {
    pub token: u64,
    pub generation: u64,
    pub output: String,
    pub fade: bool,
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
            generation,
            output,
            target,
            path,
            mode,
            fill,
            blur_radius,
            blur_dim,
            fade,
        } = job;
        let outcome = render_image(&path, mode, fill, target, blur_radius, blur_dim)
            .map_err(|e| format!("{e:#}"));
        if results
            .send(JobResult {
                token,
                generation,
                output,
                fade,
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

/// Decode `path`, reject anything over [`MAX_PIXELS`], compose it for `target`
/// under `mode` over `fill`, and build its blurred/dimmed overview backdrop.
fn render_image(
    path: &PathBuf,
    mode: Mode,
    fill: Color,
    target: Size,
    blur_radius: u32,
    blur_dim: f64,
) -> Result<Rendered> {
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

    let sharp = render::compose(&image, mode, fill, target);
    let blurred = render::blur_dim(&sharp, target, blur_radius, blur_dim);
    Ok(Rendered {
        size: target,
        sharp,
        blurred,
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

    fn job(token: u64, path: PathBuf, target: Size, mode: Mode) -> Job {
        Job {
            token,
            generation: token,
            output: "o".into(),
            target,
            path,
            mode,
            fill: Color::BLACK,
            blur_radius: 8,
            blur_dim: 0.15,
            fade: false,
        }
    }

    #[test]
    fn renders_a_png_to_target_size_with_both_buffers() {
        let png = scratch("wall.png");
        write_png(&png, 8, 8, Rgba([0, 128, 255, 255]));

        let (tx, chan) = calloop::channel::channel::<JobResult>();
        let worker = Worker::spawn(tx);
        worker.submit(Job {
            output: "eDP-1".into(),
            ..job(7, png, Size::new(16, 12), Mode::Stretch)
        });

        let res = chan.recv().expect("a result");
        assert_eq!(res.token, 7);
        assert_eq!(res.output, "eDP-1");
        let r = res.outcome.expect("ok");
        assert_eq!(r.size, Size::new(16, 12));
        assert_eq!(r.sharp.len(), 16 * 12 * 4);
        assert_eq!(r.blurred.len(), r.sharp.len());
        // stretch of a solid image → sharp is that colour in BGRA
        assert_eq!(&r.sharp[0..4], &[255, 128, 0, 255]);
        // blurred+dimmed differs from sharp (dim pulls the channels down)
        assert_ne!(r.blurred, r.sharp);
        assert!(r.blurred[0] < r.sharp[0], "blurred not dimmed");
    }

    #[test]
    fn missing_file_is_an_err_result_not_a_panic() {
        let (tx, chan) = calloop::channel::channel::<JobResult>();
        let worker = Worker::spawn(tx);
        worker.submit(job(
            1,
            PathBuf::from("/no/such/wallpaper.png"),
            Size::new(2, 2),
            Mode::Fill,
        ));
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
        let r = render_image(&png, Mode::Fit, Color::BLACK, Size::new(10, 10), 6, 0.2).unwrap();
        assert_eq!(r.sharp.len(), 10 * 10 * 4);
        assert_eq!(r.blurred.len(), 10 * 10 * 4);
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
            worker.submit(job(token, png, Size::new(2, 2), Mode::Stretch));
        }
        for expected in 0..5 {
            assert_eq!(chan.recv().unwrap().token, expected);
        }
    }
}
