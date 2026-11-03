//! `FrameTiming`, which times each frame for the perf monitor while it's enabled: the render
//! thread's CPU time, GL submit and swap time, GPU time, and when frames reach the display.

use std::collections::VecDeque;
use std::ffi::{c_void, CStr};
use std::time::{Duration, Instant};

use super::super::{
    egl_sys::{self, EGLBoolean, EGLDisplay, EGLSurface, EGLint},
    gl_sys::{self, GLenum, GLint, GLsizei, GLuint},
    opengl::get_gl_string,
};
use super::android::CxAndroidDisplay;
use super::android_performance_hint::thread_cpu_time;
use crate::{
    cx::Cx,
    perf_monitor::{perf_gpu_frame_completed, PerfMonitor, PERF_CHANNEL_DRAW, PERF_CHANNEL_DRAWABLE_WAIT},
};

const TIME_ELAPSED_EXT: GLenum = 0x88BF;
const QUERY_COUNTER_BITS_EXT: GLenum = 0x8864;
const QUERY_RESULT_EXT: GLenum = 0x8866;
const QUERY_RESULT_AVAILABLE_EXT: GLenum = 0x8867;
const GPU_DISJOINT_EXT: GLenum = 0x8FBB;

const EGL_DRAW: EGLint = 0x3059;
const EGL_TIMESTAMPS_ANDROID: EGLint = 0x3430;
const EGL_COMPOSITE_INTERVAL_ANDROID: EGLint = 0x3432;
const EGL_DISPLAY_PRESENT_TIME_ANDROID: EGLint = 0x343A;
const EGL_TIMESTAMP_PENDING_ANDROID: i64 = -2;

/// Enough queries for the few frames a tiler's GPU runs behind the CPU.
const GPU_QUERY_COUNT: usize = 6;
/// Android only keeps timestamps for a handful of recent frames.
const PRESENT_HISTORY: usize = 8;

type GenQueries = unsafe extern "C" fn(GLsizei, *mut GLuint);
type DeleteQueries = unsafe extern "C" fn(GLsizei, *const GLuint);
type BeginQuery = unsafe extern "C" fn(GLenum, GLuint);
type EndQuery = unsafe extern "C" fn(GLenum);
type GetQueryiv = unsafe extern "C" fn(GLenum, GLenum, *mut GLint);
type GetQueryObjectuiv = unsafe extern "C" fn(GLuint, GLenum, *mut GLuint);
type GetQueryObjectui64v = unsafe extern "C" fn(GLuint, GLenum, *mut u64);
type GetIntegerv = unsafe extern "C" fn(GLenum, *mut GLint);
type GetNextFrameId = unsafe extern "C" fn(EGLDisplay, EGLSurface, *mut u64) -> EGLBoolean;
type GetFrameTimestamps =
    unsafe extern "C" fn(EGLDisplay, EGLSurface, u64, EGLint, *const EGLint, *mut i64) -> EGLBoolean;
type GetCompositorTiming =
    unsafe extern "C" fn(EGLDisplay, EGLSurface, EGLint, *const EGLint, *mut i64) -> EGLBoolean;
type GetFrameTimestampSupported = unsafe extern "C" fn(EGLDisplay, EGLSurface, EGLint) -> EGLBoolean;
type GetCurrentSurface = unsafe extern "C" fn(EGLint) -> EGLSurface;
type SurfaceAttrib = unsafe extern "C" fn(EGLDisplay, EGLSurface, EGLint, EGLint) -> EGLBoolean;

pub(crate) struct FrameTiming {
    gpu_timer: Option<GpuTimer>,
    present_timer: Option<PresentTimer>,
    /// The render loop ticks since our last swap, counting the current one.
    /// Ticks stopping and starting again counts as one more, since we sat idle.
    ticks_since_swap: u32,
    /// The start of the GL submit time we haven't added to the draw channel yet.
    draw_start: Instant,
    /// When this frame's swap started, if it swaps.
    swap_start: Option<Instant>,
    last_swap_cpu_time: Duration,
}

