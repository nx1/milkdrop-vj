//! Pattern 1 Multi-Deck Renderer: One Shared OpenGL Context per `libprojectM` Deck.
//!
//! How Pattern 1 works:
//! - Each `RenderDeck` owns its own OpenGL 3.3 Core context that **shares its
//!   texture namespace** with the main window's OpenGL context (`.with_sharing(...)`).
//! - Each `RenderDeck` also owns an off-screen WGL pbuffer surface (default `1280x720`
//!   during development on this laptop) and a shared `GL_TEXTURE_2D` (`output_texture`).
//! - Cross-context synchronization (OpenGL 3.3 Core Spec, Chapter 5 / Section 5.3):
//!   Shared textures are visible across contexts in the same share group, but each
//!   context has an independent GPU command stream. `glFlush()` alone does NOT guarantee
//!   that this deck's `glCopyTexSubImage2D` finishes before the main window samples the
//!   texture. Therefore, after copying into `output_texture`, we insert a GPU fence sync
//!   (`glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0)`) followed immediately by `glFlush()`.
//!   Before sampling `output_texture`, the main context calls `glWaitSync` (GPU-side wait
//!   only — zero CPU stall) and deletes the sync object.
//! - Blank-deck detection (`2A-1`):
//!   Instead of reading back full 720p/1080p frames every frame, each deck maintains a tiny
//!   reusable `32x18` FBO and a `2,304`-byte Pixel Buffer Object (`GL_PIXEL_PACK_BUFFER`).
//!   Every `500 ms` (`2x/sec`), it reads the *previous* asynchronous PBO transfer (already
//!   complete — zero GPU stall) and starts the next `32x18` blit + async PBO readback.
//!   If Rec. 709 average luma stays below `3.0 / 255.0` continuously for `>= 2.0 seconds`,
//!   `is_blank_warning` becomes `true`.

use std::collections::VecDeque;
use std::ffi::CStr;
use std::fs::{self, File};
use std::io::BufWriter;
use std::num::NonZeroU32;
use std::path::Path;
use std::time::{Duration, Instant};

use glow::HasContext;
use glutin::config::Config;
use glutin::context::{
    ContextApi, ContextAttributesBuilder, NotCurrentContext, PossiblyCurrentContext, Version,
};
use glutin::display::Display;
use glutin::prelude::*;
use glutin::surface::{PbufferSurface, Surface, SurfaceAttributesBuilder};

use crate::gl_log::GlErrorLogger;
use crate::gl_state::{reset_read_to_default_fbo, GlStateSnapshot};
use milkdrop_vj::deck::Deck;

/// Downscaled sample dimensions for asynchronous blank-deck detection (`32x18` = 576 pixels).
const BLANK_SAMPLE_W: i32 = 32;
const BLANK_SAMPLE_H: i32 = 18;
const BLANK_SAMPLE_BYTES: usize = (BLANK_SAMPLE_W * BLANK_SAMPLE_H * 4) as usize;
/// Sample blank-deck luma twice per second (`500 ms` interval).
const BLANK_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
/// Rec. 709 average luma (`0.0..=255.0`) below which a frame counts as "dark".
/// Broken `shader_body` presets measured `0.0/255`; `3.0` catches pure black plus
/// tiny dithering/rounding noise without false-triggering dim valid presets.
pub const BLANK_LUMA_THRESHOLD: f32 = 3.0;
/// How long average luma must stay below `BLANK_LUMA_THRESHOLD` before warning (`2.0 s`),
/// so legitimate brief fade-to-black transitions do not trigger false alarms.
pub const BLANK_HOLD_SECONDS: f32 = 2.0;

/// Split timing returned by `RenderDeck::render_to_texture()` for one frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeckRenderTiming {
    /// CPU time spent inside `add_pcm_stereo()`, `projectm_opengl_render_frame()`,
    /// `glCopyTexSubImage2D`, and fence insertion (note: on Deck A with VSync enabled,
    /// NVIDIA's driver may absorb idle VSync wait inside this wall-clock span).
    pub cpu_submit_ms: f64,
    /// Extra time waiting inside `glFinish()` (only non-zero when `--measure-gpu` is active).
    pub gpu_wait_ms: f64,
    /// Non-blocking `GL_TIME_ELAPSED` GPU execution time from 2 frames ago (zero VSync wait,
    /// zero CPU stall). `None` during the first 2 warm-up frames.
    pub gpu_timer_ms: Option<f64>,
    /// CPU time spent in the `500 ms` async downscaled blank-deck check, when it ran this frame.
    pub blank_check_ms: Option<f64>,
}

