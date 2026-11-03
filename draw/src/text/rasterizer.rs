use super::{
    font::{Font, FontId, GlyphId},
    font_atlas::{ColorAtlas, GlyphImageKey, GlyphImageKind, GrayscaleAtlas, MsdfAtlas},
    geom::{Point, Rect, Size},
    glyph_outline::{GlyphOutline, OutlineComplexity},
    image::{Bgra, Image, R},
    msdfer,
    msdfer::Msdfer,
    sdfer,
    sdfer::Sdfer,
};
use fxhash::{FxHashMap, FxHashSet};
use std::rc::Rc;
//use std::{fs::File, io::BufWriter, path::Path, slice};

#[derive(Debug)]
pub struct Rasterizer {
    sdfer: Sdfer,
    msdfer: Msdfer,
    msdf_resolution: MsdfResolutionSettings,
    msdf_complexity: MsdfComplexitySettings,
    outline_rasterization_mode: OutlineRasterizationMode,
    pub(super) sdf_glyphs_rasterized: u64,
    pub(super) msdf_jobs_queued: u64,
    atlas: ColorAtlas,
    allocator: MultiPlaneAllocator,
    cached_slots: FxHashMap<GlyphImageKey, AtlasSlot>,
    outline_msdf_ready: FxHashSet<GlyphImageKey>,
    outline_msdf_pending: FxHashSet<GlyphImageKey>,
    outline_msdf_failed: FxHashSet<GlyphImageKey>,
    queued_msdf_jobs: Vec<QueuedOutlineJob>,
    outline_sdf_pending: FxHashSet<GlyphImageKey>,
    queued_sdf_jobs: Vec<QueuedOutlineJob>,
    /// The fonts and sizes (as `f32` bits) whose printable ASCII we've already queued.
    prewarm_sdf_sizes: FxHashSet<(FontId, u32)>,
    /// The fonts and sizes whose printable ASCII we queue after this frame's own glyphs.
    queued_sdf_prewarms: Vec<(Rc<Font>, f32)>,
    atlas_epoch: u64,
}

impl Rasterizer {
    pub(crate) fn epoch(&self) -> u64 {
        self.atlas_epoch
    }

    pub(crate) fn export_glyph(&self, glyph: RasterizedGlyph) -> Option<PublishedGlyph> {
        let key = self.cached_slots.iter().find_map(|(key, slot)| {
            (slot.rect == glyph.atlas_image_bounds && slot.plane.index() == glyph.atlas_plane)
                .then(|| key.clone())
        })?;
        let mut pixels = Vec::with_capacity(key.size.width * key.size.height);
        for y in 0..key.size.height {
            for x in 0..key.size.width {
                pixels.push(self.atlas.image()[glyph.atlas_image_bounds.origin + Size::new(x, y)]);
            }
        }
        Some(PublishedGlyph { key, glyph, pixels })
    }

    /// Install worker-rasterized pixels; this path never loads an outline or shapes text.
    pub(crate) fn import_glyph(&mut self, published: &PublishedGlyph) -> Option<RasterizedGlyph> {
        let (slot, fresh) = if published.glyph.atlas_kind == AtlasKind::Grayscale {
            self.allocate_sdf_slot(published.key.clone())?
        } else {
            self.allocate_shared_slot(published.key.clone())?
        };
        if fresh {
            let mut image = self.atlas.get_cached_glyph_image_mut(slot.rect);
            let src_plane = AtlasPlane::from_index(published.glyph.atlas_plane as usize);
            for y in 0..slot.rect.size.height {
                for x in 0..slot.rect.size.width {
                    let at = Point::new(x, y);
                    let pixel = published.pixels[y * slot.rect.size.width + x];
                    image[at] = if published.glyph.atlas_kind == AtlasKind::Grayscale {
                        slot.plane.set(image[at], src_plane.get(pixel))
                    } else {
                        pixel
                    };
                }
            }
        }
        Some(RasterizedGlyph {
            atlas_size: self.atlas.size(),
            atlas_image_bounds: slot.rect,
            atlas_plane: slot.plane.index(),
            ..published.glyph
        })
    }

