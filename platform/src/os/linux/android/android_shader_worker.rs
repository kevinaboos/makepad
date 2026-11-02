//! Builds GL programs on a background thread for drivers that compile one program at a time, so
//! after the first frame the render thread only loads finished binaries instead of compiling.

use std::collections::{HashSet, VecDeque};
use std::ffi::CStr;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use super::super::{
    egl_sys::{self, EGLContext, EGLDisplay, EGLSurface, LibEgl},
    gl_sys::{self, LibGl},
    libc_sys,
    opengl::{get_gl_string, CxOsDrawShader, GlShader, GlShaderState, SHADER_VARIANT_WINDOW},
};
use super::android::{load_libgl, set_current_thread_priority, set_thread_priority, CxOs};
use crate::{
    cx::{Cx, OsType},
    draw_pass::DrawPassId,
    draw_shader::CxDrawShaderMapping,
    thread::ThreadOptions,
    CxThreadPriority,
};

pub(crate) enum ShaderWorkerState {
    /// The first frame's programs still compile on the render thread, behind the compile screen.
    BeforeFirstPresent,
    Running(ShaderWorker),
    /// No worker, since the driver compiles in parallel or ours couldn't start, so the render
    /// thread builds each program itself.
    Off,
}

struct Job {
    os_shader_id: usize,
    vertex: String,
    pixel: String,
    cache_path: String,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    /// Programs a draw is waiting on, which go ahead of everything else.
    wanted: HashSet<usize>,
    in_flight: Option<usize>,
    /// Some drivers (Adreno) make glProgramBinary wait for any compile, so the render thread never
    /// loads a binary during one, and the worker doesn't start one during a load or a handover.
    compiling: bool,
    ui_loading: bool,
    /// The render thread hasn't taken the program we handed over yet.
    handing_over: bool,
    /// Programs whose cache file didn't load, so the worker compiles them even though it's there.
    skip_cache: HashSet<usize>,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    changed: Condvar,
    /// The worker's thread id, so a draw that's waiting can raise its priority.
    tid: AtomicI32,
    /// The cache was missing a program this session.
    found_uncached: AtomicBool,
}

fn lock(queue: &Mutex<Queue>) -> MutexGuard<'_, Queue> {
    queue.lock().unwrap_or_else(PoisonError::into_inner)
}

enum Reply {
    /// A program the worker compiled, which it also wrote to the cache.
    Built { os_shader_id: usize, binary_format: u32, binary: Vec<u8> },
    /// A program a draw asked for that's in the cache after all, so the render thread loads it itself.
    Cached { os_shader_id: usize },
    Rejected { os_shader_id: usize },
    Unavailable(String),
}

pub(crate) struct ShaderWorker {
    shared: Arc<Shared>,
    replies: Receiver<Reply>,
    cache_dir: String,
    /// Passes that skipped a draw for a program the worker hasn't delivered yet.
    waiting_passes: Vec<DrawPassId>,
    /// A reply we took but couldn't load yet, since our context wasn't current.
    unloaded: Option<Reply>,
}

impl ShaderWorker {
    fn new_job(&self, gl: &LibGl, os_type: &OsType, os_shader_id: usize, shader: &CxOsDrawShader) -> Job {
        let vertex = shader.vertex[SHADER_VARIANT_WINDOW].clone();
        let pixel = shader.pixel[SHADER_VARIANT_WINDOW].clone();
        let cache_path = GlShader::program_cache_path(gl, &self.cache_dir, &vertex, &pixel, os_type);
        Job { os_shader_id, vertex, pixel, cache_path }
    }

    fn queue_build(&self, gl: &LibGl, os_type: &OsType, os_shader_id: usize, shader: &CxOsDrawShader) {
        let job = self.new_job(gl, os_type, os_shader_id, shader);
        if !Path::new(&job.cache_path).exists() {
            self.shared.found_uncached.store(true, Ordering::Relaxed);
        }
        lock(&self.shared.queue).jobs.push_back(job);
        self.shared.changed.notify_all();
    }