impl DeckRenderTiming {
    #[allow(dead_code)]
    pub fn total_ms(self) -> f64 {
        self.cpu_submit_ms + self.gpu_wait_ms
    }
}

/// Snapshot of a deck's asynchronous blank-deck detector state for UI display.
#[derive(Debug, Clone, Copy, Default)]
pub struct BlankDeckStatus {
    /// Most recent Rec. 709 average luma (`0.0..=255.0`) across the `32x18` sample.
    pub avg_luma: f32,
    /// True once a first async readback sample has completed.
    pub has_sample: bool,
    /// Seconds the deck has stayed continuously below `BLANK_LUMA_THRESHOLD`.
    pub dark_seconds: f32,
    /// True when `dark_seconds >= BLANK_HOLD_SECONDS` (`2.0 s`).
    pub is_blank_warning: bool,
}

/// Reusable GPU resources for zero-stall `32x18` PBO blank-deck detection.
struct BlankDetector {
    src_fbo: glow::NativeFramebuffer,
    dst_fbo: glow::NativeFramebuffer,
    dst_tex: glow::NativeTexture,
    pbo: glow::NativeBuffer,
    pbo_in_flight: bool,
    last_sample_time: Instant,
    avg_luma: f32,
    has_sample: bool,
    dark_since: Option<Instant>,
}

impl BlankDetector {
    fn new(
        gl: &glow::Context,
        output_texture: glow::NativeTexture,
        name: &str,
    ) -> Result<Self, String> {
        unsafe {
            let src_fbo = gl
                .create_framebuffer()
                .map_err(|e| format!("[{name}] Failed to create blank src FBO: {e}"))?;
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(src_fbo));
            gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(output_texture),
                0,
            );