    pub fn new(settings: Settings) -> Self {
        let atlas_size = settings.atlas_size;
        Self {
            sdfer: Sdfer::new(settings.sdfer),
            msdfer: Msdfer::new(settings.msdfer),
            msdf_resolution: settings.msdf_resolution,
            msdf_complexity: settings.msdf_complexity,
            outline_rasterization_mode: settings.outline_rasterization_mode,
            sdf_glyphs_rasterized: 0,
            msdf_jobs_queued: 0,
            atlas: ColorAtlas::new(atlas_size),
            allocator: MultiPlaneAllocator::new(atlas_size),
            cached_slots: FxHashMap::default(),
            outline_msdf_ready: FxHashSet::default(),
            outline_msdf_pending: FxHashSet::default(),
            outline_msdf_failed: FxHashSet::default(),
            queued_msdf_jobs: Vec::new(),
            outline_sdf_pending: FxHashSet::default(),
            queued_sdf_jobs: Vec::new(),
            prewarm_sdf_sizes: FxHashSet::default(),
            queued_sdf_prewarms: Vec::new(),
            atlas_epoch: 0,
        }
    }

    pub fn sdfer(&self) -> &Sdfer {
        &self.sdfer
    }

    pub fn msdfer(&self) -> &Msdfer {
        &self.msdfer
    }

    pub fn msdf_resolution(&self) -> MsdfResolutionSettings {
        self.msdf_resolution
    }

    pub fn msdf_complexity(&self) -> MsdfComplexitySettings {
        self.msdf_complexity
    }

    pub fn outline_rasterization_mode(&self) -> OutlineRasterizationMode {
        self.outline_rasterization_mode
    }

    pub fn set_outline_rasterization_mode(&mut self, mode: OutlineRasterizationMode) {
        self.outline_rasterization_mode = mode;
    }

    pub fn on_atlas_reset(&mut self) {
        self.allocator.reset(self.atlas.size());
        self.cached_slots.clear();
        self.outline_msdf_ready.clear();
        self.outline_msdf_pending.clear();
        self.outline_msdf_failed.clear();
        self.queued_msdf_jobs.clear();
        self.outline_sdf_pending.clear();
        self.queued_sdf_jobs.clear();
        self.prewarm_sdf_sizes.clear();
        self.queued_sdf_prewarms.clear();
        self.atlas_epoch = self.atlas_epoch.wrapping_add(1);
    }

    pub fn take_queued_msdf_jobs(&mut self) -> Vec<QueuedOutlineJob> {
        std::mem::take(&mut self.queued_msdf_jobs)
    }

    /// Takes the SDF jobs queued since the last call, with the printable ASCII we prewarm last.
    pub fn take_queued_sdf_jobs(&mut self) -> Vec<QueuedOutlineJob> {
        for (font, dpxs_per_em) in std::mem::take(&mut self.queued_sdf_prewarms) {
            let glyph_ids = font.with_ttf_parser_face(|face| {
                (' '..='~')
                    .filter_map(|ch| face.glyph_index(ch))
                    .collect::<Vec<_>>()
            });
            for glyph_id in glyph_ids {
                if let Some((key, outline, dpxs_per_em)) =
                    self.get_glyph_outline_sdf_key(&font, glyph_id.0, dpxs_per_em)
                {
                    self.queue_glyph_outline_sdf(key, &outline, dpxs_per_em);
                }
            }
        }
        std::mem::take(&mut self.queued_sdf_jobs)
    }

    pub fn apply_completed_sdf_job(&mut self, job: CompletedSdfJob) {
        if job.epoch != self.atlas_epoch || !self.outline_sdf_pending.remove(&job.key) {
            return;
        }
        let Some((slot, allocated)) = self.allocate_sdf_slot(job.key) else {
            return;
        };
        if allocated {
            self.write_sdf_image(slot, &job.sdf_image);
        }
    }

    pub fn apply_completed_msdf_job(&mut self, job: CompletedMsdfJob) {
        if job.epoch != self.atlas_epoch {
            return;
        }
        if !self.outline_msdf_pending.remove(&job.key) {
            return;
        }
        let Some(slot) = self.cached_slots.get(&job.key).copied() else {
            return;
        };
        if job.pixels.len() != job.key.size.width.saturating_mul(job.key.size.height) {
            self.outline_msdf_failed.insert(job.key);
            return;
        }

        {
            let mut dst = self.atlas.get_cached_glyph_image_mut(slot.rect);
            for y in 0..job.key.size.height {
                for x in 0..job.key.size.width {
                    let point = Point::new(x, y);
                    let old = dst[point];
                    let msdf = job.pixels[y * job.key.size.width + x];
                    // Keep alpha as seeded SDF coverage for stable visual parity while RGB carries MSDF.
                    dst[point] = Bgra::new(msdf.b(), msdf.g(), msdf.r(), old.a());
                }
            }
        }
        self.outline_msdf_ready.insert(job.key);
    }