    /// Puts a program a draw is waiting on ahead of everything else, and raises the worker,
    /// since whatever it's compiling now holds up that draw.
    fn want(&self, gl: &LibGl, os_type: &OsType, os_shader_id: usize, shader: &CxOsDrawShader) {
        let mut queue = lock(&self.shared.queue);
        if !queue.wanted.insert(os_shader_id) {
            return;
        }
        if let Some(index) = queue.jobs.iter().position(|job| job.os_shader_id == os_shader_id) {
            let job = queue.jobs.remove(index).unwrap();
            queue.jobs.push_front(job);
        } else if queue.in_flight != Some(os_shader_id) {
            queue.jobs.push_front(self.new_job(gl, os_type, os_shader_id, shader));
        }
        drop(queue);
        self.shared.changed.notify_all();
        set_thread_priority(self.shared.tid.load(Ordering::Relaxed), CxThreadPriority::Normal);
    }

    /// Drops whatever the worker still has queued for a program we just loaded.
    fn forget(&self, os_shader_id: usize) {
        let mut queue = lock(&self.shared.queue);
        queue.wanted.remove(&os_shader_id);
        queue.skip_cache.remove(&os_shader_id);
        queue.jobs.retain(|job| job.os_shader_id != os_shader_id);
    }

    /// Lets the worker go on, or tells it to stop, once we've taken what it handed over.
    fn end_handover(&self, stop: bool) {
        let mut queue = lock(&self.shared.queue);
        queue.handing_over = false;
        queue.stop |= stop;
        drop(queue);
        self.shared.changed.notify_all();
    }

    pub(crate) fn stop(&self) {
        lock(&self.shared.queue).stop = true;
        self.shared.changed.notify_all();
    }
}

impl CxOs {
    pub(crate) fn queue_shader_worker_build(
        &self,
        os_shader_id: usize,
        shader: &CxOsDrawShader,
        os_type: &OsType,
    ) {
        if let ShaderWorkerState::Running(worker) = &self.shader_worker {
            worker.queue_build(&self.display.as_ref().unwrap().libgl, os_type, os_shader_id, shader);
        }
    }

    /// Loads a program from the cache while the worker isn't compiling, or else asks the worker
    /// for it. False means we skip this draw until the worker delivers the program.
    pub(crate) fn load_shader_worker_program(
        &mut self,
        shader: &mut CxOsDrawShader,
        os_shader_id: usize,
        mapping: &CxDrawShaderMapping,
        os_type: &OsType,
        draw_pass_id: DrawPassId,
    ) -> bool {
        let ShaderWorkerState::Running(worker) = &mut self.shader_worker else {
            return true;
        };
        let gl = &self.display.as_ref().unwrap().libgl;
        let mut queue = lock(&worker.shared.queue);
        let can_load = !queue.compiling;
        queue.ui_loading = can_load;
        drop(queue);
        if can_load {
            let vertex = &shader.vertex[SHADER_VARIANT_WINDOW];
            let pixel = &shader.pixel[SHADER_VARIANT_WINDOW];
            let program = GlShader::read_program_cache(gl, vertex, pixel, os_type)
                .map(|program| GlShader::build_from_program(gl, program, mapping));
            lock(&worker.shared.queue).ui_loading = false;
            worker.shared.changed.notify_all();
            if let Some(program) = program {
                shader.gl_shader[SHADER_VARIANT_WINDOW] = Some(GlShaderState::Ready(program));
                worker.forget(os_shader_id);
                return true;
            }
        }
        shader.gl_shader[SHADER_VARIANT_WINDOW] = Some(GlShaderState::Queued);
        worker.want(gl, os_type, os_shader_id, shader);
        if !worker.waiting_passes.contains(&draw_pass_id) {
            worker.waiting_passes.push(draw_pass_id);
        }
        false
    }

    pub(crate) fn is_building_shaders_in_background(&self) -> bool {
        matches!(
            &self.shader_worker,
            ShaderWorkerState::Running(worker) if worker.shared.found_uncached.load(Ordering::Relaxed)
        )
    }
}