impl FrameTiming {
    fn new(display: &CxAndroidDisplay) -> Self {
        // Vulkan draws and presents the frame, so there's nothing for GL and EGL to time.
        let is_gl_rendering = cfg!(not(use_vulkan));
        Self {
            gpu_timer: is_gl_rendering.then(|| GpuTimer::new(display)).flatten(),
            present_timer: is_gl_rendering.then(|| PresentTimer::new(display)).flatten(),
            ticks_since_swap: 0,
            draw_start: Instant::now(),
            swap_start: None,
            last_swap_cpu_time: thread_cpu_time(),
        }
    }

    /// Called when vsync ticks start again after they stopped.
    pub(crate) fn handle_vsync_restart(&mut self) {
        self.ticks_since_swap += 1;
    }

    fn stop(self, display: &CxAndroidDisplay) {
        if let Some(gpu_timer) = self.gpu_timer {
            unsafe { (gpu_timer.delete_queries)(GPU_QUERY_COUNT as GLsizei, gpu_timer.queries.as_ptr()) };
        }
        if let Some(present_timer) = self.present_timer.filter(|timer| !timer.surface.is_null()) {
            unsafe {
                (present_timer.surface_attrib)(display.egl_display, present_timer.surface, EGL_TIMESTAMPS_ANDROID, 0)
            };
        }
    }
}

/// Times whole frames on the GPU with `GL_EXT_disjoint_timer_query`.
struct GpuTimer {
    delete_queries: DeleteQueries,
    begin_query: BeginQuery,
    end_query: EndQuery,
    get_query_object: GetQueryObjectuiv,
    get_query_object_u64: GetQueryObjectui64v,
    get_integer: GetIntegerv,
    queries: [GLuint; GPU_QUERY_COUNT],
    /// The queries still waiting on their result, oldest first, each with whether we'll skip it,
    /// since a disjoint event spoiled it or its repaint didn't present anything.
    pending: VecDeque<(GLuint, bool)>,
    next_query: usize,
    is_open: bool,
}

impl GpuTimer {
    fn new(display: &CxAndroidDisplay) -> Option<Self> {
        let has_extension = get_gl_string(&display.libgl, gl_sys::EXTENSIONS)
            .split_whitespace()
            .any(|extension| extension == "GL_EXT_disjoint_timer_query");
        if !has_extension {
            return None;
        }
        unsafe {
            let gen_queries: GenQueries = load_proc(display, &[c"glGenQueriesEXT", c"glGenQueries"])?;
            let get_query: GetQueryiv = load_proc(display, &[c"glGetQueryivEXT", c"glGetQueryiv"])?;
            let mut timer = Self {
                delete_queries: load_proc(display, &[c"glDeleteQueriesEXT", c"glDeleteQueries"])?,
                begin_query: load_proc(display, &[c"glBeginQueryEXT", c"glBeginQuery"])?,
                end_query: load_proc(display, &[c"glEndQueryEXT", c"glEndQuery"])?,
                get_query_object: load_proc(display, &[c"glGetQueryObjectuivEXT", c"glGetQueryObjectuiv"])?,
                get_query_object_u64: load_proc(display, &[c"glGetQueryObjectui64vEXT"])?,
                get_integer: load_proc(display, &[c"glGetIntegerv"])?,
                queries: [0; GPU_QUERY_COUNT],
                pending: VecDeque::new(),
                next_query: 0,
                is_open: false,
            };
            // Some drivers list the extension without a timer behind it.
            let mut counter_bits = 0;
            get_query(TIME_ELAPSED_EXT, QUERY_COUNTER_BITS_EXT, &mut counter_bits);
            if counter_bits == 0 {
                return None;
            }
            gen_queries(GPU_QUERY_COUNT as GLsizei, timer.queries.as_mut_ptr());
            // Reading the disjoint flag clears it, so an old event can't spoil our first queries.
            let mut is_disjoint = 0;
            (timer.get_integer)(GPU_DISJOINT_EXT, &mut is_disjoint);
            Some(timer)
        }
    }

