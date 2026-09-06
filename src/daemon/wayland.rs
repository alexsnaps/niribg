// SPDX-License-Identifier: GPL-3.0-or-later
//! Wayland: one `background` layer-shell surface per output, painted from an
//! shm slot pool.
//!
//! SCTK 0.21 has no fractional-scale support, so `wp_fractional_scale_v1` and
//! `wp_viewporter` are wired by hand: the buffer is rendered at physical
//! pixels and the viewport's destination is the logical size. A compositor
//! that lacks those globals falls back to integer `wl_surface` buffer scale.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use smithay_client_toolkit::compositor::{
    CompositorHandler, CompositorState, FrameCallbackData, Region,
};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use wayland_client::backend::ObjectId;
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use super::anim::Anim;
use super::ipc::{SetDispatch, SetRequest};
use super::render::{self, Size};
use super::{DaemonState, PendingSet};
use crate::config::{Config, DEFAULT_SLOT, OutputConfig, Resolved};
use crate::proto::{BlurStatus, LogicalOutput, OutputStatus, Source, Status, WireReply};
use crate::state::State;

/// `scale_120 == 120` means a 1.0 scale factor (niri reports fractional scale
/// in 1/120ths).
const SCALE_UNIT: u32 = 120;

/// SCTK globals plus our per-output surface state.
pub(super) struct Wayland {
    pub registry_state: RegistryState,
    pub output_state: OutputState,
    pub shm: Shm,
    pub pool: SlotPool,
    pub compositor: CompositorState,
    pub layer_shell: LayerShell,
    pub frac_mgr: Option<WpFractionalScaleManagerV1>,
    pub viewporter: Option<WpViewporter>,
    pub qh: QueueHandle<DaemonState>,
    pub outputs: HashMap<ObjectId, OutputEntry>,
}

impl Wayland {
    /// Bind the globals we need. A missing *required* global is a hard error;
    /// fractional-scale / viewporter are optional (integer-scale fallback).
    pub(super) fn bind(globals: &GlobalList, qh: &QueueHandle<DaemonState>) -> Result<Self> {
        let compositor = CompositorState::bind(globals, qh)
            .context("compositor does not support wl_compositor")?;
        let layer_shell = LayerShell::bind(globals, qh)
            .context("compositor does not support zwlr_layer_shell_v1")?;
        let shm = Shm::bind(globals, qh).context("compositor does not support wl_shm")?;
        let pool = SlotPool::new(1920 * 1200 * 4, &shm).context("creating the shm slot pool")?;

        let frac_mgr = globals
            .bind::<WpFractionalScaleManagerV1, _, _>(qh, 1..=1, FracMgrData)
            .ok();
        let viewporter = globals
            .bind::<WpViewporter, _, _>(qh, 1..=1, ViewporterData)
            .ok();
        if frac_mgr.is_none() || viewporter.is_none() {
            tracing::warn!(
                "compositor lacks wp_fractional_scale / wp_viewporter; \
                 falling back to integer buffer scale"
            );
        }

        Ok(Self {
            registry_state: RegistryState::new(globals),
            output_state: OutputState::new(globals, qh),
            shm,
            pool,
            compositor,
            layer_shell,
            frac_mgr,
            viewporter,
            qh: qh.clone(),
            outputs: HashMap::new(),
        })
    }
}