impl Cx {
    /// Hands every program the first frame didn't draw to a worker, on drivers that compile one at a time.
    pub(crate) fn start_shader_worker_after_first_present(&mut self) {
        if !matches!(self.os.shader_worker, ShaderWorkerState::BeforeFirstPresent) {
            return;
        }
        self.os.shader_worker = ShaderWorkerState::Off;
        let Some(display) = self.os.display.as_ref() else {
            return;
        };
        let Some(cache_dir) = self.os_type.get_cache_dir() else {
            return;
        };
        if self.os.in_xr_mode || GlShader::supports_parallel_compile(&display.libgl) {
            return;
        }
        let init = WorkerInit {
            egl_config: display.egl_config as usize,
            driver: get_driver_strings(&display.libgl),
            cache_dir: cache_dir.clone(),
            android_suffix: display.libgl.android_cache_suffix.get().cloned(),
            os_type: self.os_type.clone(),
        };
        let shared = Arc::new(Shared::default());
        let (reply_tx, replies) = mpsc::channel();
        let worker_shared = shared.clone();
        let options = ThreadOptions {
            name: Some("makepad-shaders".into()),
            priority: CxThreadPriority::Background,
            ..Default::default()
        };
        let Ok(task) = self.spawn_worker_with(options, move || run_worker(init, worker_shared, reply_tx))
        else {
            return;
        };
        task.detach();
        let worker = ShaderWorker { shared, replies, cache_dir, waiting_passes: Vec::new(), unloaded: None };
        for (os_shader_id, shader) in self.draw_shaders.os_shaders.iter().enumerate() {
            if shader.gl_shader[SHADER_VARIANT_WINDOW].is_none() {
                worker.queue_build(&display.libgl, &self.os_type, os_shader_id, shader);
            }
        }
        self.os.shader_worker = ShaderWorkerState::Running(worker);
    }

    /// Loads what the worker handed over, and repaints the draws that were waiting on it.
    pub(crate) fn handle_shader_worker_replies(&mut self) {
        if !self.os.has_drawable_surface() {
            return;
        }
        let ShaderWorkerState::Running(worker) = &mut self.os.shader_worker else {
            return;
        };
        let display = self.os.display.as_ref().unwrap();
        let gl = &display.libgl;
        let mut context_is_current = false;
        let mut got_reply = false;
        let mut off_reason = None;
        while off_reason.is_none() {
            let reply = match worker.unloaded.take() {
                Some(reply) => reply,
                None => match worker.replies.try_recv() {
                    Ok(reply) => reply,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        off_reason = Some("its thread ended".to_string());
                        break;
                    }
                },
            };
            // Loads need our context, so a reply waits for it, and the worker waits on its handover.
            if matches!(reply, Reply::Built { .. } | Reply::Cached { .. }) && !context_is_current {
                if !display.try_make_current() {
                    worker.unloaded = Some(reply);
                    break;
                }
                context_is_current = true;
            }
            let (os_shader_id, built) = match reply {
                Reply::Built { os_shader_id, binary_format, binary } => {
                    (os_shader_id, Some((binary_format, binary)))
                }
                Reply::Cached { os_shader_id } => (os_shader_id, None),
                Reply::Rejected { os_shader_id } => {
                    got_reply = true;
                    self.draw_shaders.os_shaders[os_shader_id].gl_shader[SHADER_VARIANT_WINDOW] =
                        Some(GlShaderState::Failed);
                    worker.forget(os_shader_id);
                    worker.end_handover(false);
                    continue;
                }
                Reply::Unavailable(reason) => {
                    off_reason = Some(reason);
                    break;
                }
            };
            got_reply = true;
            let os_shader = &mut self.draw_shaders.os_shaders[os_shader_id];
            let mapping = self.draw_shaders.shaders.iter()
                .find(|shader| shader.os_shader_id == Some(os_shader_id))
                .map(|shader| &shader.mapping);
            // A draw may have loaded it from the cache already.
            let unbuilt = matches!(
                os_shader.gl_shader[SHADER_VARIANT_WINDOW],
                None | Some(GlShaderState::Queued)
            );
            if let (true, Some(mapping)) = (unbuilt, mapping) {
                let program = match built {
                    Some((binary_format, binary)) => GlShader::load_program_binary(gl, binary_format, &binary)
                        .map_err(|error| off_reason = Some(format!("a binary it built didn't load:\n{error}")))
                        .ok(),
                    None => GlShader::read_program_cache(
                        gl,
                        &os_shader.vertex[SHADER_VARIANT_WINDOW],
                        &os_shader.pixel[SHADER_VARIANT_WINDOW],
                        &self.os_type,
                    ),
                };
                match program {
                    Some(program) => {
                        os_shader.gl_shader[SHADER_VARIANT_WINDOW] =
                            Some(GlShaderState::Ready(GlShader::build_from_program(gl, program, mapping)));
                        worker.forget(os_shader_id);
                    }
                    // That cache file didn't load, so the worker builds it from scratch.
                    None if off_reason.is_none() => {
                        lock(&worker.shared.queue).skip_cache.insert(os_shader_id);
                        worker.want(gl, &self.os_type, os_shader_id, os_shader);
                    }
                    None => {}
                }
            }
            // A binary the worker built that won't load here means none of its builds will.
            worker.end_handover(off_reason.is_some());
        }
        if got_reply || off_reason.is_some() {
            for draw_pass_id in worker.waiting_passes.drain(..) {
                // Its slot may have been freed since, or reused by a pass that isn't set up yet.
                if !self.passes.0.is_free(draw_pass_id.0)
                    && self.passes[draw_pass_id].main_draw_list_id.is_some()
                {
                    self.passes[draw_pass_id].paint_dirty = true;
                }
            }
        }
        if let Some(reason) = off_reason {
            crate::warning!(
                "Shaders can't be built in the background ({reason}), so they compile on their first draw"
            );
            worker.stop();
            for os_shader in &mut self.draw_shaders.os_shaders {
                if matches!(os_shader.gl_shader[SHADER_VARIANT_WINDOW], Some(GlShaderState::Queued)) {
                    os_shader.gl_shader[SHADER_VARIANT_WINDOW] = None;
                }
            }
            self.os.shader_worker = ShaderWorkerState::Off;
        }
    }
}