    pub fn grayscale_atlas(&self) -> &GrayscaleAtlas {
        &self.atlas
    }

    pub fn color_atlas(&self) -> &ColorAtlas {
        &self.atlas
    }

    pub fn msdf_atlas(&self) -> &MsdfAtlas {
        &self.atlas
    }

    pub fn grayscale_atlas_mut(&mut self) -> &mut GrayscaleAtlas {
        &mut self.atlas
    }

    pub fn color_atlas_mut(&mut self) -> &mut ColorAtlas {
        &mut self.atlas
    }

    pub fn msdf_atlas_mut(&mut self) -> &mut MsdfAtlas {
        &mut self.atlas
    }

    fn get_cached_slot(&self, key: &GlyphImageKey) -> Option<AtlasSlot> {
        self.cached_slots.get(key).copied()
    }

    fn allocate_sdf_slot(&mut self, key: GlyphImageKey) -> Option<(AtlasSlot, bool)> {
        if let Some(slot) = self.get_cached_slot(&key) {
            return Some((slot, false));
        }
        let Some(slot) = self.allocator.allocate_sdf_slot(key.size) else {
            self.atlas.request_reset();
            return None;
        };
        self.cached_slots.insert(key, slot);
        Some((slot, true))
    }

    fn allocate_shared_slot(&mut self, key: GlyphImageKey) -> Option<(AtlasSlot, bool)> {
        if let Some(slot) = self.get_cached_slot(&key) {
            return Some((slot, false));
        }
        let Some(rect) = self.allocator.allocate_shared_slot(key.size) else {
            self.atlas.request_reset();
            return None;
        };
        let slot = AtlasSlot {
            rect,
            plane: AtlasPlane::R,
        };
        self.cached_slots.insert(key, slot);
        Some((slot, true))
    }

    fn seed_msdf_slot_from_sdf(&mut self, dst_slot: AtlasSlot, sdf_glyph: RasterizedGlyph) {
        if sdf_glyph.atlas_kind != AtlasKind::Grayscale {
            return;
        }
        let src_rect = sdf_glyph.atlas_image_bounds;
        if src_rect.size != dst_slot.rect.size {
            return;
        }
        let src_plane = AtlasPlane::from_index(sdf_glyph.atlas_plane as usize);
        let mut src_values = Vec::with_capacity(src_rect.size.width * src_rect.size.height);
        {
            let atlas = self.atlas.image();
            for y in 0..src_rect.size.height {
                for x in 0..src_rect.size.width {
                    let src = atlas[src_rect.origin + Size::new(x, y)];
                    src_values.push(src_plane.get(src));
                }
            }
        }
        let mut dst = self.atlas.get_cached_glyph_image_mut(dst_slot.rect);
        for y in 0..dst_slot.rect.size.height {
            for x in 0..dst_slot.rect.size.width {
                let v = src_values[y * dst_slot.rect.size.width + x];
                dst[Point::new(x, y)] = Bgra::new(v, v, v, v);
            }
        }
    }

    pub fn rasterize_glyph(
        &mut self,
        font: &Font,
        glyph_id: GlyphId,
        dpxs_per_em: f32,
    ) -> Option<RasterizedGlyph> {
        let _phase = makepad_platform::thread::ui_phase(makepad_platform::thread::UiPhase::FontAtlas);
        if let Some(rasterized_glyph) =
            self.rasterize_glyph_raster_image(font, glyph_id, dpxs_per_em)
        {
            return Some(rasterized_glyph);
        }
        if let Some(rasterized_glyph) = self.rasterize_glyph_outline(font, glyph_id, dpxs_per_em) {
            return Some(rasterized_glyph);
        }
        None
    }

    pub fn rasterize_glyph_stable_fallback(
        &mut self,
        font: &Font,
        glyph_id: GlyphId,
        dpxs_per_em: f32,
    ) -> Option<RasterizedGlyph> {
        let _phase = makepad_platform::thread::ui_phase(makepad_platform::thread::UiPhase::FontAtlas);
        if let Some(rasterized_glyph) =
            self.rasterize_glyph_raster_image(font, glyph_id, dpxs_per_em)
        {
            return Some(rasterized_glyph);
        }
        if let Some(rasterized_glyph) =
            self.rasterize_glyph_outline_sdf(font, glyph_id, dpxs_per_em)
        {
            return Some(rasterized_glyph);
        }
        None
    }