/// Per-output wallpaper surface.
pub(super) struct OutputEntry {
    pub name: String,
    /// Held so the objects are destroyed when the output goes away.
    _wl_output: WlOutput,
    pub surface: WlSurface,
    pub layer: LayerSurface,
    pub viewport: Option<WpViewport>,
    _frac: Option<WpFractionalScaleV1>,
    _input_region: Option<Region>,
    _opaque_region: Option<Region>,
    /// Fractional scale in 1/120ths; integer fallback stores `n * 120`.
    pub scale_120: u32,
    /// Whether this output uses `wp_viewporter` (physical buffer, logical
    /// destination) rather than an integer buffer scale.
    pub fractional: bool,
    pub logical: Option<(u32, u32)>,
    pub configured: bool,
    pub resolved: Resolved,
    pub status: PaintStatus,
    /// Physical size of the buffer currently on screen, if any.
    pub painted: Option<Size>,
    /// The last render's buffers, retained so an overview toggle / `set` can
    /// crossfade between them. Both are BGRA at `buffers_size`. `Rc` so a live
    /// [`Transition`] can share them instead of copying ~20 MiB per fade.
    pub sharp: Option<Rc<Vec<u8>>>,
    pub blurred: Option<Rc<Vec<u8>>>,
    pub buffers_size: Option<Size>,
    /// Which of the two buffers is (or is fading toward being) on screen.
    pub showing: Showing,
    /// An in-flight crossfade, driven by frame callbacks.
    pub transition: Option<Transition>,
    /// A frame callback is outstanding (don't stack them).
    pub frame_pending: bool,
    /// Bumped before each render job; a result with a stale generation is
    /// dropped (M3).
    pub generation: u64,
}

/// Which composited buffer an output is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Showing {
    Sharp,
    Blurred,
}

/// An in-flight crossfade: displayed pixels are `blend(from, to, anim.eased())`
/// until `anim.done()`, then `to` exactly. `from`/`to` are BGRA at `size`,
/// shared (`Rc`) with the output's retained `sharp` / `blurred` where they
/// match — only an interrupted fade's `from` is a freshly-owned snapshot.
pub(super) struct Transition {
    pub from: Rc<Vec<u8>>,
    pub to: Rc<Vec<u8>>,
    pub size: Size,
    pub anim: Anim,
}

/// Pixel source for [`DaemonState::commit_frame`].
pub(super) enum Frame<'a> {
    /// A whole, already-composed BGRA buffer to copy into the shm canvas.
    Whole(&'a [u8]),
    /// Blend the output's live [`Transition`] endpoints at `t` directly into
    /// the shm canvas (no intermediate buffer). Requires `entry.transition`.
    Fade(f32),
}

/// Where an output's wallpaper currently stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PaintStatus {
    Pending,
    Painted,
    Failed(String),
}

impl OutputEntry {
    /// Physical buffer size for the current logical size and scale.
    fn physical(&self) -> Option<Size> {
        let (lw, lh) = self.logical?;
        if self.fractional {
            let s = u64::from(self.scale_120);
            let w = (u64::from(lw) * s).div_ceil(u64::from(SCALE_UNIT)) as u32;
            let h = (u64::from(lh) * s).div_ceil(u64::from(SCALE_UNIT)) as u32;
            Some(Size::new(w.max(1), h.max(1)))
        } else {
            let n = (self.scale_120 / SCALE_UNIT).max(1);
            Some(Size::new(lw * n, lh * n))
        }
    }
}

// --- helper methods on DaemonState ----------------------------------------

impl DaemonState {
    /// Create a `background` layer surface for a newly-announced output.
    pub(super) fn create_output_surface(
        &mut self,
        qh: &QueueHandle<DaemonState>,
        output: WlOutput,
    ) {
        let id = output.id();
        if self.wl.outputs.contains_key(&id) {
            return;
        }

        let name = self
            .wl
            .output_state
            .info(&output)
            .and_then(|i| i.name)
            .unwrap_or_else(|| format!("output-{}", id.protocol_id()));

        let surface = self.wl.compositor.create_surface(qh);

        let input_region = Region::new(&self.wl.compositor).ok();
        if let Some(r) = &input_region {
            surface.set_input_region(Some(r.wl_region())); // empty → clicks pass through
        }

        let layer = self.wl.layer_shell.create_layer_surface(
            qh,
            surface.clone(),
            Layer::Background,
            Some("niribg"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);

        let frac = self.wl.frac_mgr.as_ref().map(|m| {
            m.get_fractional_scale(
                &surface,
                qh,
                FracData {
                    surface: surface.clone(),
                },
            )
        });
        let viewport = self
            .wl
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(&surface, qh, ViewportData));
        let fractional = frac.is_some() && viewport.is_some();

        layer.commit();

        let resolved = self
            .config
            .resolve(&name)
            .map(|(r, _)| r)
            .unwrap_or_else(|_| Resolved {
                path: None,
                mode: crate::config::Mode::default(),
                color: crate::color::Color::BLACK,
            });

        tracing::info!(output = %name, fractional, "output connected");
        self.wl.outputs.insert(
            id,
            OutputEntry {
                name,
                _wl_output: output,
                surface,
                layer,
                viewport,
                _frac: frac,
                _input_region: input_region,
                _opaque_region: None,
                scale_120: SCALE_UNIT,
                fractional,
                logical: None,
                configured: false,
                resolved,
                status: PaintStatus::Pending,
                painted: None,
                sharp: None,
                blurred: None,
                buffers_size: None,
                showing: Showing::Sharp,
                transition: None,
                frame_pending: false,
                generation: 0,
            },
        );
    }