/// GL_VENDOR, GL_RENDERER and GL_VERSION, which have to match between the two contexts so their binaries do.
fn get_driver_strings(gl: &LibGl) -> [String; 3] {
    [gl_sys::VENDOR, gl_sys::RENDERER, gl_sys::VERSION].map(|key| get_gl_string(gl, key))
}

struct WorkerInit {
    /// The render thread's EGL config, as an address since EGL handles aren't Send.
    egl_config: usize,
    driver: [String; 3],
    cache_dir: String,
    android_suffix: Option<String>,
    os_type: OsType,
}

/// The worker's own context. It shares nothing with the render thread's, so drivers can't lock the two together.
struct WorkerEgl {
    libegl: LibEgl,
    display: EGLDisplay,
    context: EGLContext,
    surface: EGLSurface,
}

impl Drop for WorkerEgl {
    fn drop(&mut self) {
        unsafe {
            (self.libegl.eglMakeCurrent.unwrap())(
                self.display,
                egl_sys::EGL_NO_SURFACE,
                egl_sys::EGL_NO_SURFACE,
                egl_sys::EGL_NO_CONTEXT,
            );
            if !self.surface.is_null() {
                (self.libegl.eglDestroySurface.unwrap())(self.display, self.surface);
            }
            if !self.context.is_null() {
                (self.libegl.eglDestroyContext.unwrap())(self.display, self.context);
            }
            // Android counts initializations per display, so this only drops our own.
            (self.libegl.eglTerminate.unwrap())(self.display);
        }
    }
}

struct WorkerGl {
    libgl: LibGl,
    _egl: WorkerEgl,
}