    /*
    pub fn save_to_png(item:&Image<R>, path: impl AsRef<Path>) {
        let file = File::create(path).unwrap();
        let writer = BufWriter::new(file);
        let size = item.size();
        let mut encoder = png::Encoder::new(writer, size.width as u32, size.height as u32);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let pixels = item.as_pixels();
        let data = unsafe { slice::from_raw_parts(pixels.as_ptr() as *const u8, pixels.len()) };
        writer.write_image_data(&data).unwrap();
    }
    */
    fn rasterize_glyph_outline(
        &mut self,
        font: &Font,
        glyph_id: GlyphId,
        dpxs_per_em: f32,
    ) -> Option<RasterizedGlyph> {
        match self.outline_rasterization_mode {
            OutlineRasterizationMode::Sdf => {
                self.rasterize_glyph_outline_sdf(font, glyph_id, dpxs_per_em)
            }
            OutlineRasterizationMode::Msdf => {
                self.rasterize_glyph_outline_msdf(font, glyph_id, dpxs_per_em)
            }
        }
    }

    fn rasterize_glyph_outline_sdf(
        &mut self,
        font: &Font,
        glyph_id: GlyphId,
        dpxs_per_em: f32,
    ) -> Option<RasterizedGlyph> {
        let (key, outline, dpxs_per_em) =
            self.get_glyph_outline_sdf_key(font, glyph_id, dpxs_per_em)?;
        let (slot, allocated) = self.allocate_sdf_slot(key.clone())?;
        if allocated {
            self.sdf_glyphs_rasterized += 1;
            let sdf_image = rasterize_outline_sdf(&mut self.sdfer, &outline, dpxs_per_em, key.size);
            self.write_sdf_image(slot, &sdf_image);
        }
        Some(self.build_sdf_glyph(slot, &outline, dpxs_per_em))
    }

    /// Returns the glyph's SDF if the atlas has it, or else queues it for the worker.
    ///
    /// The first time a font misses at some size, we also queue that font's printable ASCII.
    pub fn get_or_queue_glyph_outline_sdf(
        &mut self,
        font: &Rc<Font>,
        glyph_id: GlyphId,
        dpxs_per_em: f32,
    ) -> Option<RasterizedGlyph> {
        let (key, outline, dpxs_per_em) =
            self.get_glyph_outline_sdf_key(font, glyph_id, dpxs_per_em)?;
        if let Some(slot) = self.get_cached_slot(&key) {
            return Some(self.build_sdf_glyph(slot, &outline, dpxs_per_em));
        }
        self.queue_glyph_outline_sdf(key, &outline, dpxs_per_em);
        if self.prewarm_sdf_sizes.insert((font.id(), dpxs_per_em.to_bits())) {
            self.queued_sdf_prewarms.push((font.clone(), dpxs_per_em));
        }
        None
    }

    /// Returns the atlas key of the glyph's SDF, along with its outline and the size we build it at.
    fn get_glyph_outline_sdf_key(
        &self,
        font: &Font,
        glyph_id: GlyphId,
        dpxs_per_em: f32,
    ) -> Option<(GlyphImageKey, Rc<GlyphOutline>, f32)> {
        debug_assert_eq!(
            self.sdfer.settings().padding,
            self.msdfer.settings().padding
        );
        let dpxs_per_em = dpxs_per_em.max(self.msdf_resolution.min_dpxs_per_em);
        let mut outline = None;
        let bounds_in_ems = font.glyph_outline_bounds_in_ems(glyph_id, &mut outline)?;
        let outline = outline.unwrap_or_else(|| font.glyph_outline_rc(glyph_id).unwrap());
        let atlas_image_size = glyph_outline_image_size(bounds_in_ems.size, dpxs_per_em);
        let key = GlyphImageKey {
            font_id: font.id(),
            glyph_id,
            size: atlas_image_size + Size::from(self.sdfer.settings().padding) * 2,
            kind: GlyphImageKind::OutlineSdf,
        };
        Some((key, outline, dpxs_per_em))
    }

    fn queue_glyph_outline_sdf(
        &mut self,
        key: GlyphImageKey,
        outline: &GlyphOutline,
        dpxs_per_em: f32,
    ) {
        if self.cached_slots.contains_key(&key) || !self.outline_sdf_pending.insert(key.clone()) {
            return;
        }
        self.queued_sdf_jobs.push(QueuedOutlineJob {
            key,
            outline: outline.clone(),
            dpxs_per_em,
            epoch: self.atlas_epoch,
        });
    }

