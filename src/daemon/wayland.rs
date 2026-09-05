//! Wayland: one `background` layer-shell surface per output, painted from an
//! shm slot pool.
//!
//! SCTK 0.21 has no fractional-scale support, so `wp_fractional_scale_v1` and
//! `wp_viewporter` are wired by hand: the buffer is rendered at physical
//! pixels and the viewport's destination is the logical size. A compositor
//! that lacks those globals falls back to integer `wl_surface` buffer scale.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
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
    /// `token` is `0` for a plain repaint (configure / scale / hotplug /
    /// reload) or a `set` token when the reply is deferred on the result.
    pub(super) fn repaint(&mut self, id: &ObjectId) {
        self.repaint_tagged(id, 0);
    }

    pub(super) fn repaint_tagged(&mut self, id: &ObjectId, token: u64) {
        let Some(entry) = self.wl.outputs.get(id) else {
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

        match entry.resolved.path.clone() {
            None => {
                let color = entry.resolved.color;
                let name = entry.name.clone();
                self.commit_pixels(id, &render::solid(color, phys), phys);
                self.note_result(token, &name, None);
            }
            Some(path) => {
                let first_paint = entry.status == PaintStatus::Pending;
                let color = entry.resolved.color;
                let mode = entry.resolved.mode;
                let name = entry.name.clone();
                tracing::trace!(output = %name, first_paint, ?phys, "queueing image job");
                if first_paint {
                    // Show something immediately so the layer surface maps.
                    self.commit_pixels(id, &render::solid(color, phys), phys);
                }
                self.worker.submit(super::worker::Job {
                    token,
                    output: name,
                    target: phys,
                    path,
                    mode,
                    fill: color,
                });
            }
        }
    }

    /// Copy `pixels` (BGRA, `size`) into a fresh shm slot and present it.
    fn commit_pixels(&mut self, id: &ObjectId, pixels: &[u8], size: Size) {
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

        let buffer = match self.wl.pool.create_buffer(
            size.w as i32,
            size.h as i32,
            stride,
            wl_shm::Format::Xrgb8888,
        ) {
            Ok((buffer, canvas)) => {
                let n = canvas.len().min(pixels.len());
                canvas[..n].copy_from_slice(&pixels[..n]);
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

        let opaque = Region::new(&self.wl.compositor).ok();
        if let Some(r) = &opaque {
            r.add(0, 0, lw as i32, lh as i32);
            surface.set_opaque_region(Some(r.wl_region()));
        }
        surface.set_buffer_scale(buffer_scale);
        if use_viewport {
            if let Some(vp) = &viewport {
                vp.set_destination(lw as i32, lh as i32);
            }
        }
        if let Err(e) = buffer.attach_to(&surface) {
            tracing::error!(error = ?e, "attaching buffer");
            return;
        }
        surface.damage_buffer(0, 0, size.w as i32, size.h as i32);
        surface.commit();

        if let Some(e) = self.wl.outputs.get_mut(id) {
            e._opaque_region = opaque;
            e.painted = Some(size);
            e.status = PaintStatus::Painted;
            tracing::debug!(output = %e.name, w = size.w, h = size.h, "committed wallpaper");
        }
    }

    /// Apply one finished [`worker::JobResult`].
    pub(super) fn apply_rendered(&mut self, result: super::worker::JobResult) {
        let id = self
            .wl
            .outputs
            .iter()
            .find(|(_, e)| e.name == result.output)
            .map(|(id, _)| id.clone());

        let error = match result.outcome {
            Ok(rendered) => match &id {
                Some(id) if self.output_wants(id, rendered.size) => {
                    self.commit_pixels(id, &rendered.pixels, rendered.size);
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

    /// Whether a render of `size` still matches what output `id` currently
    /// needs (its source is still an image and the physical size is
    /// unchanged). A cheap staleness guard until M3's generation counter.
    fn output_wants(&self, id: &ObjectId, size: Size) -> bool {
        self.wl
            .outputs
            .get(id)
            .is_some_and(|e| e.resolved.path.is_some() && e.physical() == Some(size))
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
            niri_connected: false, // M2
            blur: BlurStatus {
                enable: self.config.blur.enable,
                radius: self.config.blur.radius,
                dim: self.config.blur.dim,
                active: false, // M2
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
            self.repaint_tagged(&id, token);
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
                self.repaint(&id);
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
        _surface: &WlSurface,
        _time: u32,
    ) {
        // Animations arrive in M3.
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