    pub(super) fn destroy_output_surface(&mut self, output: &WlOutput) {
        if let Some(entry) = self.wl.outputs.remove(&output.id()) {
            tracing::info!(output = %entry.name, "output disconnected");
        }
    }

    fn entry_id_for_surface(&self, surface: &WlSurface) -> Option<ObjectId> {
        self.wl
            .outputs
            .iter()
            .find(|(_, e)| e.surface.id() == surface.id())
            .map(|(id, _)| id.clone())
    }

    fn entry_id_for_layer(&self, layer: &LayerSurface) -> Option<ObjectId> {
        let sid = layer.wl_surface().id();
        self.wl
            .outputs
            .iter()
            .find(|(_, e)| e.layer.wl_surface().id() == sid)
            .map(|(id, _)| id.clone())
    }

    fn on_fractional_scale(&mut self, surface: &WlSurface, scale: u32) {
        let Some(id) = self.entry_id_for_surface(surface) else {
            return;
        };
        let entry = self.wl.outputs.get_mut(&id).expect("id just looked up");
        if entry.scale_120 == scale || scale == 0 {
            return;
        }
        entry.scale_120 = scale;
        self.repaint(&id);
    }

    /// Integer-scale fallback (only meaningful when there is no fractional
    /// scale object for this surface).
    pub(super) fn on_integer_scale(&mut self, surface: &WlSurface, factor: i32) {
        let Some(id) = self.entry_id_for_surface(surface) else {
            return;
        };
        let entry = self.wl.outputs.get_mut(&id).expect("id just looked up");
        if entry.fractional {
            return;
        }
        let s = u32::try_from(factor.max(1)).unwrap_or(1) * SCALE_UNIT;
        if entry.scale_120 == s {
            return;
        }
        entry.scale_120 = s;
        self.repaint(&id);
    }

    pub(super) fn on_layer_configure(
        &mut self,
        layer: &LayerSurface,
        configure: &LayerSurfaceConfigure,
    ) {
        let Some(id) = self.entry_id_for_layer(layer) else {
            return;
        };
        let (w, h) = configure.new_size;
        let entry = self.wl.outputs.get_mut(&id).expect("id just looked up");
        if w != 0 && h != 0 {
            entry.logical = Some((w, h));
        }
        entry.configured = true;
        self.repaint(&id);
    }

    pub(super) fn on_layer_closed(&mut self, layer: &LayerSurface) {
        if let Some(id) = self.entry_id_for_layer(layer) {
            self.wl.outputs.remove(&id);
        }
    }

    /// (Re)paint one output for its current resolved source, logical size and
    /// scale. A colour source is filled inline; an image source is queued on
    /// the worker (and a colour placeholder is shown until the first result
    /// lands, so the surface maps immediately).
    ///
    /// Instant repaint — configure / scale / hotplug.
    pub(super) fn repaint(&mut self, id: &ObjectId) {
        self.repaint_tagged(id, 0, true);
    }