            let dst_tex = gl
                .create_texture()
                .map_err(|e| format!("[{name}] Failed to create blank 32x18 texture: {e}"))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(dst_tex));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                BLANK_SAMPLE_W,
                BLANK_SAMPLE_H,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);

            let dst_fbo = gl
                .create_framebuffer()
                .map_err(|e| format!("[{name}] Failed to create blank 32x18 FBO: {e}"))?;
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(dst_fbo));
            gl.framebuffer_texture_2d(
                glow::DRAW_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(dst_tex),
                0,
            );

            let pbo = gl
                .create_buffer()
                .map_err(|e| format!("[{name}] Failed to create blank PBO: {e}"))?;
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(pbo));
            gl.buffer_data_size(
                glow::PIXEL_PACK_BUFFER,
                BLANK_SAMPLE_BYTES as i32,
                glow::STREAM_READ,
            );
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);

            Ok(Self {
                src_fbo,
                dst_fbo,
                dst_tex,
                pbo,
                pbo_in_flight: false,
                // Trigger first sample quickly (~100 ms after startup) instead of waiting 500 ms.
                last_sample_time: Instant::now()
                    .checked_sub(Duration::from_millis(400))
                    .unwrap_or_else(Instant::now),
                avg_luma: 255.0,
                has_sample: false,
                dark_since: None,
            })
        }
    }

    fn reset_dark_timer(&mut self) {
        self.dark_since = None;
    }

    fn status(&self) -> BlankDeckStatus {
        let dark_seconds = self
            .dark_since
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        BlankDeckStatus {
            avg_luma: self.avg_luma,
            has_sample: self.has_sample,
            dark_seconds,
            is_blank_warning: self.has_sample && dark_seconds >= BLANK_HOLD_SECONDS,
        }
    }

    /// Runs once every `500 ms`: reads the previous async PBO transfer (already finished on GPU)
    /// and kicks off a new downscaled `32x18` blit + async PBO `glReadPixels`.
    fn maybe_sample(
        &mut self,
        gl: &glow::Context,
        src_width: u32,
        src_height: u32,
    ) -> Option<f64> {
        let now = Instant::now();
        if now.duration_since(self.last_sample_time) < BLANK_SAMPLE_INTERVAL {
            return None;
        }
        self.last_sample_time = now;

        let t0 = Instant::now();
        unsafe {
            // 1. Collect previous sample from PBO (500 ms old -> DMA already finished, zero stall).
            if self.pbo_in_flight {
                gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(self.pbo));
                let ptr = gl.map_buffer_range(
                    glow::PIXEL_PACK_BUFFER,
                    0,
                    BLANK_SAMPLE_BYTES as i32,
                    glow::MAP_READ_BIT,
                );
                if !ptr.is_null() {
                    let bytes = std::slice::from_raw_parts(ptr, BLANK_SAMPLE_BYTES);
                    let mut luma_sum = 0.0_f64;
                    let pixel_count = (BLANK_SAMPLE_W * BLANK_SAMPLE_H) as f64;
                    for px in bytes.chunks_exact(4) {
                        // Rec. 709 perceived luminance on 0..=255 RGB.
                        luma_sum +=
                            0.2126 * px[0] as f64 + 0.7152 * px[1] as f64 + 0.0722 * px[2] as f64;
                    }
                    self.avg_luma = (luma_sum / pixel_count) as f32;
                    self.has_sample = true;
                    if self.avg_luma < BLANK_LUMA_THRESHOLD {
                        if self.dark_since.is_none() {
                            self.dark_since = Some(now);
                        }
                    } else {
                        self.dark_since = None;
                    }
                    gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
                }
                gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            }

            // 2. Downscale full output_texture -> 32x18 FBO using hardware linear blit.
            // Disable scissor test so a preset's scissor box can never clip the 32x18 blit
            // (`saved.restore()` restores the deck's original scissor state right afterward).
            gl.disable(glow::SCISSOR_TEST);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(self.src_fbo));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.dst_fbo));
            gl.blit_framebuffer(
                0,
                0,
                src_width as i32,
                src_height as i32,
                0,
                0,
                BLANK_SAMPLE_W,
                BLANK_SAMPLE_H,
                glow::COLOR_BUFFER_BIT,
                glow::LINEAR,
            );

            // 3. Kick off asynchronous GPU -> PBO readback of the tiny 32x18 image (returns immediately).
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(self.dst_fbo));
            gl.read_buffer(glow::COLOR_ATTACHMENT0);
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(self.pbo));
            gl.read_pixels(
                0,
                0,
                BLANK_SAMPLE_W,
                BLANK_SAMPLE_H,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::BufferOffset(0),
            );
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
            self.pbo_in_flight = true;
        }

        Some(t0.elapsed().as_secs_f64() * 1000.0)
    }

    fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_buffer(self.pbo);
            gl.delete_framebuffer(self.dst_fbo);
            gl.delete_framebuffer(self.src_fbo);
            gl.delete_texture(self.dst_tex);
        }
    }
}

/// One isolated MilkDrop deck owning its own shared OpenGL context, off-screen
/// pbuffer surface, shared output texture, fence sync, GPU timer ring, blank detector,
/// and `libprojectM` instance.
pub struct RenderDeck {
    name: String,
    width: u32,
    height: u32,
    /// Owned `libprojectM` instance (wrapped in `Option` so `destroy()` can drop it
    /// explicitly while this deck's OpenGL context is current).
    deck: Option<Deck>,
    /// Shared `GL_TEXTURE_2D` visible to BOTH this deck's context and the main window context.
    output_texture: glow::NativeTexture,
    /// Pending GPU fence inserted after `glCopyTexSubImage2D` + `glFlush()` so the main/projector
    /// context can call `glWaitSync` before sampling `output_texture`.
    pending_sync: Option<glow::NativeFence>,
    /// Previous fences waiting to finish on the GPU before `glDeleteSync` (so we never delete
    /// a fence while `glWaitSync` on the main context still references it).
    retired_syncs: VecDeque<glow::NativeFence>,
    /// 3-slot `GL_TIME_ELAPSED` timer query ring (measures true per-deck GPU time without
    /// VSync wait or CPU stalls by reading the query issued 2 frames earlier).
    gpu_timer_queries: [glow::NativeQuery; 3],
    gpu_timer_in_flight: [bool; 3],
    gpu_timer_cursor: usize,
    /// Asynchronous `32x18` PBO blank-deck detector (`2x/sec`, zero CPU/GPU stall).
    blank_detector: BlankDetector,
    pbuffer: Surface<PbufferSurface>,
    context: PossiblyCurrentContext,
    gl: glow::Context,
}

