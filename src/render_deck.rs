//! Pattern 1 Multi-Deck Renderer: One Shared OpenGL Context per `libprojectM` Deck.
//!
//! How Pattern 1 works:
//! - Each `RenderDeck` owns its own OpenGL 3.3 Core context that **shares its
//!   texture namespace** with the main window's OpenGL context (`.with_sharing(...)`).
//! - Each `RenderDeck` also owns an off-screen 1920x1080 WGL pbuffer surface and
//!   a 1920x1080 shared `GL_TEXTURE_2D` (`output_texture`).
//! - On every frame (all on ONE thread — see `src/main.rs` for why):
//!   1. Make this deck's context current on its pbuffer.
//!   2. Save OpenGL state (`GlStateSnapshot::save`).
//!   3. Feed PCM audio and call `projectm_opengl_render_frame()`.
//!   4. Reset `READ_FRAMEBUFFER` to FBO 0 (the pbuffer) and copy the 1920x1080 image
//!      GPU-to-GPU into `output_texture` (`glCopyTexSubImage2D`).
//!   5. Restore OpenGL state and check `glGetError()`.
//! - Back on the main window context, the compositor reads both decks' shared
//!   `output_texture` handles and draws them side by side.

use std::ffi::CStr;
use std::fs::{self, File};
use std::io::BufWriter;
use std::num::NonZeroU32;
use std::path::Path;

use glow::HasContext;
use glutin::config::Config;
use glutin::context::{
    ContextApi, ContextAttributesBuilder, NotCurrentContext, PossiblyCurrentContext, Version,
};
use glutin::display::Display;
use glutin::prelude::*;
use glutin::surface::{PbufferSurface, Surface, SurfaceAttributesBuilder};

use crate::deck::Deck;
use crate::gl_log::GlErrorLogger;
use crate::gl_state::{reset_read_to_default_fbo, GlStateSnapshot};

/// One isolated MilkDrop deck owning its own shared OpenGL context, off-screen
/// pbuffer surface, shared output texture, and `libprojectM` instance.
pub struct RenderDeck {
    name: String,
    width: u32,
    height: u32,
    /// Owned `libprojectM` instance (wrapped in `Option` so `destroy()` can drop it
    /// explicitly while this deck's OpenGL context is current).
    deck: Option<Deck>,
    /// Shared `GL_TEXTURE_2D` visible to BOTH this deck's context and the main window context.
    output_texture: glow::NativeTexture,
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

        // 1. Create the off-screen 1920x1080 WGL pbuffer surface.
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

        // 4. Allocate the shared 1920x1080 RGBA8 texture that the main compositor will sample.
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

        // 5. Create this deck's own independent `libprojectM` instance on this context.
        let deck = Deck::new(width as usize, height as usize)?;

        // Check GL errors right after deck initialization.
        logger.check_frame(&gl, 0, &format!("{name}-init"));

        Ok(Self {
            name: name.to_string(),
            width,
            height,
            deck: Some(deck),
            output_texture,
            pbuffer,
            context,
            gl,
        })
    }

    /// Returns the OpenGL texture handle shared with the main window context.
    pub fn output_texture(&self) -> glow::NativeTexture {
        self.output_texture
    }

    /// Makes this deck's OpenGL context current on the calling thread.
    pub fn make_current(&self) -> Result<(), String> {
        self.context
            .make_current(&self.pbuffer)
            .map_err(|e| format!("[{}] Failed to make context current: {e}", self.name))
    }

    /// Loads a `.milk` preset onto this deck (making its context current first).
    pub fn load_preset(&self, path: &Path, smooth_transition: bool) -> Result<(), String> {
        self.make_current()?;
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
    /// and logs any OpenGL errors.
    pub fn render_to_texture(
        &self,
        interleaved_lr_pcm: &[f32],
        frame_index: u64,
        logger: &mut GlErrorLogger,
    ) -> Result<(), String> {
        self.make_current()?;
        let deck = self
            .deck
            .as_ref()
            .ok_or_else(|| format!("[{}] Deck already destroyed", self.name))?;

        // Save GL state before handing control to libprojectM.
        let saved = GlStateSnapshot::save(&self.gl);

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
        }

        // Restore GL state cleanly and flush GPU commands so the shared texture is ready
        // when the main window context samples it.
        saved.restore(&self.gl);
        unsafe {
            self.gl.flush();
        }

        logger.check_frame(&self.gl, frame_index, &self.name);
        Ok(())
    }

    /// Reads this deck's `output_texture` via `glReadPixels` (attached to a temporary FBO)
    /// and saves it as a PNG file (flipped vertically so top-of-screen is top-of-PNG).
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

        // Flip rows vertically (OpenGL origin is bottom-left; PNG origin is top-left)
        // and force alpha=255 so the captured image is fully opaque.
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

    /// Cleanly destroys `libprojectM` and the shared texture while this deck's context is current.
    pub fn destroy(&mut self) {
        if self.make_current().is_ok() {
            // Drop `Deck` (calls `projectm_destroy`) while this context is current.
            self.deck.take();
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