    fn write_sdf_image(&mut self, slot: AtlasSlot, sdf_image: &Image<R>) {
        let mut image = self.atlas.get_cached_glyph_image_mut(slot.rect);
        for y in 0..slot.rect.size.height {
            for x in 0..slot.rect.size.width {
                let point = Point::new(x, y);
                image[point] = slot.plane.set(image[point], sdf_image[point].r());
            }
        }
    }

    fn build_sdf_glyph(
        &self,
        slot: AtlasSlot,
        outline: &GlyphOutline,
        dpxs_per_em: f32,
    ) -> RasterizedGlyph {
        RasterizedGlyph {
            atlas_kind: AtlasKind::Grayscale,
            atlas_size: self.atlas.size(),
            atlas_image_bounds: slot.rect,
            atlas_image_padding: self.sdfer.settings().padding,
            atlas_plane: slot.plane.index(),
            origin_in_dpxs: outline.origin_in_ems() * dpxs_per_em,
            dpxs_per_em,
        }
    }

    fn rasterize_glyph_outline_msdf(
        &mut self,
        font: &Font,
        glyph_id: GlyphId,
        dpxs_per_em: f32,
    ) -> Option<RasterizedGlyph> {
        // Always keep small text on SDF, even if an MSDF slot already exists.
        if dpxs_per_em <= self.msdf_resolution.min_request_dpxs_per_em {
            return self.rasterize_glyph_outline_sdf(font, glyph_id, dpxs_per_em);
        }
        let mut outline = None;
        let bounds_in_ems = font.glyph_outline_bounds_in_ems(glyph_id, &mut outline)?;
        let outline = outline.unwrap_or_else(|| font.glyph_outline_rc(glyph_id).unwrap());
        let complexity = outline.complexity();
        if !is_msdf_complexity_acceptable(self.msdf_complexity, complexity) {
            return self.rasterize_glyph_outline_sdf(font, glyph_id, dpxs_per_em);
        }
        let dpxs_per_em = dpxs_per_em.max(self.msdf_resolution.min_dpxs_per_em);
        let atlas_image_size = glyph_outline_image_size(bounds_in_ems.size, dpxs_per_em);
        let atlas_image_padding = self.msdfer.settings().padding;
        let key = GlyphImageKey {
            font_id: font.id(),
            glyph_id,
            size: atlas_image_size + Size::from(atlas_image_padding) * 2,
            kind: GlyphImageKind::OutlineMsdf,
        };
        if self.outline_msdf_failed.contains(&key) {
            return self.rasterize_glyph_outline_sdf(font, glyph_id, dpxs_per_em);
        }
        if self.outline_msdf_ready.contains(&key) {
            if let Some(slot) = self.get_cached_slot(&key) {
                return Some(RasterizedGlyph {
                    atlas_kind: AtlasKind::Msdf,
                    atlas_size: self.atlas.size(),
                    atlas_image_bounds: slot.rect,
                    atlas_image_padding,
                    atlas_plane: AtlasPlane::R.index(),
                    origin_in_dpxs: bounds_in_ems.origin * dpxs_per_em,
                    dpxs_per_em,
                });
            }
            self.outline_msdf_ready.remove(&key);
        }

        let sdf_glyph = self.rasterize_glyph_outline_sdf(font, glyph_id, dpxs_per_em)?;
        if self.outline_msdf_ready.contains(&key) || self.outline_msdf_pending.contains(&key) {
            return Some(sdf_glyph);
        }
        let (slot, allocated) = match self.allocate_shared_slot(key.clone()) {
            Some(slot) => slot,
            None => return Some(sdf_glyph),
        };
        if allocated {
            self.seed_msdf_slot_from_sdf(slot, sdf_glyph);
        }
        if self.outline_msdf_pending.insert(key.clone()) {
            self.msdf_jobs_queued += 1;
            // The job crosses a thread boundary, so it needs an owned copy of the
            // outline. This deep clone happens at most once per glyph per atlas
            // epoch, guarded by the pending set above.
            self.queued_msdf_jobs.push(QueuedOutlineJob {
                key,
                outline: (*outline).clone(),
                dpxs_per_em,
                epoch: self.atlas_epoch,
            });
        }
        Some(sdf_glyph)
    }