impl WorkerGl {
    unsafe fn new(init: &WorkerInit) -> Result<Self, String> {
        let libegl = LibEgl::try_load().ok_or("libEGL didn't load")?;
        let display = (libegl.eglGetDisplay.unwrap())(null_mut());
        if (libegl.eglInitialize.unwrap())(display, null_mut(), null_mut()) == 0 {
            return Err(format!("eglInitialize failed: EGL error {:#x}", libegl.last_error()));
        }
        let mut egl = WorkerEgl {
            libegl,
            display,
            context: egl_sys::EGL_NO_CONTEXT,
            surface: egl_sys::EGL_NO_SURFACE,
        };
        let mut config = init.egl_config as egl_sys::EGLConfig;
        let mut surface_type = 0;
        (egl.libegl.eglGetConfigAttrib.unwrap())(
            display,
            config,
            egl_sys::EGL_SURFACE_TYPE as _,
            &mut surface_type,
        );
        let has_pbuffer = (surface_type as u32 & egl_sys::EGL_PBUFFER_BIT) != 0;
        let extensions = (egl.libegl.eglQueryString.unwrap())(display, egl_sys::EGL_EXTENSIONS as _);
        let surfaceless = !extensions.is_null()
            && CStr::from_ptr(extensions)
                .to_string_lossy()
                .split_whitespace()
                .any(|extension| extension == "EGL_KHR_surfaceless_context");
        // The render thread's config is for windows, so a 1x1 pbuffer may need one that allows those.
        if !has_pbuffer && !surfaceless {
            let attributes = [
                egl_sys::EGL_RENDERABLE_TYPE,
                egl_sys::EGL_OPENGL_ES3_BIT_KHR,
                egl_sys::EGL_SURFACE_TYPE,
                egl_sys::EGL_PBUFFER_BIT,
                egl_sys::EGL_RED_SIZE,
                8,
                egl_sys::EGL_GREEN_SIZE,
                8,
                egl_sys::EGL_BLUE_SIZE,
                8,
                egl_sys::EGL_NONE,
            ];
            let mut count = 0;
            (egl.libegl.eglChooseConfig.unwrap())(
                display,
                attributes.as_ptr() as _,
                &mut config,
                1,
                &mut count as *mut _ as *mut _,
            );
            if count < 1 {
                return Err("no EGL config allows a pbuffer".to_string());
            }
        }
        let context_attributes = [egl_sys::EGL_CONTEXT_MAJOR_VERSION, 3, egl_sys::EGL_NONE];
        egl.context = (egl.libegl.eglCreateContext.unwrap())(
            display,
            config,
            egl_sys::EGL_NO_CONTEXT,
            context_attributes.as_ptr() as _,
        );
        if egl.context.is_null() {
            return Err(format!("eglCreateContext failed: EGL error {:#x}", egl.libegl.last_error()));
        }
        if has_pbuffer || !surfaceless {
            let pbuffer_attributes = [
                egl_sys::EGL_WIDTH as i32,
                1,
                egl_sys::EGL_HEIGHT as i32,
                1,
                egl_sys::EGL_NONE as i32,
            ];
            egl.surface =
                (egl.libegl.eglCreatePbufferSurface.unwrap())(display, config, pbuffer_attributes.as_ptr());
            if egl.surface.is_null() {
                return Err(format!(
                    "eglCreatePbufferSurface failed: EGL error {:#x}",
                    egl.libegl.last_error()
                ));
            }
        }
        if (egl.libegl.eglMakeCurrent.unwrap())(display, egl.surface, egl.surface, egl.context) == 0 {
            return Err(format!("eglMakeCurrent failed: EGL error {:#x}", egl.libegl.last_error()));
        }
        let libgl = load_libgl(&egl.libegl)?;
        if get_driver_strings(&libgl) != init.driver {
            return Err("its GL driver strings differ from the render thread's".to_string());
        }
        Ok(Self { libgl, _egl: egl })
    }
}

/// Leaves the queue usable and tells the render thread, however the worker ends.
struct ExitGuard(Arc<Shared>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        let mut queue = lock(&self.0.queue);
        queue.compiling = false;
        let stopped = queue.stop;
        drop(queue);
        if !stopped {
            crate::thread::wake_ui_event_loop();
        }
    }
}

