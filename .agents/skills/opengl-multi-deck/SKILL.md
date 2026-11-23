---
name: opengl-multi-deck
description: OpenGL 3.3 Core multi-context rules for Deck A/B, GpuMixer, EguiGlRenderer, GPU timers, async blank detection, and Stage 2B Projector Output. Use before editing render_deck.rs, mixer.rs, egui_renderer.rs, gl_state.rs, or window/surface creation in main.rs.
---

# OpenGL Multi-Deck & Zero-Leak Rendering Playbook

Read this carefully before modifying any OpenGL, WGL, `glutin`, `glow`, or `libprojectM` rendering code.

## 1. Shared Context Architecture (Pattern 1)

- **Main window context:** OpenGL 3.3 Core on the primary Control Console window. Owns `GpuMixer` FBO, `EguiGlRenderer`, and shared texture sampling.
- **Deck A & Deck B contexts (`src/render_deck.rs`):** Each deck has its own OpenGL 3.3 Core context created with `with_sharing(&main_context)` and bound to its own WGL pbuffer surface.
  - **Windows WGL rule:** The pbuffer pixel-format template MUST include `.compatible_with_native_window(raw_window_handle)` so WGL share lists succeed on NVIDIA and Intel drivers.
  - **GLEW rule:** Call `init_gl_loader_for_projectm()` (`glewInit()`) once after the main context is current and before any `Deck::new()`.
  - **State isolation (`src/gl_state.rs`):** Always wrap `projectm_opengl_render_frame()` in `GlStateGuard` save/restore because `libprojectM` mutates `READ_FRAMEBUFFER_BINDING`, viewport, scissor, blend, and active texture units.

## 2. Cross-Context Synchronization (OpenGL 3.3 Core Spec, Section 5.3)

Shared textures are visible across contexts in the same share group, **but command streams are independent**. `glFlush()` alone is NOT sufficient.

Required sequence every frame:
1. On Deck context: render `libprojectM` -> copy pbuffer to shared `GL_TEXTURE_2D` -> call `glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0)` -> call `glFlush()`.
2. On Deck context (next frame / shutdown): retire/delete signaled fences on the deck context that created them.
3. On Main (and future Projector) context: BEFORE sampling `deck_a.output_texture()` or `deck_b.output_texture()`, call GPU-only `glWaitSync(fence, 0, GL_TIMEOUT_IGNORED)`. Never call CPU-blocking `glClientWaitSync` or `glFinish()` in the live loop.

## 3. Why `src/egui_renderer.rs` Replaces `egui_glow` (CRITICAL)

- On Windows NVIDIA driver `596.58` with WGL shared contexts, `egui_glow::Painter::paint_mesh()` calling `glBufferData(..., GL_STREAM_DRAW)` ~25×/frame leaks `+3.2 MiB` every 5 seconds (`+2.3 GiB/hr`).
- `src/egui_renderer.rs` solves this by keeping persistent vertex/index VBOs that only grow (never reallocate every frame) and updating them via `glBufferSubData`.
- **Rule:** Never replace `src/egui_renderer.rs` with `egui_glow`, and never introduce per-frame `glBufferData` allocations in any renderer or mixer pass.

## 4. Non-Blocking GPU Timing & Blank-Deck Detection

- **VSync timing trap:** With VSync enabled, NVIDIA blocks for the previous frame's VSync inside Deck A's first OpenGL call, inflating CPU wall time by ~10 ms.
- **Deck GPU timer (`src/render_deck.rs`):** Use the 3-slot `GL_TIME_ELAPSED` query ring, reading results from 2 frames earlier with zero CPU stall.
- **Blank-deck warning (`src/render_deck.rs`):** Disable `GL_SCISSOR_TEST` before blitting to the `32x18` FBO, then read back asynchronously via a 2-slot PBO ring (`~2x/sec`, ~200 us CPU cost). Warn only when average Rec. 709 luma stays `< 3.0 / 255.0` continuously for `>= 2.0 s`.

## 5. Runtime Resize (`2A-5`) & Dual-Window Projector (`2B`) Rules

- **Resolution toggle (`2A-5`):** Switching `1280x720` <-> `1920x1080` must recreate each deck's WGL pbuffer surface + shared output texture on that deck's context, call `projectm_set_window_size()`, recreate `GpuMixer`'s Master FBO/texture on the main context, and update the registered `egui` texture IDs cleanly (destroying old GL handles so memory stays flat).
- **Single-VSync rule (`2B`):** When the borderless fullscreen Projector window is open, set `SwapInterval::Wait(1)` on the **Projector** surface and `SwapInterval::DontWait` on the **Control Console** surface. Restore `Wait(1)` on the Control Console when the Projector closes.
- **Clean teardown:** Always make the owning context current before deleting GL programs, FBOs, textures, VAOs, VBOs, PBOs, queries, or fences.