    fn begin(&mut self) {
        if self.is_open || self.pending.len() == GPU_QUERY_COUNT {
            return;
        }
        unsafe { (self.begin_query)(TIME_ELAPSED_EXT, self.queries[self.next_query]) };
        self.is_open = true;
    }

    fn end(&mut self, is_presented: bool) {
        if !self.is_open {
            return;
        }
        unsafe { (self.end_query)(TIME_ELAPSED_EXT) };
        self.pending.push_back((self.queries[self.next_query], !is_presented));
        self.next_query = (self.next_query + 1) % GPU_QUERY_COUNT;
        self.is_open = false;
    }

    /// Reports the GPU time of the oldest frame that's done, so each perf monitor frame gets one sample.
    /// Without `is_reporting`, it only frees the queries of finished frames that never presented.
    fn collect(&mut self, is_reporting: bool) {
        while let Some(&(query, is_skipped)) = self.pending.front() {
            if !is_skipped && !is_reporting {
                return;
            }
            let mut is_available = 0;
            unsafe { (self.get_query_object)(query, QUERY_RESULT_AVAILABLE_EXT, &mut is_available) };
            if is_available == 0 {
                return;
            }
            // The query is done, so a disjoint event during it has been flagged by now.
            let mut is_disjoint = 0;
            unsafe { (self.get_integer)(GPU_DISJOINT_EXT, &mut is_disjoint) };
            if is_disjoint != 0 {
                self.pending.iter_mut().for_each(|(_, is_skipped)| *is_skipped = true);
            }
            let Some((_, is_skipped)) = self.pending.pop_front() else { return };
            if !is_skipped {
                let mut nanos = 0;
                unsafe { (self.get_query_object_u64)(query, QUERY_RESULT_EXT, &mut nanos) };
                perf_gpu_frame_completed(nanos as f64 / 1e9);
                return;
            }
        }
    }
}

/// Reads when frames reach the display with `EGL_ANDROID_get_frame_timestamps`.
struct PresentTimer {
    get_current_surface: GetCurrentSurface,
    surface_attrib: SurfaceAttrib,
    get_next_frame_id: GetNextFrameId,
    get_frame_timestamps: GetFrameTimestamps,
    get_compositor_timing: GetCompositorTiming,
    /// The surface we turned timestamps on for, since a new one starts with them off.
    surface: EGLSurface,
    vsync_interval_ns: i64,
    /// The frames we swapped but haven't read the present time of yet, oldest first,
    /// each with whether we drew it right after the frame before it.
    pending: VecDeque<(u64, bool)>,
    last_present_ns: Option<i64>,
}

impl PresentTimer {
    fn new(display: &CxAndroidDisplay) -> Option<Self> {
        let query_string = display.libegl.eglQueryString?;
        let extensions = unsafe { query_string(display.egl_display, egl_sys::EGL_EXTENSIONS as _) };
        let has_extension = !extensions.is_null()
            && unsafe { CStr::from_ptr(extensions) }
                .to_string_lossy()
                .split_whitespace()
                .any(|extension| extension == "EGL_ANDROID_get_frame_timestamps");
        if !has_extension {
            return None;
        }
        unsafe {
            let timer = Self {
                get_current_surface: display.libegl.eglGetCurrentSurface?,
                surface_attrib: display.libegl.eglSurfaceAttrib?,
                get_next_frame_id: load_proc(display, &[c"eglGetNextFrameIdANDROID"])?,
                get_frame_timestamps: load_proc(display, &[c"eglGetFrameTimestampsANDROID"])?,
                get_compositor_timing: load_proc(display, &[c"eglGetCompositorTimingANDROID"])?,
                surface: egl_sys::EGL_NO_SURFACE,
                vsync_interval_ns: 0,
                pending: VecDeque::new(),
                last_present_ns: None,
            };
            // Without present fences the display never reports when a frame showed up.
            let is_supported: GetFrameTimestampSupported =
                load_proc(display, &[c"eglGetFrameTimestampSupportedANDROID"])?;
            let surface = (timer.get_current_surface)(EGL_DRAW);
            if surface.is_null() || is_supported(display.egl_display, surface, EGL_DISPLAY_PRESENT_TIME_ANDROID) == 0 {
                return None;
            }
            Some(timer)
        }
    }