    /// `token` is `0` for a plain repaint or a `set` token when the reply is
    /// deferred on the result. `instant` skips the crossfade (first paint,
    /// scale change, `--no-fade`, `transition_ms = 0`).
    pub(super) fn repaint_tagged(&mut self, id: &ObjectId, token: u64, instant: bool) {
        let Some(entry) = self.wl.outputs.get_mut(id) else {
            return;
        };
        if !entry.configured {
            return;
        }
        let Some(phys) = entry.physical() else {
            return;
        };
        if phys.w == 0 || phys.h == 0 {
            return;
        }

        // Snapshot everything we need, then release the borrow.
        let resolved = entry.resolved.clone();
        let name = entry.name.clone();
        let first_paint = entry.status == PaintStatus::Pending;
        entry.generation += 1;
        let generation = entry.generation;

        let (radius, dim) = (self.config.blur.radius, self.config.blur.dim);

        match resolved.path {
            None => {
                let color = resolved.color;
                if let Some(e) = self.wl.outputs.get_mut(id) {
                    e.sharp = Some(Rc::new(render::solid(color, phys)));
                    e.blurred = Some(Rc::new(render::solid(color.dimmed(dim), phys)));
                    e.buffers_size = Some(phys);
                }
                self.present_target(id, instant);
                self.note_result(token, &name, None);
            }
            Some(path) => {
                tracing::trace!(output = %name, first_paint, ?phys, "queueing image job");
                if first_paint {
                    // Show the fill colour immediately so the surface maps;
                    // the image swaps in when the worker returns.
                    self.commit_frame(id, Frame::Whole(&render::solid(resolved.color, phys)), phys);
                }
                self.worker.submit(super::worker::Job {
                    token,
                    generation,
                    output: name,
                    target: phys,
                    path,
                    mode: resolved.mode,
                    fill: resolved.color,
                    blur_radius: radius,
                    blur_dim: dim,
                    fade: !instant,
                });
            }
        }
    }

    /// Overview opened or closed: crossfade every output between its sharp
    /// and blurred buffer.
    pub(super) fn set_overview_open(&mut self, open: bool) {
        if self.overview_open == open {
            return;
        }
        self.overview_open = open;
        tracing::debug!(open, "overview toggled");
        if !self.config.blur.enable {
            return; // nothing to swap
        }
        let ids: Vec<ObjectId> = self.wl.outputs.keys().cloned().collect();
        for id in ids {
            self.present_target(&id, false);
        }
    }

    /// Show the buffer that matches the current overview state — blurred when
    /// the overview is open and blur is enabled, else sharp. `instant`
    /// commits it directly; otherwise start (or retarget) a crossfade.
    fn present_target(&mut self, id: &ObjectId, instant: bool) {
        let Some(entry) = self.wl.outputs.get(id) else {
            return;
        };
        let Some(size) = entry.buffers_size else {
            return;
        };
        let want_blur = self.overview_open && self.config.blur.enable;
        let (target, showing) = match (want_blur, &entry.blurred, &entry.sharp) {
            (true, Some(b), _) => (b.clone(), Showing::Blurred),
            (_, _, Some(s)) => (s.clone(), Showing::Sharp),
            _ => return, // nothing composited yet
        };

        let dur = Duration::from_millis(u64::from(self.config.transition_ms));
        let current = self.current_displayed(id);
        let can_fade =
            !instant && !dur.is_zero() && current.len() == target.len() && !current.is_empty();

        if !can_fade {
            if let Some(e) = self.wl.outputs.get_mut(id) {
                e.transition = None;
                e.showing = showing;
            }
            self.commit_frame(id, Frame::Whole(&target), size);
            return;
        }

        let Some(e) = self.wl.outputs.get_mut(id) else {
            return;
        };
        tracing::debug!(output = %e.name, ?showing, ms = dur.as_millis(), "crossfade start");
        e.showing = showing;
        e.transition = Some(Transition {
            from: current,
            to: target,
            size,
            anim: Anim::new(dur),
        });
        // `Fade(0.0)` copies `from` (== the old `current`) into the canvas.
        self.commit_frame(id, Frame::Fade(0.0), size); // re-requests a frame while a transition is live
    }

    /// The exact pixels currently on screen for `id` — a shared handle to the
    /// shown buffer, or a freshly-owned mid-fade blend when a crossfade is
    /// interrupted. Empty if nothing is composited yet.
    fn current_displayed(&self, id: &ObjectId) -> Rc<Vec<u8>> {
        let Some(entry) = self.wl.outputs.get(id) else {
            return Rc::new(Vec::new());
        };
        if let Some(tr) = &entry.transition {
            return Rc::new(render::blend(
                tr.from.as_slice(),
                tr.to.as_slice(),
                tr.anim.eased(),
            ));
        }
        match entry.showing {
            Showing::Blurred => entry.blurred.clone().unwrap_or_default(),
            Showing::Sharp => entry.sharp.clone().unwrap_or_default(),
        }
    }

