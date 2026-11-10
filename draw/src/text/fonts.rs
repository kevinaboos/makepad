use {
    super::{
        font::{Font, FontId, GlyphId},
        font_atlas::{ColorAtlas, GlyphImageKind},
        font_family::{FontFamily, FontFamilyId},
        geom::Size,
        image::{Bgra, Image},
        layouter::{self, LaidoutText, LayoutParams, Layouter},
        loader::{FontDefinition, FontFamilyDefinition},
        msdfer::Msdfer,
        rasterizer::{
            rasterize_outline_sdf, CompletedMsdfJob, CompletedSdfJob, OutlineRasterizationMode,
            QueuedOutlineJob, Rasterizer,
        },
        sdfer::Sdfer,
        slug_atlas::{SlugAtlas, SlugGlyphCacheResult},
        text_counters::TextCounters,
    },
    crate::{cx_draw::CxDraw, makepad_platform::*},
    fxhash::{FxHashMap, FxHashSet},
    std::{cell::RefCell, mem::ManuallyDrop, rc::Rc, sync::mpsc},
};

/// How we draw glyphs that have outlines. Color glyphs like emoji always come from the atlas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GlyphRendering {
    /// Slug on Android, Apple and the web, and the atlas below 128 dpx/em on Linux and Windows.
    #[default]
    PlatformDefault,
    /// The GPU draws every glyph from its outline's curves.
    Slug,
    /// Glyphs below 128 dpx/em come from the SDF atlas, which we fill on the UI thread as they show up.
    Atlas,
    /// Like `Atlas`, but a worker builds the SDFs, and a glyph draws with Slug until its SDF is ready.
    ///
    /// Linux and Windows draw each run of text either all with Slug or all from the atlas,
    /// so there this is the same as `Atlas`.
    Hybrid,
}

pub fn get_glyph_rendering(cx: &Cx) -> GlyphRendering {
    cx.get_global_ref::<GlyphRendering>().copied().unwrap_or_default()
}

/// Sets how we draw glyphs from now on, redrawing everything if text already exists.
///
/// It's fine to call this before any text exists, like in `app_main!`'s `configure`.
pub fn set_glyph_rendering(cx: &mut Cx, glyph_rendering: GlyphRendering) {
    if *cx.global::<GlyphRendering>() == glyph_rendering {
        return;
    }
    *cx.global::<GlyphRendering>() = glyph_rendering;
    if !cx.has_global::<Rc<RefCell<Fonts>>>() {
        return;
    }
    let fonts = cx.get_global::<Rc<RefCell<Fonts>>>().clone();
    fonts.borrow_mut().apply_glyph_rendering(cx, glyph_rendering);
    cx.redraw_all();
}

/// Redraws the text that drew Slug in place of SDFs the worker has since built.
///
/// Window calls this for every event, so an idle screen still swaps over to those SDFs.
/// It also shrinks the atlas once it's been idle or when the app goes to the background.
pub fn handle_event(cx: &mut Cx, event: &Event) {
    if !matches!(event, Event::Signal | Event::Timer(_) | Event::Background)
        || !cx.has_global::<Rc<RefCell<Fonts>>>()
    {
        return;
    }
    let fonts = cx.get_global::<Rc<RefCell<Fonts>>>().clone();
    let mut fonts = fonts.borrow_mut();
    match event {
        Event::Signal => {
            fonts.receive_completed_sdf_jobs();
            fonts.redraw_slug_fallbacks_if_sdfs_are_done(cx);
        }
        Event::Background => fonts.shrink_atlas(cx),
        Event::Timer(timer) if fonts.atlas_shrink_timer.is_timer(timer).is_some() => {
            fonts.shrink_atlas(cx);
            cx.redraw_all();
        }
        _ => {}
    }
}

/// The most one copy of the atlas can take up, as much as a 4096x4096 BGRA texture.
const MAX_ATLAS_BYTES: usize = 64 * 1024 * 1024;

/// How long a grown atlas waits for new glyphs before it goes back to its base size.
const ATLAS_IDLE_SECS: f64 = 300.0;

#[derive(Default)]
struct LazyFontRequests {
    requested: FxHashSet<(FontFamilyId, LazyFontFamily)>,
}

