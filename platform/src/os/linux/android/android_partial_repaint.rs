//! Repaints only the parts of the window that changed, on EGL drivers that report back buffer ages.
//!
//! Android hands us one of a few back buffers each frame, and its age says which frame it showed last.
//! So we repaint what changed since that frame, scissored to those rects, and keep the rest of the buffer.

use std::collections::VecDeque;
use std::ffi::{c_void, CStr};

use super::super::{
    egl_sys::{self, EGLBoolean, EGLDisplay, EGLSurface, EGLint},
    gl_sys,
    opengl::{GlShaderState, SHADER_VARIANT_WINDOW},
};
use super::android::CxAndroidDisplay;
use super::android_shader_worker::ShaderWorkerState;
use crate::{
    cx::Cx,
    draw_damage::{add_damage_rect, DamageRect},
    draw_list::DrawListId,
    draw_pass::{CxDrawPassParent, DrawPassId},
    makepad_math::Vec4f,
};

const EGL_BUFFER_AGE_KHR: EGLint = 0x313D;

/// Some phones hand out back buffers 6 frames old, so we remember as many frames as Android's BufferQueue has slots.
/// Each repaint only reads as far back as its buffer's age.
const DAMAGE_HISTORY_LEN: usize = 64;

type SetDamageRegionFn = unsafe extern "C" fn(EGLDisplay, EGLSurface, *mut EGLint, EGLint) -> EGLBoolean;
type SwapBuffersWithDamageFn = unsafe extern "C" fn(EGLDisplay, EGLSurface, *const EGLint, EGLint) -> EGLBoolean;

#[derive(Clone, Copy)]
struct PartialRepaintExtensions {
    can_repaint_partially: bool,
    set_damage_region: Option<SetDamageRegionFn>,
    swap_buffers_with_damage: Option<SwapBuffersWithDamageFn>,
}

#[derive(Default)]
pub(crate) struct PartialRepaint {
    /// Looked up when the first frame is painted.
    extensions: Option<PartialRepaintExtensions>,
    /// The damage of the last few frames we showed, newest first, where `None` changed the whole window.
    history: VecDeque<Option<Vec<DamageRect>>>,
    /// This frame's window pass and the rects of it to repaint, where `None` repaints all of it.
    plan: Option<(DrawPassId, Option<Vec<DamageRect>>)>,
    /// What changed since the last frame we showed, which we pass on to the compositor.
    frame_damage: Option<Vec<DamageRect>>,
    framebuffer_height: i32,
    /// Popups draw over the window outside of its pass, so the frame after one closes repaints everything.
    had_popups: bool,
    /// Nothing changed, so there's no new frame to show.
    pub(crate) is_frame_unchanged: bool,
}

impl PartialRepaint {
    /// A new surface's buffers hold none of the frames we remember.
    pub(crate) fn clear_frames(&mut self) {
        self.history.clear();
        self.plan = None;
        self.is_frame_unchanged = false;
    }
}