    /// Advance one crossfade a frame (called from the frame callback).
    fn advance_transition(&mut self, id: &ObjectId) {
        let Some(entry) = self.wl.outputs.get(id) else {
            return;
        };
        let Some(tr) = &entry.transition else {
            return;
        };
        let done = tr.anim.done();
        let size = tr.size;
        // `Fade(1.0)` copies `to` exactly (blend's `t >= 1.0` fast path).
        let t = if done { 1.0 } else { tr.anim.eased() };
        tracing::trace!(t, done, "crossfade frame");
        // Commit before clearing the transition — `Fade` reads its endpoints.
        self.commit_frame(id, Frame::Fade(t), size);
        if done {
            if let Some(e) = self.wl.outputs.get_mut(id) {
                e.transition = None;
            }
        }
    }

    /// Fill a fresh shm slot per `frame`, `size` (BGRA), and present it. If a
    /// crossfade is live, also requests the next frame callback.
    ///
    /// [`Frame::Fade`] blends the output's live [`Transition`] endpoints
    /// straight into the shm canvas — no per-frame intermediate `Vec`.
    fn commit_frame(&mut self, id: &ObjectId, frame: Frame<'_>, size: Size) {
        let Some(entry) = self.wl.outputs.get(id) else {
            return;
        };
        let Some((lw, lh)) = entry.logical else {
            return;
        };
        let surface = entry.surface.clone();
        let viewport = entry.viewport.clone();
        let use_viewport = entry.fractional && viewport.is_some();
        let buffer_scale = if use_viewport {
            1
        } else {
            i32::try_from(entry.scale_120 / SCALE_UNIT)
                .unwrap_or(1)
                .max(1)
        };
        let stride = size.w as i32 * 4;
        // Surface geometry (opaque region, buffer scale, viewport dest) only
        // changes when the size does — skip re-sending it every crossfade
        // frame.
        let geometry_changed = entry.painted != Some(size);
        // The settling frame of a fade: paint it, but don't chain another
        // frame callback — `advance_transition` clears the transition next.
        let terminal_fade = matches!(frame, Frame::Fade(t) if t >= 1.0);

        let buffer = match self.wl.pool.create_buffer(
            size.w as i32,
            size.h as i32,
            stride,
            wl_shm::Format::Xrgb8888,
        ) {
            Ok((buffer, canvas)) => {
                match frame {
                    Frame::Whole(pixels) => {
                        let n = canvas.len().min(pixels.len());
                        canvas[..n].copy_from_slice(&pixels[..n]);
                    }
                    // Disjoint borrow: `canvas` is `self.wl.pool`, the endpoints
                    // are `self.wl.outputs` — read them back here so no blended
                    // `Vec` is ever allocated per fade frame.
                    Frame::Fade(t) => {
                        match self.wl.outputs.get(id).and_then(|e| e.transition.as_ref()) {
                            Some(tr) => {
                                render::blend_into(canvas, tr.from.as_slice(), tr.to.as_slice(), t);
                            }
                            None => {
                                tracing::error!("commit_frame(Fade) with no live transition");
                                return;
                            }
                        }
                    }
                }
                buffer
            }
            Err(err) => {
                let reason = format!("shm: {err}");
                tracing::error!(error = %err, "allocating shm buffer");
                if let Some(entry) = self.wl.outputs.get_mut(id) {
                    entry.status = PaintStatus::Failed(reason);
                }
                return;
            }
        };

        let opaque = if geometry_changed {
            let r = Region::new(&self.wl.compositor).ok();
            if let Some(r) = &r {
                r.add(0, 0, lw as i32, lh as i32);
                surface.set_opaque_region(Some(r.wl_region()));
            }
            surface.set_buffer_scale(buffer_scale);
            if use_viewport {
                if let Some(vp) = &viewport {
                    vp.set_destination(lw as i32, lh as i32);
                }
            }
            r
        } else {
            None
        };

        if let Err(e) = buffer.attach_to(&surface) {
            tracing::error!(error = ?e, "attaching buffer");
            return;
        }
        surface.damage_buffer(0, 0, size.w as i32, size.h as i32);

        // While a crossfade is live, ask for the next frame callback *before*
        // committing so the request is latched with this commit.
        let want_frame = if let Some(e) = self.wl.outputs.get_mut(id) {
            if geometry_changed {
                e._opaque_region = opaque;
            }
            e.painted = Some(size);
            e.status = PaintStatus::Painted;
            let want = e.transition.is_some() && !e.frame_pending && !terminal_fade;
            if want {
                e.frame_pending = true;
            }
            want
        } else {
            false
        };
        if want_frame {
            surface.frame(&self.wl.qh, FrameCallbackData(surface.clone()));
        }
        surface.commit();
    }