    /// Remembers the frame we're about to swap, so a later frame can read when it reached the display.
    fn mark_frame(&mut self, display: &CxAndroidDisplay, is_back_to_back: bool) {
        let surface = unsafe { (self.get_current_surface)(EGL_DRAW) };
        if surface.is_null() {
            return;
        }
        if surface != self.surface {
            // A new surface's frame ids start over, so the ones we kept are meaningless.
            unsafe { (self.surface_attrib)(display.egl_display, surface, EGL_TIMESTAMPS_ANDROID, 1) };
            self.surface = surface;
            self.pending.clear();
            self.last_present_ns = None;
        }
        let mut vsync_interval_ns = 0;
        let is_timed = unsafe {
            (self.get_compositor_timing)(
                display.egl_display,
                surface,
                1,
                &EGL_COMPOSITE_INTERVAL_ANDROID,
                &mut vsync_interval_ns,
            )
        };
        if is_timed != 0 && vsync_interval_ns > 0 {
            self.vsync_interval_ns = vsync_interval_ns;
        }
        let is_back_to_back = is_back_to_back && self.vsync_interval_ns > 0;
        let mut frame_id = 0;
        if unsafe { (self.get_next_frame_id)(display.egl_display, surface, &mut frame_id) } != 0 {
            self.pending.push_back((frame_id, is_back_to_back));
        }
        if self.pending.len() > PRESENT_HISTORY {
            self.pending.pop_front();
            self.last_present_ns = None;
        }
    }

    fn collect(&mut self, display: &CxAndroidDisplay, perf_monitor: &mut PerfMonitor) {
        while let Some(&(frame_id, is_back_to_back)) = self.pending.front() {
            let mut present_ns = 0;
            let is_read = unsafe {
                (self.get_frame_timestamps)(
                    display.egl_display,
                    self.surface,
                    frame_id,
                    1,
                    &EGL_DISPLAY_PRESENT_TIME_ANDROID,
                    &mut present_ns,
                )
            } != 0;
            if is_read && present_ns == EGL_TIMESTAMP_PENDING_ANDROID {
                return;
            }
            self.pending.pop_front();
            if !is_read || present_ns < 0 {
                // We can't time the gap after a frame that has no present time either.
                self.last_present_ns = None;
                // A failed read can mean a new surface took the old one's address, with timestamps off.
                if !is_read {
                    unsafe { (self.surface_attrib)(display.egl_display, self.surface, EGL_TIMESTAMPS_ANDROID, 1) };
                }
                continue;
            }
            if let Some(last_present_ns) = self.last_present_ns.filter(|_| is_back_to_back) {
                let interval_ns = present_ns - last_present_ns;
                let vsyncs = (interval_ns as f64 / self.vsync_interval_ns as f64).round() as u32;
                perf_monitor.add_present(interval_ns as f32 / 1e6, vsyncs.saturating_sub(1));
            }
            self.last_present_ns = Some(present_ns);
        }
    }
}

/// Looks up a GL or EGL extension function, trying each name in turn.
///
/// `T` has to be the function's real type.
unsafe fn load_proc<T>(display: &CxAndroidDisplay, names: &[&CStr]) -> Option<T> {
    let get_proc_address = display.libegl.eglGetProcAddress?;
    let proc = names
        .iter()
        .map(|name| get_proc_address(name.as_ptr()))
        .find(|proc| !proc.is_null())?;
    Some(std::mem::transmute_copy::<*mut c_void, T>(&proc))
}