    fn rasterize_glyph_raster_image(
        &mut self,
        font: &Font,
        glyph_id: GlyphId,
        dpxs_per_em: f32,
    ) -> Option<RasterizedGlyph> {
        const PADDING: usize = 2;
        // Keep a little extra resolution above the display size so minor magnification
        // (line scaling, fractional DPI) stays crisp without re-introducing aliasing.
        const HEADROOM: f32 = 1.5;

        font.with_glyph_raster_image(glyph_id, dpxs_per_em, |raster_image| {
            let native = raster_image.decode_size();
            let native_dpxs = raster_image.dpxs_per_em();

            // Color emoji ship at one large native strike; pre-shrink toward display size so it
            // isn't minified (no mipmaps) into a blocky look. Never upscale.
            let scale = if native_dpxs > 0.0 {
                (dpxs_per_em * HEADROOM / native_dpxs).clamp(0.0, 1.0)
            } else {
                1.0
            };
            // Quantize the scale into coarse buckets (powers of 1.25, rounded up toward
            // native). The atlas slot and the strike decode below are keyed by the
            // resulting size, so without this a continuous zoom or a fractional-DPI
            // change re-decodes the full strike and allocates a fresh slot for every
            // sub-pixel size step. Rounding up keeps at least the requested resolution,
            // and the achieved-scale compensation below keeps the on-screen size exact.
            const SIZE_BUCKET_FACTOR: f32 = 1.25;
            let scale = if scale > 0.0 && scale < 1.0 {
                let steps = ((1.0 / scale).ln() / SIZE_BUCKET_FACTOR.ln() + 1.0e-3).floor();
                (1.0 / SIZE_BUCKET_FACTOR.powf(steps)).min(1.0)
            } else {
                scale
            };
            let target = if scale < 1.0 {
                Size::new(
                    ((native.width as f32 * scale).round() as usize).max(1),
                    ((native.height as f32 * scale).round() as usize).max(1),
                )
            } else {
                native
            };
            // Scale actually achieved after integer rounding. Scaling both origin_in_dpxs
            // and dpxs_per_em by it keeps the glyph's on-screen size & position invariant
            // (the draw side works in em units = *_in_dpxs / dpxs_per_em).
            //
            // width and height are rounded independently above, so their ratios can differ
            // slightly; we apply one uniform scale (origin is scaled on both axes and
            // dpxs_per_em is a scalar), so use the geometric mean of the per-axis ratios to
            // spread the sub-pixel rounding error evenly instead of distorting one axis.
            let achieved_w = if native.width > 0 {
                target.width as f32 / native.width as f32
            } else {
                1.0
            };
            let achieved_h = if native.height > 0 {
                target.height as f32 / native.height as f32
            } else {
                1.0
            };
            let achieved = (achieved_w * achieved_h).sqrt();

            let key = GlyphImageKey {
                font_id: font.id(),
                glyph_id,
                size: target + Size::from(2 * PADDING),
                kind: GlyphImageKind::Color,
            };
            let (slot, allocated) = self.allocate_shared_slot(key)?;
            let atlas_image_bounds = if !allocated {
                slot.rect
            } else {
                let mut image = self.atlas.get_cached_glyph_image_mut(slot.rect);
                {
                    let size = image.size();
                    // The decode only writes inside the padding, and an atlas reset leaves
                    // old glyphs' pixels behind, so we clear the padding ourselves.
                    for y in (0..PADDING).chain(size.height - PADDING..size.height) {
                        for x in 0..size.width {
                            image[Point::new(x, y)] = Bgra::default();
                        }
                    }
                    for y in PADDING..size.height - PADDING {
                        for x in (0..PADDING).chain(size.width - PADDING..size.width) {
                            image[Point::new(x, y)] = Bgra::default();
                        }
                    }
                    image = image.subimage_mut(Rect::from(size).unpad(PADDING));
                    raster_image.decode_scaled(&mut image);
                }
                slot.rect
            };
            Some(RasterizedGlyph {
                atlas_kind: AtlasKind::Color,
                atlas_size: self.atlas.size(),
                atlas_image_bounds,
                atlas_image_padding: PADDING,
                atlas_plane: AtlasPlane::R.index(),
                origin_in_dpxs: raster_image.origin_in_dpxs() * achieved,
                dpxs_per_em: native_dpxs * achieved,
            })
        })?
    }
}

#[derive(Clone, Copy, Debug)]
struct AtlasSlot {
    rect: Rect<usize>,
    plane: AtlasPlane,
}

#[derive(Clone, Copy, Debug)]
enum AtlasPlane {
    R,
    G,
    B,
    A,
}

impl AtlasPlane {
    fn from_index(index: usize) -> Self {
        match index & 3 {
            0 => Self::R,
            1 => Self::G,
            2 => Self::B,
            _ => Self::A,
        }
    }