impl Cx {
    /// Works out which rects of the window's back buffer this frame repaints, and tells EGL.
    /// It runs before any pass paints, since EGL wants the damage region before anything is drawn.
    pub(crate) fn plan_window_repaint(&mut self, passes_todo: &[DrawPassId]) {
        let Some((draw_pass_id, window_id)) = passes_todo.iter().find_map(|draw_pass_id| {
            match self.passes[*draw_pass_id].parent {
                CxDrawPassParent::Window(window_id) if !self.windows[window_id].is_popup => {
                    Some((*draw_pass_id, window_id))
                }
                _ => None,
            }
        }) else {
            return;
        };
        let has_popups = self.windows.id_iter().any(|id| {
            let window = &self.windows[id];
            window.is_created && window.is_popup && window.popup_parent == Some(window_id)
        });
        let framebuffer = (self.os.display_size.x as i32, self.os.display_size.y as i32);
        let is_drawable = self.os.has_drawable_surface();
        // Java takes its cover down once we present, and the shader worker starts after our first present.
        let is_present_wanted = self.os.hide_surface_cover_after_first_present
            || self.os.refresh_surface_snapshot_after_first_present
            || matches!(self.os.shader_worker, ShaderWorkerState::BeforeFirstPresent);
        let Some(display) = self.os.display.as_mut().filter(|_| is_drawable) else {
            return;
        };
        let libegl = &display.libegl;
        let extensions = *display.partial_repaint.extensions.get_or_insert_with(|| unsafe {
            let names = libegl
                .eglQueryString
                .map(|query| query(display.egl_display, egl_sys::EGL_EXTENSIONS as EGLint))
                .filter(|names| !names.is_null())
                .map(|names| CStr::from_ptr(names).to_string_lossy().into_owned())
                .unwrap_or_default();
            let has = |name: &str| names.split_ascii_whitespace().any(|extension| extension == name);
            let get_proc = |name: &CStr| {
                libegl
                    .eglGetProcAddress
                    .map(|get_proc_address| get_proc_address(name.as_ptr()))
                    .filter(|proc| !proc.is_null())
            };
            let has_partial_update = has("EGL_KHR_partial_update");
            let set_damage_region = has_partial_update
                .then(|| get_proc(c"eglSetDamageRegionKHR"))
                .flatten()
                .map(|proc| std::mem::transmute::<*mut c_void, SetDamageRegionFn>(proc));
            let swap_buffers_with_damage = if has("EGL_KHR_swap_buffers_with_damage") {
                get_proc(c"eglSwapBuffersWithDamageKHR")
            } else if has("EGL_EXT_swap_buffers_with_damage") {
                get_proc(c"eglSwapBuffersWithDamageEXT")
            } else {
                None
            };
            crate::log!(
                "EGL partial repaint: partial_update={} buffer_age={} set_damage_region={} swap_with_damage={}",
                has_partial_update,
                has("EGL_EXT_buffer_age"),
                set_damage_region.is_some(),
                swap_buffers_with_damage.is_some()
            );
            PartialRepaintExtensions {
                // With partial updates, a buffer only keeps what's outside the damage region we set.
                can_repaint_partially: if has_partial_update {
                    set_damage_region.is_some()
                } else {
                    has("EGL_EXT_buffer_age")
                },
                set_damage_region,
                swap_buffers_with_damage: swap_buffers_with_damage
                    .map(|proc| std::mem::transmute::<*mut c_void, SwapBuffersWithDamageFn>(proc)),
            }
        });
        let state = &mut display.partial_repaint;
        state.plan = None;
        state.frame_damage = None;
        state.is_frame_unchanged = false;
        state.framebuffer_height = framebuffer.1;
        if !extensions.can_repaint_partially || self.draw_damage.is_disabled {
            state.history.clear();
            return;
        }
        // EGL only takes the buffer age and damage region of the current surface.
        if !display.try_make_current() {
            return;
        }
        let had_popups = std::mem::replace(&mut display.partial_repaint.had_popups, has_popups);

        // We walk the pass even when popups force a full repaint, so it remembers what this frame shows.
        let damage = self
            .compute_pass_damage(draw_pass_id, framebuffer, passes_todo, |cx, os_shader_id| {
                match &cx.draw_shaders.os_shaders[os_shader_id].gl_shader[SHADER_VARIANT_WINDOW] {
                    Some(GlShaderState::Ready(_)) => Some(true),
                    Some(GlShaderState::Failed) => Some(false),
                    // It may get built while this frame paints, or stay skipped.
                    _ => None,
                }
            })
            .filter(|_| !has_popups && !had_popups);
        let Some(display) = self.os.display.as_mut() else {
            return;
        };
        // The screen already shows this frame, unless this surface hasn't shown one yet.
        let is_frame_on_screen = !display.partial_repaint.history.is_empty()
            && damage.as_ref().is_some_and(|damage| damage.is_empty());
        if is_frame_on_screen && !is_present_wanted {
            // This frame never reaches a buffer, so we don't remember it.
            display.partial_repaint.is_frame_unchanged = true;
            display.partial_repaint.plan = Some((draw_pass_id, Some(Vec::new())));
            return;
        }
        let mut age: EGLint = 0;
        let has_age = unsafe {
            (display.libegl.eglQuerySurface.unwrap())(display.egl_display, display.surface, EGL_BUFFER_AGE_KHR, &mut age)
        } != 0;
        // A buffer shown `age` frames ago is missing what every frame since changed.
        let older_frames = (has_age && age >= 1)
            .then(|| age as usize - 1)
            .filter(|older_frames| *older_frames <= display.partial_repaint.history.len());
        let mut rects = older_frames.and_then(|older_frames| {
            let mut rects = damage.clone()?;
            for older in display.partial_repaint.history.iter().take(older_frames) {
                for rect in older.as_ref()? {
                    add_damage_rect(&mut rects, *rect);
                }
            }
            // A buffer has to be drawn to be shown again, so an up-to-date one gets repainted in full.
            (!rects.is_empty()).then_some(rects)
        });
        let is_damage_region_set = match (&rects, extensions.set_damage_region) {
            (Some(repaint_rects), Some(set_damage_region)) => {
                let mut egl_rects: Vec<EGLint> =
                    repaint_rects.iter().flat_map(|rect| rect.to_bottom_left(framebuffer.1)).collect();
                unsafe {
                    set_damage_region(
                        display.egl_display,
                        display.surface,
                        egl_rects.as_mut_ptr(),
                        repaint_rects.len() as EGLint,
                    ) != 0
                }
            }
            _ => true,
        };
        if !is_damage_region_set {
            crate::error!(
                "eglSetDamageRegionKHR failed: EGL error {:#x}, so we repaint whole frames from now on",
                display.libegl.last_error()
            );
            if let Some(extensions) = &mut display.partial_repaint.extensions {
                extensions.can_repaint_partially = false;
            }
            rects = None;
        }
        let state = &mut display.partial_repaint;
        // A new surface has no earlier frame on screen for the compositor to patch.
        state.frame_damage = damage.clone().filter(|_| !state.history.is_empty());
        state.history.push_front(damage);
        state.history.truncate(DAMAGE_HISTORY_LEN);
        state.plan = Some((draw_pass_id, rects));
    }