    /// Apply one finished [`worker::JobResult`].
    pub(super) fn apply_rendered(&mut self, result: super::worker::JobResult) {
        let id = self
            .wl
            .outputs
            .iter()
            .find(|(_, e)| e.name == result.output)
            .map(|(id, _)| id.clone());

        let fresh = id
            .as_ref()
            .and_then(|id| self.wl.outputs.get(id))
            .is_some_and(|e| e.generation == result.generation);

        let error = match result.outcome {
            Ok(rendered) => match &id {
                Some(id) if fresh => {
                    if let Some(e) = self.wl.outputs.get_mut(id) {
                        e.buffers_size = Some(rendered.size);
                        e.sharp = Some(Rc::new(rendered.sharp));
                        e.blurred = Some(Rc::new(rendered.blurred));
                    }
                    self.present_target(id, !result.fade);
                    None
                }
                Some(_) => {
                    tracing::debug!(output = %result.output, "discarding stale render");
                    None
                }
                None => {
                    tracing::debug!(output = %result.output, "render for a vanished output");
                    None
                }
            },
            Err(reason) => {
                tracing::warn!(output = %result.output, error = %reason, "render failed");
                if let Some(id) = &id {
                    if let Some(e) = self.wl.outputs.get_mut(id) {
                        e.status = PaintStatus::Failed(reason.clone());
                    }
                }
                Some(reason)
            }
        };

        self.note_result(result.token, &result.output, error);
    }

    /// Build `niribg get`'s payload from the live output list plus any
    /// configured-but-disconnected named slots.
    pub(super) fn live_status(&self) -> Status {
        let source_of = |resolved: &Resolved| match &resolved.path {
            Some(p) => Source::Image {
                path: p.to_string_lossy().into_owned(),
            },
            None => Source::Color,
        };

        let mut rows: Vec<OutputStatus> = self
            .wl
            .outputs
            .values()
            .map(|e| OutputStatus {
                name: e.name.clone(),
                logical: e.logical.map(|(w, h)| LogicalOutput {
                    width: w,
                    height: h,
                    scale: f64::from(e.scale_120) / f64::from(SCALE_UNIT),
                }),
                source: source_of(&e.resolved),
                mode: e.resolved.mode,
                color: e.resolved.color,
                overridden: self.config.output.contains_key(&e.name),
                loaded: e.status == PaintStatus::Painted,
                error: match &e.status {
                    PaintStatus::Failed(m) => Some(m.clone()),
                    _ => None,
                },
            })
            .collect();

        let connected: HashSet<&str> = self.wl.outputs.values().map(|e| e.name.as_str()).collect();
        for name in self.config.output.keys() {
            if name == DEFAULT_SLOT || connected.contains(name.as_str()) {
                continue;
            }
            if let Ok((resolved, _)) = self.config.resolve(name) {
                rows.push(OutputStatus {
                    name: name.clone(),
                    logical: None,
                    source: source_of(&resolved),
                    mode: resolved.mode,
                    color: resolved.color,
                    overridden: true,
                    loaded: false,
                    error: None,
                });
            }
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));