impl RenderDeck {
    /// Creates a new off-screen `RenderDeck` sharing OpenGL textures with `shared_with`.
    ///
    /// Note: Calling this switches the thread's active OpenGL context to the new deck's
    /// context during initialization; the caller should re-bind the main context afterward.
    pub fn new(
        name: &str,
        gl_display: &Display,
        pbuffer_config: &Config,
        shared_with: &PossiblyCurrentContext,
        width: u32,
        height: u32,
        logger: &mut GlErrorLogger,
    ) -> Result<Self, String> {
        let w_nz = NonZeroU32::new(width).ok_or_else(|| "Deck width must be > 0".to_string())?;
        let h_nz = NonZeroU32::new(height).ok_or_else(|| "Deck height must be > 0".to_string())?;

        // 1. Create the off-screen WGL pbuffer surface.
        let pbuffer_attrs = SurfaceAttributesBuilder::<PbufferSurface>::new().build(w_nz, h_nz);
        let pbuffer = unsafe {
            gl_display
                .create_pbuffer_surface(pbuffer_config, &pbuffer_attrs)
                .map_err(|e| format!("[{name}] Failed to create {width}x{height} pbuffer: {e}"))?
        };

        // 2. Create an OpenGL 3.3 Core context that shares textures with the main context.
        let ctx_attrs = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .with_sharing(shared_with)
            .build(None);

        let not_current: NotCurrentContext = unsafe {
            gl_display
                .create_context(pbuffer_config, &ctx_attrs)
                .map_err(|e| format!("[{name}] Failed to create shared OpenGL context: {e}"))?
        };

        // 3. Make this deck's context current on its pbuffer before initializing libprojectM.
        let context = not_current
            .make_current(&pbuffer)
            .map_err(|e| format!("[{name}] Failed to make deck context current: {e}"))?;

        let gl = unsafe {
            glow::Context::from_loader_function_cstr(|symbol: &CStr| {
                gl_display.get_proc_address(symbol) as *const _
            })
        };

        // 4. Allocate the shared RGBA8 texture that the main window / compositor will sample.
        let output_texture = unsafe {
            let tex = gl
                .create_texture()
                .map_err(|e| format!("[{name}] Failed to create shared output texture: {e}"))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
            tex
        };

        // 5. Create reusable async 32x18 PBO blank-deck detector and 3-slot GL_TIME_ELAPSED ring.
        let blank_detector = BlankDetector::new(&gl, output_texture, name)?;
        let gpu_timer_queries = unsafe {
            let q0 = gl
                .create_query()
                .map_err(|e| format!("[{name}] Failed to create GL timer query 0: {e}"))?;
            let q1 = gl
                .create_query()
                .map_err(|e| format!("[{name}] Failed to create GL timer query 1: {e}"))?;
            let q2 = gl
                .create_query()
                .map_err(|e| format!("[{name}] Failed to create GL timer query 2: {e}"))?;
            [q0, q1, q2]
        };

        // 6. Create this deck's own independent `libprojectM` instance on this context.
        let deck = Deck::new(width as usize, height as usize)?;

        // Check GL errors right after deck initialization.
        logger.check_frame(&gl, 0, &format!("{name}-init"));

        Ok(Self {
            name: name.to_string(),
            width,
            height,
            deck: Some(deck),
            output_texture,
            pending_sync: None,
            retired_syncs: VecDeque::with_capacity(8),
            gpu_timer_queries,
            gpu_timer_in_flight: [false; 3],
            gpu_timer_cursor: 0,
            blank_detector,
            pbuffer,
            context,
            gl,
        })
    }

    /// Returns the OpenGL texture handle shared with the main window context.
    pub fn output_texture(&self) -> glow::NativeTexture {
        self.output_texture
    }

    /// Returns this deck's latest cross-context GPU fence (inserted after `glCopyTexSubImage2D`
    /// + `glFlush`), so the main/projector context can call GPU-only `glWaitSync` before sampling
    /// `output_texture`. `RenderDeck` retains ownership and retires signaled fences on its own context.
    pub fn pending_sync(&self) -> Option<glow::NativeFence> {
        self.pending_sync
    }

    /// Returns the latest asynchronous blank-deck detector status for UI display.
    pub fn blank_status(&self) -> BlankDeckStatus {
        self.blank_detector.status()
    }

    /// Makes this deck's OpenGL context current on the calling thread.
    pub fn make_current(&self) -> Result<(), String> {
        self.context
            .make_current(&self.pbuffer)
            .map_err(|e| format!("[{}] Failed to make context current: {e}", self.name))
    }