impl Cx {
    /// Starts or stops timing frames as the perf monitor turns on or off, at the start of each tick.
    pub(crate) fn start_perf_frame(&mut self) {
        if !self.perf_monitor.enabled() {
            if let (Some(frame_timing), Some(display)) = (self.os.frame_timing.take(), &self.os.display) {
                frame_timing.stop(display);
            }
            return;
        }
        let Some(display) = &self.os.display else { return };
        self.os.frame_timing.get_or_insert_with(|| FrameTiming::new(display)).ticks_since_swap += 1;
    }

    pub(crate) fn start_perf_repaint(&mut self) {
        let Some(frame_timing) = &mut self.os.frame_timing else { return };
        frame_timing.draw_start = Instant::now();
        if let Some(gpu_timer) = &mut frame_timing.gpu_timer {
            gpu_timer.begin();
        }
    }

    pub(crate) fn finish_perf_repaint(&mut self) {
        let Some(frame_timing) = &mut self.os.frame_timing else { return };
        if let Some(gpu_timer) = &mut frame_timing.gpu_timer {
            gpu_timer.end(false);
            gpu_timer.collect(false);
        }
        self.perf_monitor.add(PERF_CHANNEL_DRAW, frame_timing.draw_start.elapsed().as_micros() as u64);
    }

    pub(crate) fn start_perf_swap(&mut self) {
        let (Some(frame_timing), Some(display)) = (&mut self.os.frame_timing, &self.os.display) else { return };
        // A frame we don't swap never reaches the display, so its time goes into the next one that does.
        #[cfg(not(use_vulkan))]
        if !display.is_surface_alive() || display.partial_repaint.is_frame_unchanged {
            return;
        }
        if let Some(gpu_timer) = &mut frame_timing.gpu_timer {
            gpu_timer.end(true);
        }
        if let Some(present_timer) = &mut frame_timing.present_timer {
            // A tick that didn't swap means we sat idle, so this frame's wait isn't one we missed.
            present_timer.mark_frame(display, frame_timing.ticks_since_swap == 1);
        }
        let swap_start = Instant::now();
        frame_timing.swap_start = Some(swap_start);
        let draw_time = swap_start.duration_since(frame_timing.draw_start);
        self.perf_monitor.add(PERF_CHANNEL_DRAW, draw_time.as_micros() as u64);
    }

    /// Closes the perf monitor's frame, since a frame ends once it's swapped.
    pub(crate) fn finish_perf_swap(&mut self) {
        let (Some(frame_timing), Some(display)) = (&mut self.os.frame_timing, &self.os.display) else { return };
        let Some(swap_start) = frame_timing.swap_start.take() else { return };
        let swap_end = Instant::now();
        let swap_time = swap_end.duration_since(swap_start);
        self.perf_monitor.add(PERF_CHANNEL_DRAWABLE_WAIT, swap_time.as_micros() as u64);
        frame_timing.draw_start = swap_end;
        frame_timing.ticks_since_swap = 0;
        if let Some(gpu_timer) = &mut frame_timing.gpu_timer {
            gpu_timer.collect(true);
        }
        if let Some(present_timer) = &mut frame_timing.present_timer {
            present_timer.collect(display, &mut self.perf_monitor);
        }
        let cpu_time = thread_cpu_time();
        let frame_cpu_time = cpu_time.saturating_sub(frame_timing.last_swap_cpu_time);
        self.perf_monitor.add_thread_cpu_time(frame_cpu_time.as_micros() as u64);
        frame_timing.last_swap_cpu_time = cpu_time;
        self.perf_monitor.frame_boundary(self.os.timers.time_now());
    }
}