    fn index(self) -> u8 {
        match self {
            Self::R => 0,
            Self::G => 1,
            Self::B => 2,
            Self::A => 3,
        }
    }

    fn set(self, pixel: Bgra, value: u8) -> Bgra {
        match self {
            Self::R => Bgra::new(pixel.b(), pixel.g(), value, pixel.a()),
            Self::G => Bgra::new(pixel.b(), value, pixel.r(), pixel.a()),
            Self::B => Bgra::new(value, pixel.g(), pixel.r(), pixel.a()),
            Self::A => Bgra::new(pixel.b(), pixel.g(), pixel.r(), value),
        }
    }

    fn get(self, pixel: Bgra) -> u8 {
        match self {
            Self::R => pixel.r(),
            Self::G => pixel.g(),
            Self::B => pixel.b(),
            Self::A => pixel.a(),
        }
    }
}

/// Skyline packing, one skyline per plane. Each glyph goes as near the top as any plane allows,
/// then as far left, so a frame's new glyphs land next to each other and its upload stays small.
#[derive(Debug)]
struct MultiPlaneAllocator {
    size: Size<usize>,
    skylines: [Skyline; 4],
}

impl MultiPlaneAllocator {
    fn new(size: Size<usize>) -> Self {
        Self {
            size,
            skylines: std::array::from_fn(|_| Skyline::new(size.width)),
        }
    }

    fn reset(&mut self, size: Size<usize>) {
        *self = Self::new(size);
    }

    fn allocate_sdf_slot(&mut self, size: Size<usize>) -> Option<AtlasSlot> {
        if size.width == 0 || size.height == 0 {
            return None;
        }
        let mut best: Option<(Point<usize>, usize)> = None;
        for (plane, skyline) in self.skylines.iter().enumerate() {
            for (index, segment) in skyline.segments.iter().enumerate() {
                if segment.x + size.width > self.size.width {
                    break;
                }
                let origin = Point::new(segment.x, skyline.top(index, segment.x + size.width));
                if origin.y + size.height <= self.size.height
                    && best.is_none_or(|(best_origin, _)| {
                        (origin.y, origin.x) < (best_origin.y, best_origin.x)
                    })
                {
                    best = Some((origin, plane));
                }
            }
        }
        let (origin, plane) = best?;
        let rect = Rect::new(origin, size);
        self.skylines[plane].raise(rect);
        Some(AtlasSlot {
            rect,
            plane: AtlasPlane::from_index(plane),
        })
    }

    fn allocate_shared_slot(&mut self, size: Size<usize>) -> Option<Rect<usize>> {
        if size.width == 0 || size.height == 0 {
            return None;
        }
        // A shared slot covers all four planes, so it has to sit below every plane's skyline.
        let mut lefts: Vec<usize> = self
            .skylines
            .iter()
            .flat_map(|skyline| skyline.segments.iter().map(|segment| segment.x))
            .collect();
        lefts.sort_unstable();
        lefts.dedup();
        let mut best: Option<Point<usize>> = None;
        for x in lefts {
            if x + size.width > self.size.width {
                break;
            }
            let y = self
                .skylines
                .iter()
                .map(|skyline| skyline.top(skyline.segment_at(x), x + size.width))
                .fold(0, usize::max);
            if y + size.height <= self.size.height && best.is_none_or(|best| y < best.y) {
                best = Some(Point::new(x, y));
            }
        }
        let rect = Rect::new(best?, size);
        for skyline in &mut self.skylines {
            skyline.raise(rect);
        }
        Some(rect)
    }
}

#[derive(Debug)]
struct Skyline {
    segments: Vec<SkylineSegment>,
}

/// Columns `x..x + width` are used in rows `0..y`.
#[derive(Clone, Copy, Debug)]
struct SkylineSegment {
    x: usize,
    y: usize,
    width: usize,
}

impl Skyline {
    fn new(width: usize) -> Self {
        Self {
            segments: vec![SkylineSegment { x: 0, y: 0, width }],
        }
    }

    fn segment_at(&self, x: usize) -> usize {
        self.segments.partition_point(|segment| segment.x + segment.width <= x)
    }

    /// The first row free in all of columns `x..end`, where segment `first` holds column `x`.
    fn top(&self, first: usize, end: usize) -> usize {
        self.segments[first..]
            .iter()
            .take_while(|segment| segment.x < end)
            .map(|segment| segment.y)
            .max()
            .unwrap_or(0)
    }