    /// Loads a `.milk` preset onto this deck (making its context current first).
    pub fn load_preset(&mut self, path: &Path, smooth_transition: bool) -> Result<(), String> {
        self.make_current()?;
        self.blank_detector.reset_dark_timer();
        let deck = self
            .deck
            .as_ref()
            .ok_or_else(|| format!("[{}] Deck already destroyed", self.name))?;
        deck.load_preset(path, smooth_transition)
    }

    /// Updates `libprojectM`'s FPS hint for this deck.
    pub fn set_fps(&self, fps: i32) {
        if let Some(deck) = self.deck.as_ref() {
            deck.set_fps(fps);
        }
    }

    /// Feeds PCM audio, renders one frame inside this deck's shared context with full
    /// OpenGL state save/restore, copies the pbuffer GPU-to-GPU into `output_texture`,
    /// runs the `500 ms` async blank-deck PBO check, inserts an OpenGL 5.3 fence sync +
    /// `glFlush()`, and logs any OpenGL errors.
    pub fn render_to_texture(
        &mut self,
        interleaved_lr_pcm: &[f32],
        frame_index: u64,
        measure_gpu: bool,
        logger: &mut GlErrorLogger,
    ) -> Result<DeckRenderTiming, String> {
        let t_start = Instant::now();
        self.make_current()?;

        // Move previous frame's fence into `retired_syncs`, and delete any retired fences
        // that the GPU has already signaled (non-blocking `timeout = 0` check).
        if let Some(prev_sync) = self.pending_sync.take() {
            self.retired_syncs.push_back(prev_sync);
        }
        while let Some(&oldest) = self.retired_syncs.front() {
            let status = unsafe { self.gl.client_wait_sync(oldest, 0, 0) };
            if status == glow::ALREADY_SIGNALED || status == glow::CONDITION_SATISFIED {
                self.retired_syncs.pop_front();
                unsafe {
                    self.gl.delete_sync(oldest);
                }
            } else {
                break;
            }
        }

        let deck = self
            .deck
            .as_ref()
            .ok_or_else(|| format!("[{}] Deck already destroyed", self.name))?;

        // Save GL state before handing control to libprojectM.
        let saved = GlStateSnapshot::save(&self.gl);

        // Read back the GL_TIME_ELAPSED query from 2 frames ago if ready (non-blocking, zero stall).
        let read_idx = (self.gpu_timer_cursor + 1) % 3;
        let gpu_timer_ms = if self.gpu_timer_in_flight[read_idx] {
            let q = self.gpu_timer_queries[read_idx];
            unsafe {
                if self
                    .gl
                    .get_query_parameter_u32(q, glow::QUERY_RESULT_AVAILABLE)
                    != 0
                {
                    let ns = self.gl.get_query_parameter_u64(q, glow::QUERY_RESULT);
                    self.gpu_timer_in_flight[read_idx] = false;
                    Some(ns as f64 / 1_000_000.0)
                } else {
                    None
                }
            }
        } else {
            None
        };

        // Start a fresh GL_TIME_ELAPSED query around pure deck GPU rendering + texture copy.
        let write_idx = self.gpu_timer_cursor;
        let write_q = self.gpu_timer_queries[write_idx];
        let started_timer = if !self.gpu_timer_in_flight[write_idx] {
            unsafe {
                self.gl.begin_query(glow::TIME_ELAPSED, write_q);
            }
            true
        } else {
            false
        };

        // Feed this deck's private audio buffer and render one MilkDrop frame to FBO 0 (pbuffer).
        deck.add_pcm_stereo(interleaved_lr_pcm);
        deck.render_frame();

        // Reset READ_FRAMEBUFFER back to FBO 0 so glCopyTexSubImage2D never reads one of
        // libprojectM's internal intermediate FBOs, then copy into our shared texture.
        reset_read_to_default_fbo(&self.gl);
        unsafe {
            self.gl
                .bind_texture(glow::TEXTURE_2D, Some(self.output_texture));
            self.gl.copy_tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                0,
                0,
                self.width as i32,
                self.height as i32,
            );
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            if started_timer {
                self.gl.end_query(glow::TIME_ELAPSED);
                self.gpu_timer_in_flight[write_idx] = true;
            }
        }
        self.gpu_timer_cursor = (self.gpu_timer_cursor + 1) % 3;