impl LazyFontRequests {
    fn take_for_glyph_miss(
        &mut self,
        family_id: FontFamilyId,
        lazy_family: LazyFontFamily,
        text: &str,
    ) -> bool {
        text.chars().any(|ch| lazy_family.contains(ch))
            && self.requested.insert((family_id, lazy_family))
    }

    fn contains(&self, family_id: FontFamilyId, lazy_family: LazyFontFamily) -> bool {
        self.requested.contains(&(family_id, lazy_family))
    }
}

fn default_slug_new_glyphs_per_redraw(cx: &Cx) -> usize {
    match cx.os_type() {
        OsType::LinuxWindow(_) | OsType::LinuxDirect | OsType::Windows => 1,
        _ => usize::MAX,
    }
}

fn default_slug_min_dpxs_per_em(cx: &Cx, rasterizer: &Rasterizer) -> f32 {
    match cx.os_type() {
        OsType::LinuxWindow(_) | OsType::LinuxDirect | OsType::Windows => {
            rasterizer.msdf_resolution().max_dpxs_per_em
        }
        _ => 0.0,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FontsMemoryBytes {
    pub atlas_bytes: usize,
    pub layout_cache_bytes: usize,
}

pub struct Fonts {
    layouter: Layouter,
    lazy_font_requests: LazyFontRequests,
    missing_font_paths: FxHashSet<String>,
    needs_prepare_atlases: bool,
    /// The counts we keep here, which `text_counters` joins with the atlas and rasterizer ones.
    pub(crate) counters: TextCounters,
    atlas_texture: Texture,
    atlas_base_size: Size<usize>,
    atlas_shrink_timer: Timer,
    /// The `redraw_id` of the frame that fills an atlas we just emptied.
    atlas_refill_redraw_id: u64,
    /// Cleared once a refill fits.
    seen_atlas_overflow_hashes: FxHashSet<u64>,
    slug_atlas: SlugAtlas,
    slug_min_dpxs_per_em: f32,
    slug_new_glyphs_per_redraw: usize,
    slug_budget_redraw_id: u64,
    slug_built_glyphs_this_redraw: usize,
    glyph_rendering: GlyphRendering,
    default_outline_rasterization_mode: OutlineRasterizationMode,
    outline_job_sender: FromUISender<QueuedOutlineJob>,
    msdf_result_receiver: ToUIReceiver<CompletedMsdfJob>,
    sdf_result_receiver: mpsc::Receiver<CompletedSdfJob>,
    completed_sdf_jobs: Vec<CompletedSdfJob>,
    sdf_jobs_in_flight: usize,
    /// Whether SDFs came in since we last redrew the Slug fallbacks.
    has_new_sdfs: bool,
    /// The draw lists that drew Slug in place of an SDF that wasn't ready yet.
    /// The key is the draw list's ID,
    /// and the value is the `recording_gen` of the recording that drew the Slug glyphs.
    slug_fallback_draw_lists: FxHashMap<DrawListId, u64>,
}

impl Fonts {
    pub fn new(cx: &mut Cx, settings: layouter::Settings) -> Self {
        let layouter = Layouter::new(settings);
        let (
            atlas_size,
            msdfer_settings,
            sdfer_settings,
            default_outline_rasterization_mode,
            slug_min_dpxs_per_em,
        ) = {
            let rasterizer = layouter.rasterizer().borrow();
            (
                rasterizer.color_atlas().size(),
                rasterizer.msdfer().settings(),
                rasterizer.sdfer().settings(),
                rasterizer.outline_rasterization_mode(),
                default_slug_min_dpxs_per_em(cx, &rasterizer),
            )
        };

        let mut outline_job_sender: FromUISender<QueuedOutlineJob> = Default::default();
        let msdf_result_receiver: ToUIReceiver<CompletedMsdfJob> = Default::default();
        let (sdf_result_sender, sdf_result_receiver) = mpsc::channel();
        let worker_rx = outline_job_sender
            .receiver()
            .expect("MSDF worker receiver is taken exactly once");
        let worker_tx = msdf_result_receiver.sender();
        if let Ok(task) = cx.spawn_worker(move || {
            let mut msdfer = Msdfer::new(msdfer_settings);
            let mut sdfer = Sdfer::new(sdfer_settings);
            let mut has_unsignaled_sdfs = false;
            loop {
                let job = match worker_rx.try_recv() {
                    Ok(job) => job,
                    Err(_) => {
                        // We're out of jobs, so we tell the UI thread once about all the SDFs we built.
                        if has_unsignaled_sdfs {
                            has_unsignaled_sdfs = false;
                            SignalToUI::set_ui_signal();
                        }
                        let Ok(job) = worker_rx.recv() else {
                            break;
                        };
                        job
                    }
                };
                if job.key.kind == GlyphImageKind::OutlineSdf {
                    let sdf_image =
                        rasterize_outline_sdf(&mut sdfer, &job.outline, job.dpxs_per_em, job.key.size);
                    let completed_job = CompletedSdfJob {
                        key: job.key,
                        sdf_image,
                        epoch: job.epoch,
                    };
                    if sdf_result_sender.send(completed_job).is_err() {
                        break;
                    }
                    has_unsignaled_sdfs = true;
                    continue;
                }
                let mut msdf = Image::<Bgra>::new(job.key.size);
                msdfer.outline_to_msdf(
                    &job.outline,
                    job.dpxs_per_em,
                    &mut msdf.subimage_mut(super::geom::Rect::from(job.key.size)),
                );
                if worker_tx
                    .send(CompletedMsdfJob {
                        key: job.key,
                        pixels: msdf.into_pixels(),
                        epoch: job.epoch,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }) {
            task.detach();
        }

        let mut fonts = Self {
            layouter,
            lazy_font_requests: Default::default(),
            missing_font_paths: FxHashSet::default(),
            needs_prepare_atlases: false,
            counters: TextCounters::default(),
            atlas_texture: Texture::new_with_format(
                cx,
                TextureFormat::VecBGRAu8_32 {
                    width: atlas_size.width,
                    height: atlas_size.height,
                    data: None,
                    updated: TextureUpdated::Empty,
                },
            ),
            atlas_base_size: atlas_size,
            atlas_shrink_timer: Timer::empty(),
            atlas_refill_redraw_id: 0,
            seen_atlas_overflow_hashes: FxHashSet::default(),
            slug_atlas: SlugAtlas::new(cx),
            slug_min_dpxs_per_em,
            slug_new_glyphs_per_redraw: default_slug_new_glyphs_per_redraw(cx),
            slug_budget_redraw_id: 0,
            slug_built_glyphs_this_redraw: 0,
            glyph_rendering: GlyphRendering::PlatformDefault,
            default_outline_rasterization_mode,
            outline_job_sender,
            msdf_result_receiver,
            sdf_result_receiver,
            completed_sdf_jobs: Vec::new(),
            sdf_jobs_in_flight: 0,
            has_new_sdfs: false,
            slug_fallback_draw_lists: FxHashMap::default(),
        };
        let glyph_rendering = *cx.global::<GlyphRendering>();
        if glyph_rendering != GlyphRendering::PlatformDefault {
            fonts.apply_glyph_rendering(cx, glyph_rendering);
        }
        fonts
    }

    fn apply_glyph_rendering(&mut self, cx: &Cx, glyph_rendering: GlyphRendering) {
        let mut rasterizer = self.layouter.rasterizer().borrow_mut();
        let (slug_min_dpxs_per_em, outline_rasterization_mode) = match glyph_rendering {
            GlyphRendering::PlatformDefault => (
                default_slug_min_dpxs_per_em(cx, &rasterizer),
                self.default_outline_rasterization_mode,
            ),
            GlyphRendering::Slug => (0.0, OutlineRasterizationMode::Sdf),
            GlyphRendering::Atlas | GlyphRendering::Hybrid => (
                rasterizer.msdf_resolution().max_dpxs_per_em,
                OutlineRasterizationMode::Sdf,
            ),
        };
        rasterizer.set_outline_rasterization_mode(outline_rasterization_mode);
        self.slug_min_dpxs_per_em = slug_min_dpxs_per_em;
        self.glyph_rendering = glyph_rendering;
    }

    pub fn rasterizer(&self) -> &Rc<RefCell<Rasterizer>> {
        self.layouter.rasterizer()
    }

    pub fn outline_rasterization_mode(&self) -> OutlineRasterizationMode {
        self.layouter
            .rasterizer()
            .borrow()
            .outline_rasterization_mode()
    }

    pub fn set_outline_rasterization_mode(&mut self, mode: OutlineRasterizationMode) {
        self.layouter
            .rasterizer()
            .borrow_mut()
            .set_outline_rasterization_mode(mode);
    }

    pub fn grayscale_texture(&self) -> &Texture {
        &self.atlas_texture
    }

    pub fn color_texture(&self) -> &Texture {
        &self.atlas_texture
    }

    pub fn msdf_texture(&self) -> &Texture {
        &self.atlas_texture
    }

    pub fn slug_curve_texture(&self) -> &Texture {
        self.slug_atlas.curve_texture()
    }

    pub fn slug_band_texture(&self) -> &Texture {
        self.slug_atlas.band_texture()
    }

    pub fn should_use_slug_glyph(&self, dpxs_per_em: f32) -> bool {
        dpxs_per_em >= self.slug_min_dpxs_per_em
    }

    /// Returns whether a glyph should come from an SDF the worker builds, drawing with Slug until then.
    ///
    /// This is never true on Linux and Windows, see [`GlyphRendering::Hybrid`].
    pub fn should_use_hybrid_glyph(&self, dpxs_per_em: f32) -> bool {
        !cfg!(any(target_os = "linux", target_os = "windows"))
            && self.glyph_rendering == GlyphRendering::Hybrid
            && !self.should_use_slug_glyph(dpxs_per_em)
    }

    /// Remembers that the draw list we're recording drew Slug in place of an SDF that isn't ready yet.
    pub fn mark_draw_list_as_drawing_slug_fallbacks(&mut self, cx: &CxDraw) {
        let draw_list_id = *cx.draw_list_stack.last().unwrap();
        self.slug_fallback_draw_lists
            .insert(draw_list_id, cx.draw_lists[draw_list_id].recording_gen);
    }

    pub fn max_rasterized_glyph_dpxs_per_em(&self) -> f32 {
        self.layouter
            .rasterizer()
            .borrow()
            .msdf_resolution()
            .max_dpxs_per_em
    }

    pub fn get_or_cache_slug_glyph(
        &mut self,
        redraw_id: u64,
        font: &Font,
        glyph_id: GlyphId,
    ) -> SlugGlyphCacheResult {
        match self.slug_atlas.get_or_cache_glyph(font, glyph_id, false) {
            SlugGlyphCacheResult::Deferred => {}
            result => return result,
        }

        if self.slug_new_glyphs_per_redraw != usize::MAX {
            if self.slug_budget_redraw_id != redraw_id {
                self.slug_budget_redraw_id = redraw_id;
                self.slug_built_glyphs_this_redraw = 0;
            }
            if self.slug_built_glyphs_this_redraw >= self.slug_new_glyphs_per_redraw {
                return SlugGlyphCacheResult::Deferred;
            }
            self.slug_built_glyphs_this_redraw += 1;
        }

        self.slug_atlas.get_or_cache_glyph(font, glyph_id, true)
    }

    pub fn slug_cache_generation(&self) -> u64 {
        self.slug_atlas.cache_generation()
    }

    pub fn slug_uploaded_generation(&self) -> u64 {
        self.slug_atlas.uploaded_generation()
    }

    /// Uploads any newly appended SLUG curve/band data immediately so draw calls
    /// in the current frame can see glyphs cached during the draw loop.
    /// CPU bytes the text system holds: the glyph atlas pixels while the
    /// atlas (not its texture) owns them, and the laid-out text cache. Font
    /// file bytes are script resources and are counted by `Cx::memory_report`.
    pub fn memory_bytes(&self) -> FontsMemoryBytes {
        let rasterizer = self.layouter.rasterizer().borrow();
        FontsMemoryBytes {
            atlas_bytes: rasterizer
                .color_atlas()
                .image()
                .as_pixels()
                .len()
                .saturating_mul(4),
            layout_cache_bytes: self.layouter.cache_bytes(),
        }
    }

    pub fn text_counters(&self) -> TextCounters {
        let rasterizer = self.layouter.rasterizer().borrow();
        TextCounters {
            slug_glyphs_built: self.slug_atlas.glyphs_built,
            slug_upload_bytes: self.slug_atlas.upload_bytes,
            sdf_glyphs_rasterized: rasterizer.sdf_glyphs_rasterized,
            msdf_jobs_queued: rasterizer.msdf_jobs_queued,
            ..self.counters
        }
    }

    pub fn flush_slug_textures(&mut self, cx: &mut Cx) {
        self.slug_atlas.prepare_textures(cx);
    }

    pub fn is_font_family_known(&self, id: FontFamilyId) -> bool {
        self.layouter.is_font_family_known(id)
    }

    pub(crate) fn lazy_font_is_requested(
        &self,
        id: FontFamilyId,
        family: LazyFontFamily,
    ) -> bool {
        self.lazy_font_requests.contains(id, family)
    }

    pub(crate) fn take_lazy_font_request(
        &mut self,
        id: FontFamilyId,
        family: LazyFontFamily,
        text: &str,
    ) -> bool {
        let is_new_request = self.lazy_font_requests.take_for_glyph_miss(id, family, text);
        if is_new_request {
            // The family expects this font from now on, so its fonts aren't all loaded anymore.
            self.layouter.font_family_generation += 1;
        }
        is_new_request
    }

    pub fn is_font_family_complete(
        &self,
        id: FontFamilyId,
        expected_member_count: usize,
    ) -> bool {
        self.layouter
            .loader
            .font_family_definitions
            .get(&id)
            .map(|def| {
                def.expected_member_count == expected_member_count
                    && def.font_ids.len() == expected_member_count
            })
            .unwrap_or(false)
    }

    /// Returns whether a family we laid out text with is still waiting for some of its fonts.
    ///
    /// That text changes once they arrive, which we only notice the next time the family is used.
    pub fn is_font_family_loading(&self, id: FontFamilyId) -> bool {
        self.layouter.loader.is_font_family_loading(id)
    }

    pub fn is_font_known(&self, id: FontId) -> bool {
        self.layouter.is_font_known(id)
    }

    /// True the first time a resource path fails to load, so the error logs once.
    pub fn note_missing_font(&mut self, resource_path: &str) -> bool {
        self.missing_font_paths.insert(resource_path.to_string())
    }

    pub fn define_font_family(&mut self, id: FontFamilyId, definition: FontFamilyDefinition) {
        self.layouter.define_font_family(id, definition);
    }

    pub fn set_font_family_definition(
        &mut self,
        id: FontFamilyId,
        definition: FontFamilyDefinition,
    ) {
        self.layouter.set_font_family_definition(id, definition);
    }

    /// See [`Layouter::font_family_generation`].
    pub fn font_family_generation(&self) -> u64 {
        self.layouter.font_family_generation()
    }

    pub fn define_font(&mut self, id: FontId, definition: FontDefinition) {
        self.layouter.define_font(id, definition);
    }

    pub fn get_or_load_font_family(&mut self, id: FontFamilyId) -> Rc<FontFamily> {
        self.layouter.get_or_load_font_family(id)
    }

    pub fn get_or_layout(&mut self, params: impl LayoutParams) -> Rc<LaidoutText> {
        self.layouter.get_or_layout(params)
    }

    pub fn prepare_textures(&mut self, cx: &mut Cx) -> bool {
        assert!(!self.needs_prepare_atlases);
        // Frame boundary for the layout cache's working-set protection.
        self.layouter.advance_cache_generation();
        let overflow = std::mem::take(&mut self.layouter.rasterizer().borrow_mut().atlas_overflow_hash);
        let is_refill = cx.redraw_id == self.atlas_refill_redraw_id;
        if is_refill && overflow == 0 {
            self.seen_atlas_overflow_hashes.clear();
        }
        // A refill already holds all it can of this frame's glyphs, so a reset wouldn't help it,
        // and it wouldn't help any later frame that misses the same glyphs either.
        let is_new_overflow = overflow != 0 && self.seen_atlas_overflow_hashes.insert(overflow);
        let needs_atlas_reset = is_new_overflow && !is_refill;
        let needs_reset = needs_atlas_reset || self.slug_atlas.needs_reset();
        if !needs_reset {
            self.receive_completed_sdf_jobs();
            self.dispatch_outline_jobs();
            self.redraw_slug_fallbacks_if_sdfs_are_done(cx);
            let completed = self.apply_completed_msdf_jobs();
            if completed > 0 {
                cx.redraw_all();
            }
        }
        // We upload before any reset, since this frame still draws from these slots and curves.
        self.flush_slug_textures(cx);
        self.prepare_atlas_texture(cx);
        self.needs_prepare_atlases = true;
        if needs_reset {
            if needs_atlas_reset {
                self.layouter.rasterizer().borrow_mut().on_atlas_reset();
                self.atlas_refill_redraw_id = cx.redraw_id + 1;
            }
            self.slug_atlas.reset_if_needed();
        }
        !needs_reset
    }

    fn prepare_atlas_texture(&mut self, cx: &mut Cx) {
        let mut rasterizer = self.layouter.rasterizer().borrow_mut();
        let dirty_rect = rasterizer.color_atlas_mut().take_dirty_image().bounds();
        let size = rasterizer.color_atlas().size();
        let pixels = bgra_vec_into_u32(rasterizer.color_atlas_mut().take_pixels());
        // A backend that reallocates for this sends the whole image, see `TextureUpdated`.
        if let TextureFormat::VecBGRAu8_32 { height, .. } = self.atlas_texture.get_format(cx) {
            *height = size.height;
        }
        self.atlas_texture.put_back_vec_u32(
            cx,
            pixels,
            Some(RectUsize::new(
                PointUsize::new(dirty_rect.origin.x, dirty_rect.origin.y),
                SizeUsize::new(dirty_rect.size.width, dirty_rect.size.height),
            )),
        );
        self.atlas_texture.set_content_epoch(cx, rasterizer.epoch());
        // A refill doesn't start the countdown, or a screen that needs the extra rows would
        // shrink and grow back forever. The next frame starts it instead.
        if size != self.atlas_base_size
            && cx.redraw_id != self.atlas_refill_redraw_id
            && (!dirty_rect.is_empty() || self.atlas_shrink_timer.is_empty())
        {
            cx.stop_timer(self.atlas_shrink_timer);
            self.atlas_shrink_timer = cx.start_timeout(ATLAS_IDLE_SECS);
        }
    }

    /// Goes back to the base size, dropping the CPU pixels here and giving up the GPU texture.
    fn shrink_atlas(&mut self, cx: &mut Cx) {
        let size = self.atlas_base_size;
        let mut rasterizer = self.layouter.rasterizer().borrow_mut();
        if rasterizer.color_atlas().size() == size {
            return;
        }
        *rasterizer.color_atlas_mut() = ColorAtlas::new(size);
        rasterizer.on_atlas_reset();
        let pixels = bgra_vec_into_u32(rasterizer.color_atlas_mut().take_pixels());
        self.atlas_texture.release(cx);
        self.atlas_texture.set_data_u32(cx, size.width, size.height, pixels);
        cx.stop_timer(self.atlas_shrink_timer);
        self.atlas_shrink_timer = Timer::empty();
        self.atlas_refill_redraw_id = cx.redraw_id + 1;
    }

    pub fn prepare_atlases_if_needed(&mut self, cx: &mut Cx) {
        if self.needs_prepare_atlases {
            self.prepare_atlas(cx);
            self.needs_prepare_atlases = false;
        }
        // We write finished SDFs in now rather than at the end of the draw,
        // so this draw can already use them.
        self.receive_completed_sdf_jobs();
        let mut rasterizer = self.layouter.rasterizer().borrow_mut();
        for job in self.completed_sdf_jobs.drain(..) {
            rasterizer.apply_completed_sdf_job(job);
        }
    }

    fn prepare_atlas(&mut self, cx: &mut Cx) {
        let mut rasterizer = self.layouter.rasterizer().borrow_mut();
        let pixels = self.atlas_texture.take_vec_u32(cx);
        let pixels = u32_vec_into_bgra(pixels);
        rasterizer.color_atlas_mut().replace_pixels(pixels);
        let max_bytes = MAX_ATLAS_BYTES.min(cx.memory_budget() / 16);
        rasterizer.max_atlas_height = (max_bytes / (self.atlas_base_size.width * 4))
            .min(cx.gpu_info().max_texture_size)
            .max(self.atlas_base_size.height);
    }

    fn dispatch_outline_jobs(&mut self) {
        let (msdf_jobs, sdf_jobs) = {
            let mut rasterizer = self.layouter.rasterizer().borrow_mut();
            (rasterizer.take_queued_msdf_jobs(), rasterizer.take_queued_sdf_jobs())
        };
        for job in msdf_jobs {
            let _ = self.outline_job_sender.send(job);
        }
        for job in sdf_jobs {
            if self.outline_job_sender.send(job).is_ok() {
                self.sdf_jobs_in_flight += 1;
            }
        }
    }

    fn receive_completed_sdf_jobs(&mut self) {
        for job in self.sdf_result_receiver.try_iter() {
            self.completed_sdf_jobs.push(job);
            self.sdf_jobs_in_flight -= 1;
            self.has_new_sdfs = true;
        }
    }

    /// Once the worker has built every SDF we gave it, redraws the lists that drew Slug in their place.
    fn redraw_slug_fallbacks_if_sdfs_are_done(&mut self, cx: &mut Cx) {
        if self.sdf_jobs_in_flight != 0 || !self.has_new_sdfs {
            return;
        }
        self.has_new_sdfs = false;
        for (draw_list_id, recording_gen) in self.slug_fallback_draw_lists.drain() {
            // A list recorded again since then already drew whatever SDFs were ready.
            if cx
                .draw_lists
                .checked_index(draw_list_id)
                .is_some_and(|draw_list| draw_list.recording_gen == recording_gen)
            {
                cx.redraw_list_in_draw(draw_list_id);
            }
        }
    }

    fn apply_completed_msdf_jobs(&mut self) -> usize {
        let mut completed = 0usize;
        while let Ok(job) = self.msdf_result_receiver.try_recv() {
            self.layouter
                .rasterizer()
                .borrow_mut()
                .apply_completed_msdf_job(job);
            completed += 1;
        }
        self.counters.msdf_jobs_completed += completed as u64;
        completed
    }
}

fn bgra_vec_into_u32(vec: Vec<Bgra>) -> Vec<u32> {
    debug_assert_eq!(std::mem::size_of::<Bgra>(), std::mem::size_of::<u32>());
    debug_assert_eq!(std::mem::align_of::<Bgra>(), std::mem::align_of::<u32>());
    let mut vec = ManuallyDrop::new(vec);
    // SAFETY:
    // `Bgra` is `#[repr(transparent)]` over `u32`, so element layout matches exactly.
    // We preserve the same pointer/len/cap and only reinterpret the element type.
    unsafe { Vec::from_raw_parts(vec.as_mut_ptr().cast::<u32>(), vec.len(), vec.capacity()) }
}

pub(super) fn u32_vec_into_bgra(vec: Vec<u32>) -> Vec<Bgra> {
    debug_assert_eq!(std::mem::size_of::<Bgra>(), std::mem::size_of::<u32>());
    debug_assert_eq!(std::mem::align_of::<Bgra>(), std::mem::align_of::<u32>());
    let mut vec = ManuallyDrop::new(vec);
    // SAFETY:
    // `Bgra` is `#[repr(transparent)]` over `u32`, so element layout matches exactly.
    // We preserve the same pointer/len/cap and only reinterpret the element type.
    unsafe { Vec::from_raw_parts(vec.as_mut_ptr().cast::<Bgra>(), vec.len(), vec.capacity()) }
}

#[cfg(test)]
mod tests {
    use super::LazyFontRequests;
    use crate::{
        makepad_platform::LazyFontFamily,
        text::font_family::FontFamilyId,
    };

    #[test]
    fn glyph_miss_requests_each_lazy_family_once() {
        let family_id: FontFamilyId = 0xFA17_u64.into();
        let mut requests = LazyFontRequests::default();
        assert!(!requests.take_for_glyph_miss(
            family_id,
            LazyFontFamily::Cjk,
            "abc",
        ));
        assert!(requests.take_for_glyph_miss(
            family_id,
            LazyFontFamily::Cjk,
            "\u{4f60}",
        ));
        assert!(!requests.take_for_glyph_miss(
            family_id,
            LazyFontFamily::Cjk,
            "\u{597d}",
        ));
        assert!(requests.take_for_glyph_miss(
            family_id,
            LazyFontFamily::Emoji,
            "\u{1f600}",
        ));
        assert!(!requests.take_for_glyph_miss(
            family_id,
            LazyFontFamily::Emoji,
            "\u{1f680}",
        ));
    }
}