    fn raise(&mut self, used: Rect<usize>) {
        let (x, end, y) = (used.origin.x, used.max().x, used.max().y);
        let first = self.segment_at(x);
        let last = first + self.segments[first..].partition_point(|segment| segment.x < end);
        let head = self.segments[first];
        let tail = self.segments[last - 1];
        let left = (head.x < x).then(|| SkylineSegment {
            width: x - head.x,
            ..head
        });
        let right = (tail.x + tail.width > end).then(|| SkylineSegment {
            x: end,
            y: tail.y,
            width: tail.x + tail.width - end,
        });
        let raised = SkylineSegment {
            x,
            y,
            width: end - x,
        };
        self.segments
            .splice(first..last, left.into_iter().chain([raised]).chain(right));
        // Merge with neighbors at the same height so the segment list stays short.
        let index = first + left.is_some() as usize;
        if right.is_none() && self.segments.get(index + 1).is_some_and(|next| next.y == y) {
            self.segments[index].width += self.segments.remove(index + 1).width;
        }
        if left.is_none() && index > 0 && self.segments[index - 1].y == y {
            self.segments[index - 1].width += self.segments.remove(index).width;
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub sdfer: sdfer::Settings,
    pub msdfer: msdfer::Settings,
    pub msdf_resolution: MsdfResolutionSettings,
    pub msdf_complexity: MsdfComplexitySettings,
    pub outline_rasterization_mode: OutlineRasterizationMode,
    pub atlas_size: Size<usize>,
}

/// A glyph outline for the worker to build an SDF or MSDF image of, depending on its key's kind.
#[derive(Clone, Debug)]
pub struct QueuedOutlineJob {
    pub key: GlyphImageKey,
    pub outline: GlyphOutline,
    pub dpxs_per_em: f32,
    pub epoch: u64,
}

#[derive(Clone, Debug)]
pub struct CompletedMsdfJob {
    pub key: GlyphImageKey,
    pub pixels: Vec<Bgra>,
    pub epoch: u64,
}

#[derive(Clone, Debug)]
pub struct CompletedSdfJob {
    pub key: GlyphImageKey,
    pub sdf_image: Image<R>,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct MsdfResolutionSettings {
    pub min_request_dpxs_per_em: f32,
    pub min_dpxs_per_em: f32,
    pub base_dpxs_per_em: f32,
    pub max_dpxs_per_em: f32,
    pub target_feature_texels: f32,
    pub dpx_quantum: f32,
    pub min_feature_floor_ems: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct MsdfComplexitySettings {
    pub max_outline_commands: usize,
    pub max_estimated_segments: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct RasterizedGlyph {
    pub atlas_kind: AtlasKind,
    pub atlas_size: Size<usize>,
    pub atlas_image_bounds: Rect<usize>,
    pub atlas_image_padding: usize,
    pub atlas_plane: u8,
    pub origin_in_dpxs: Point<f32>,
    pub dpxs_per_em: f32,
}

#[derive(Debug)]
pub(crate) struct PublishedGlyph {
    key: GlyphImageKey,
    glyph: RasterizedGlyph,
    pixels: Vec<Bgra>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtlasKind {
    Grayscale,
    Color,
    Msdf,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OutlineRasterizationMode {
    Sdf,
    Msdf,
}

fn glyph_outline_image_size(size_in_ems: Size<f32>, dpxs_per_em: f32) -> Size<usize> {
    let size_in_dpxs = size_in_ems * dpxs_per_em;
    Size::new(
        size_in_dpxs.width.ceil() as usize,
        size_in_dpxs.height.ceil() as usize,
    )
}

/// Rasterizes an outline's coverage and turns it into an SDF image of `size`, padding included.
pub fn rasterize_outline_sdf(
    sdfer: &mut Sdfer,
    outline: &GlyphOutline,
    dpxs_per_em: f32,
    size: Size<usize>,
) -> Image<R> {
    let coverage_size = size - Size::from(sdfer.settings().padding) * 2;
    let mut coverage = Image::new(coverage_size);
    outline.rasterize(dpxs_per_em, &mut coverage.subimage_mut(coverage_size.into()));
    let mut sdf_image = Image::new(size);
    sdfer.coverage_to_sdf(
        &coverage.subimage(coverage_size.into()),
        &mut sdf_image.subimage_mut(Rect::from(size)),
    );
    sdf_image
}

fn is_msdf_complexity_acceptable(
    settings: MsdfComplexitySettings,
    complexity: OutlineComplexity,
) -> bool {
    complexity.outline_commands <= settings.max_outline_commands
        && complexity.estimated_segments <= settings.max_estimated_segments
}