        Status {
            pid: std::process::id(),
            niri_connected: self.niri_connected,
            blur: BlurStatus {
                enable: self.config.blur.enable,
                radius: self.config.blur.radius,
                dim: self.config.blur.dim,
                active: self.overview_open && self.config.blur.enable,
            },
            transition_ms: self.config.transition_ms,
            outputs: rows,
        }
    }

    /// Handle a validated `set`: persist (unless `--no-persist`), re-resolve
    /// the affected connected outputs, and queue their repaints under one
    /// token. Returns `Deferred` when jobs were queued, `Now` when there is
    /// nothing connected to paint (the change is still stored).
    pub(super) fn set_wallpaper(&mut self, req: SetRequest) -> SetDispatch {
        let slot = req.slot.unwrap_or_else(|| DEFAULT_SLOT.to_string());
        let instant = !req.fade;
        let patch = OutputConfig {
            path: req.path,
            mode: req.mode,
            color: req.color,
        };

        if req.persist {
            self.state.apply_set(&slot, patch);
            if let Err(e) = self.state.save(&self.state_path) {
                tracing::warn!(error = %format!("{e:#}"), "could not persist state.json");
            }
            self.rebuild_effective();
        } else {
            // Ephemeral: fold the patch straight into the effective config.
            // A later `reload`/`reset` drops it (it is not in state.json).
            crate::config::merge_output_patch(
                self.config.output.entry(slot.clone()).or_default(),
                patch,
            );
        }

        let targets = self.slot_targets(&slot);
        if targets.is_empty() {
            let note = if slot == DEFAULT_SLOT {
                "stored; no outputs are currently connected".to_string()
            } else {
                format!("stored; {slot} is not currently connected")
            };
            return SetDispatch::Now(WireReply::ok(serde_json::json!({ "note": note })));
        }

        let token = self.next_token;
        self.next_token += 1;
        self.pending_sets.insert(
            token,
            PendingSet {
                stream: None,
                outstanding: targets.iter().map(|(_, n)| n.clone()).collect(),
                failures: Vec::new(),
                note: None,
            },
        );

        for (id, name) in targets {
            if let Ok((resolved, _)) = self.config.resolve(&name) {
                if let Some(e) = self.wl.outputs.get_mut(&id) {
                    let new_image = e.resolved.path != resolved.path && resolved.path.is_some();
                    e.resolved = resolved;
                    if new_image {
                        // Show the fill colour immediately, swap to the image
                        // when the worker returns.
                        e.status = PaintStatus::Pending;
                    }
                }
            }
            self.repaint_tagged(&id, token, instant);
        }
        SetDispatch::Deferred(token)
    }

    /// Re-read `config.toml` (+ `state.json`, cleared first when `reset`),
    /// re-resolve every connected output and repaint the ones that changed.
    pub(super) fn reload_config(&mut self, reset: bool) -> Result<usize> {
        if reset {
            State::clear(&self.state_path)?;
        }
        let (disk_config, read_from) = Config::load(self.explicit_config.as_deref())?;
        let state = State::load(&self.state_path);
        self.disk_config = disk_config;
        self.state = state;
        self.rebuild_effective();
        if let Some(p) = &read_from {
            tracing::info!(path = %p.display(), "reloaded config");
        }

        let ids: Vec<ObjectId> = self.wl.outputs.keys().cloned().collect();
        let mut changed = 0;
        for id in ids {
            let Some(name) = self.wl.outputs.get(&id).map(|e| e.name.clone()) else {
                continue;
            };
            let Ok((resolved, _)) = self.config.resolve(&name) else {
                continue;
            };
            let repaint = match self.wl.outputs.get_mut(&id) {
                Some(e) if e.resolved != resolved => {
                    let new_image = e.resolved.path != resolved.path && resolved.path.is_some();
                    e.resolved = resolved;
                    if new_image {
                        e.status = PaintStatus::Pending;
                    }
                    changed += 1;
                    true
                }
                _ => false,
            };
            if repaint {
                self.repaint_tagged(&id, 0, false); // reload changes crossfade
            }
        }
        Ok(changed)
    }

    fn rebuild_effective(&mut self) {
        let mut effective = self.disk_config.clone();
        self.state.apply_to(&mut effective);
        self.config = effective;
    }

    /// The connected outputs a `set` on `slot` applies to: the matching
    /// connector, or — for the default slot — every output without its own
    /// `[output."NAME"]` table in the effective config.
    fn slot_targets(&self, slot: &str) -> Vec<(ObjectId, String)> {
        self.wl
            .outputs
            .iter()
            .filter(|(_, e)| {
                if slot == DEFAULT_SLOT {
                    !self.config.output.contains_key(&e.name)
                } else {
                    e.name == slot
                }
            })
            .map(|(id, e)| (id.clone(), e.name.clone()))
            .collect()
    }

    /// Feed a job outcome into any pending `set` under `token` (0 = none).
    fn note_result(&mut self, token: u64, output: &str, error: Option<String>) {
        if token == 0 {
            return;
        }
        if let Some(p) = self.pending_sets.get_mut(&token) {
            // Drain by name, or — if the output vanished mid-flight — one
            // arbitrary outstanding entry so the set can still complete.
            let drained = if p.outstanding.remove(output) {
                Some(output.to_string())
            } else if let Some(any) = p.outstanding.iter().next().cloned() {
                p.outstanding.remove(&any);
                Some(any)
            } else {
                None
            };
            if let (Some(name), Some(reason)) = (drained, error) {
                p.failures.push((name, reason));
            }
        }
        self.try_finish_pending(token);
    }
}