        // Periodic (500 ms) asynchronous 32x18 PBO blank-deck check (zero pipeline stall).
        let blank_check_ms = self
            .blank_detector
            .maybe_sample(&self.gl, self.width, self.height);

        // OpenGL 3.3 Core Spec (Section 5.3): insert a fence sync after writing the shared
        // texture and immediately flush so the fence is queued for the main context's glWaitSync.
        let fence = unsafe {
            self.gl
                .fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)
                .map_err(|e| format!("[{}] glFenceSync failed: {e}", self.name))?
        };
        self.pending_sync = Some(fence);

        saved.restore(&self.gl);
        let cpu_submit_ms = t_start.elapsed().as_secs_f64() * 1000.0;

        // Always flush so `fence` enters the GPU command stream before another context calls glWaitSync.
        // In diagnostic `--measure-gpu` mode we also time `glFinish()`; normal mode never stalls CPU.
        let gpu_wait_ms = if measure_gpu {
            let t_gpu = Instant::now();
            unsafe {
                self.gl.finish();
            }
            t_gpu.elapsed().as_secs_f64() * 1000.0
        } else {
            unsafe {
                self.gl.flush();
            }
            0.0
        };

        logger.check_frame(&self.gl, frame_index, &self.name);
        Ok(DeckRenderTiming {
            cpu_submit_ms,
            gpu_wait_ms,
            gpu_timer_ms,
            blank_check_ms,
        })
    }

    /// Reads this deck's `output_texture` via `glReadPixels` and saves it as a PNG file.
    #[allow(dead_code)]
    pub fn capture_png(&self, out_path: &Path, logger: &mut GlErrorLogger) -> Result<(), String> {
        self.make_current()?;

        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create PNG folder {}: {e}", parent.display()))?;
        }

        let w = self.width as usize;
        let h = self.height as usize;
        let mut rgba = vec![0u8; w * h * 4];

        let saved = GlStateSnapshot::save(&self.gl);
        unsafe {
            let fbo = self
                .gl
                .create_framebuffer()
                .map_err(|e| format!("[{}] Failed to create capture FBO: {e}", self.name))?;
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fbo));
            self.gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(self.output_texture),
                0,
            );
            self.gl.read_buffer(glow::COLOR_ATTACHMENT0);
            self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            self.gl.read_pixels(
                0,
                0,
                self.width as i32,
                self.height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut rgba)),
            );
            self.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            self.gl.delete_framebuffer(fbo);
        }
        saved.restore(&self.gl);
        logger.check_frame(&self.gl, 0, &format!("{}-capture-png", self.name));

        let row_bytes = w * 4;
        let mut flipped = vec![0u8; rgba.len()];
        for y in 0..h {
            let src_row = &rgba[(h - 1 - y) * row_bytes..(h - y) * row_bytes];
            let dst_row = &mut flipped[y * row_bytes..(y + 1) * row_bytes];
            dst_row.copy_from_slice(src_row);
            for px in dst_row.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }

        let file = File::create(out_path)
            .map_err(|e| format!("Failed to create PNG {}: {e}", out_path.display()))?;
        let mut encoder = png::Encoder::new(BufWriter::new(file), self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("Failed to write PNG header {}: {e}", out_path.display()))?;
        writer
            .write_image_data(&flipped)
            .map_err(|e| format!("Failed to write PNG pixels {}: {e}", out_path.display()))?;

        Ok(())
    }

    /// Cleanly destroys `libprojectM`, blank detector resources, pending fence, and the shared texture.
    pub fn destroy(&mut self) {
        if self.make_current().is_ok() {
            // Drop `Deck` (calls `projectm_destroy`) while this context is current.
            self.deck.take();
            if let Some(fence) = self.pending_sync.take() {
                unsafe {
                    self.gl.delete_sync(fence);
                }
            }
            while let Some(fence) = self.retired_syncs.pop_front() {
                unsafe {
                    self.gl.delete_sync(fence);
                }
            }
            for q in self.gpu_timer_queries {
                unsafe {
                    self.gl.delete_query(q);
                }
            }
            self.blank_detector.destroy(&self.gl);
            unsafe {
                self.gl.delete_texture(self.output_texture);
            }
        }
    }
}

impl Drop for RenderDeck {
    fn drop(&mut self) {
        if self.deck.is_some() {
            self.destroy();
        }
    }
}