    /// The rects of `draw_pass_id` that `plan_window_repaint` repaints, or `None` for all of it.
    pub(crate) fn take_window_repaint_rects(&mut self, draw_pass_id: DrawPassId) -> Option<Vec<DamageRect>> {
        let (planned_pass_id, rects) = self.os.display.as_mut()?.partial_repaint.plan.take()?;
        rects.filter(|_| planned_pass_id == draw_pass_id)
    }

    /// Clears and paints just `rects` of the window pass, leaving the rest of the back buffer as it was.
    pub(crate) fn repaint_window_rects(
        &mut self,
        draw_pass_id: DrawPassId,
        draw_list_id: DrawListId,
        rects: Vec<DamageRect>,
        clear_color: Vec4f,
        clear_depth: f32,
    ) {
        if rects.is_empty() {
            return;
        }
        let framebuffer_height = self.os.display_size.y as i32;
        let gl = self.os.gl();
        unsafe {
            (gl.glEnable)(gl_sys::SCISSOR_TEST);
            (gl.glClearDepthf)(clear_depth);
            (gl.glClearColor)(clear_color.x, clear_color.y, clear_color.z, clear_color.w);
            for rect in &rects {
                let [x, y, width, height] = rect.to_bottom_left(framebuffer_height);
                (gl.glScissor)(x, y, width, height);
                (gl.glClear)(gl_sys::COLOR_BUFFER_BIT | gl_sys::DEPTH_BUFFER_BIT);
            }
        }
        Self::set_default_depth_and_blend_mode(gl);
        self.draw_damage.start_repaint(draw_pass_id, framebuffer_height, rects);
        let mut zbias = 0.0;
        let zbias_step = self.passes[draw_pass_id].zbias_step;
        self.render_view(draw_pass_id, draw_list_id, &mut zbias, zbias_step);
        self.draw_damage.end_repaint();
        unsafe {
            (self.os.gl().glDisable)(gl_sys::SCISSOR_TEST);
        }
    }
}

impl CxAndroidDisplay {
    /// Shows the frame we drew and tells the compositor which parts of it changed, when we know.
    /// Returns false when there's no new frame to show, or the swap failed.
    pub(crate) unsafe fn swap_buffers(&mut self) -> bool {
        let state = &mut self.partial_repaint;
        if std::mem::take(&mut state.is_frame_unchanged) {
            return false;
        }
        let swap_buffers_with_damage = state.extensions.and_then(|extensions| extensions.swap_buffers_with_damage);
        match (state.frame_damage.take(), swap_buffers_with_damage) {
            (Some(rects), Some(swap_buffers_with_damage)) if !rects.is_empty() => {
                let egl_rects: Vec<EGLint> =
                    rects.iter().flat_map(|rect| rect.to_bottom_left(state.framebuffer_height)).collect();
                swap_buffers_with_damage(self.egl_display, self.surface, egl_rects.as_ptr(), rects.len() as EGLint) != 0
            }
            _ => (self.libegl.eglSwapBuffers.unwrap())(self.egl_display, self.surface) != 0,
        }
    }
}