// --- SCTK handler impls --------------------------------------------------

impl CompositorHandler for DaemonState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &WlSurface,
        new_factor: i32,
    ) {
        self.on_integer_scale(surface, new_factor);
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: wayland_client::protocol::wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &WlSurface,
        _time: u32,
    ) {
        if let Some(id) = self.entry_id_for_surface(surface) {
            if let Some(e) = self.wl.outputs.get_mut(&id) {
                e.frame_pending = false;
            }
            self.advance_transition(&id);
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }
}

impl OutputHandler for DaemonState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.wl.output_state
    }

    fn new_output(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, output: WlOutput) {
        self.create_output_surface(qh, output);
    }

    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}

    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        self.destroy_output_surface(&output);
    }
}

impl LayerShellHandler for DaemonState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        self.on_layer_closed(layer);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        self.on_layer_configure(layer, &configure);
    }
}

impl ShmHandler for DaemonState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.wl.shm
    }
}

impl ProvidesRegistryState for DaemonState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.wl.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_registry!(DaemonState);
smithay_client_toolkit::delegate_dispatch2!(DaemonState);

// --- hand-wired fractional-scale / viewporter --------------------------

/// Udata for `wp_fractional_scale_manager_v1` (no events).
pub(super) struct FracMgrData;
/// Udata for `wp_viewporter` (no events).
pub(super) struct ViewporterData;
/// Udata for `wp_viewport` (no events).
pub(super) struct ViewportData;
/// Udata for a per-surface `wp_fractional_scale_v1`; carries the surface so
/// the `preferred_scale` event can be routed to the right output.
pub(super) struct FracData {
    surface: WlSurface,
}

impl Dispatch2<WpFractionalScaleManagerV1, DaemonState> for FracMgrData {
    fn event(
        &self,
        _state: &mut DaemonState,
        _proxy: &WpFractionalScaleManagerV1,
        _event: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _conn: &Connection,
        _qh: &QueueHandle<DaemonState>,
    ) {
    }
}

impl Dispatch2<WpViewporter, DaemonState> for ViewporterData {
    fn event(
        &self,
        _state: &mut DaemonState,
        _proxy: &WpViewporter,
        _event: <WpViewporter as Proxy>::Event,
        _conn: &Connection,
        _qh: &QueueHandle<DaemonState>,
    ) {
    }
}

impl Dispatch2<WpViewport, DaemonState> for ViewportData {
    fn event(
        &self,
        _state: &mut DaemonState,
        _proxy: &WpViewport,
        _event: <WpViewport as Proxy>::Event,
        _conn: &Connection,
        _qh: &QueueHandle<DaemonState>,
    ) {
    }
}

impl Dispatch2<WpFractionalScaleV1, DaemonState> for FracData {
    fn event(
        &self,
        state: &mut DaemonState,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _conn: &Connection,
        _qh: &QueueHandle<DaemonState>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.on_fractional_scale(&self.surface, scale);
        }
    }
}