fn run_worker(init: WorkerInit, shared: Arc<Shared>, replies: Sender<Reply>) {
    let tid = unsafe { libc_sys::syscall(libc_sys::SYS_GETTID) } as i32;
    shared.tid.store(tid, Ordering::Relaxed);
    let _exit_guard = ExitGuard(shared.clone());
    // Declared after the guard, so the channel closes before the guard wakes the render thread.
    let replies = replies;
    GlShader::remove_stale_cache_files(&init.cache_dir, init.android_suffix.as_deref());
    let mut gl: Option<WorkerGl> = None;
    loop {
        let next = {
            let mut queue = lock(&shared.queue);
            loop {
                if queue.stop {
                    return;
                }
                if !queue.ui_loading && !queue.handing_over {
                    if let Some(job) = queue.jobs.pop_front() {
                        queue.in_flight = Some(job.os_shader_id);
                        let wanted = queue.wanted.contains(&job.os_shader_id);
                        let skip_cache = queue.skip_cache.contains(&job.os_shader_id);
                        break Some((job, wanted, skip_cache));
                    }
                }
                // We keep no context while there's nothing to build, and tear it down outside the lock.
                if queue.jobs.is_empty() && gl.is_some() {
                    break None;
                }
                queue = shared.changed.wait(queue).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let Some((job, wanted, skip_cache)) = next else {
            gl = None;
            continue;
        };
        let os_shader_id = job.os_shader_id;
        // An earlier launch built it, so the render thread loads it itself. We decide that under
        // the lock, so a draw that just asked for it still gets a reply.
        if !skip_cache && Path::new(&job.cache_path).exists() {
            let mut queue = lock(&shared.queue);
            queue.in_flight = None;
            if !queue.wanted.remove(&os_shader_id) {
                continue;
            }
            queue.handing_over = true;
            let stopped = queue.stop;
            drop(queue);
            if stopped || replies.send(Reply::Cached { os_shader_id }).is_err() {
                return;
            }
            crate::thread::wake_ui_event_loop();
            continue;
        }
        shared.found_uncached.store(true, Ordering::Relaxed);
        if gl.is_none() {
            match unsafe { WorkerGl::new(&init) } {
                Ok(new_gl) => gl = Some(new_gl),
                Err(reason) => {
                    let _ = replies.send(Reply::Unavailable(reason));
                    return;
                }
            }
        }
        let libgl = &gl.as_ref().unwrap().libgl;
        // Lowering our priority while holding the lock could leave the render thread waiting on us.
        set_current_thread_priority(if wanted { CxThreadPriority::Normal } else { CxThreadPriority::Background });
        {
            let mut queue = lock(&shared.queue);
            let other_is_wanted = !queue.wanted.contains(&os_shader_id)
                && queue.jobs.front().is_some_and(|front| queue.wanted.contains(&front.os_shader_id));
            if queue.ui_loading || queue.stop || other_is_wanted {
                queue.in_flight = None;
                // A draw waiting on another program goes first.
                if other_is_wanted {
                    queue.jobs.push_back(job);
                } else {
                    queue.jobs.push_front(job);
                }
                continue;
            }
            queue.compiling = true;
            // A draw started waiting on this one after we took it, and raising is fine under the lock.
            if !wanted && queue.wanted.contains(&os_shader_id) {
                set_current_thread_priority(CxThreadPriority::Normal);
            }
        }
        let pending =
            GlShader::start_pending_program_compile(libgl, &job.vertex, &job.pixel, &init.os_type);
        if pending.program == 0 {
            let _ = replies.send(Reply::Unavailable("its GL context is gone".to_string()));
            return;
        }
        let error = GlShader::get_pending_program_error(libgl, &pending, &job.vertex, &job.pixel);
        let binary = error
            .is_none()
            .then(|| GlShader::get_program_binary(libgl, pending.program))
            .flatten();
        pending.free_resources(libgl);
        let reply = match (error, binary) {
            (Some(error), _) => {
                crate::error!("{}", error);
                Reply::Rejected { os_shader_id }
            }
            (None, Some((binary_format, binary))) => {
                if let Err(e) = GlShader::write_cache_file(&job.cache_path, binary_format, &binary) {
                    crate::error!("Failed to write shader cache to {}, error: {e}", job.cache_path);
                }
                Reply::Built { os_shader_id, binary_format, binary }
            }
            (None, None) => {
                let _ = replies.send(Reply::Unavailable("the driver hands back no program binaries".to_string()));
                return;
            }
        };
        let mut queue = lock(&shared.queue);
        queue.compiling = false;
        queue.in_flight = None;
        queue.wanted.remove(&os_shader_id);
        queue.skip_cache.remove(&os_shader_id);
        // Rejections hand over too, so a draw can't ask for this program again before it's marked failed.
        queue.handing_over = true;
        let stopped = queue.stop;
        drop(queue);
        if stopped || replies.send(reply).is_err() {
            return;
        }
        crate::thread::wake_ui_event_loop();
    }
}
