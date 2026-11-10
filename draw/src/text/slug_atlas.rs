use {
    super::{
        font::{Font, FontId, GlyphId},
        geom::{Point, Rect, Size},
        glyph_outline::{Command, GlyphOutline},
    },
    crate::{makepad_platform::*, shader::draw_glyph::band_range},
    fxhash::{FxHashMap, FxHashSet},
};

const CURVE_TEX_WIDTH: usize = 2048;
const BAND_TEX_WIDTH: usize = 2048;
const CUBIC_TO_QUAD_TOLERANCE: f32 = 0.05;
const MAX_CUBIC_SPLIT_DEPTH: usize = 12;
const RGBA_F32_TEXEL_FLOATS: usize = 4;
/// Where the padding quad sits, far above and left of every glyph's unit box.
const PAD_POINT: f32 = -65536.0;
/// Slack around a curve's span when we sort it into bands, as a fraction of the glyph's box.
const BAND_EPSILON: f32 = 1.0 / 1024.0;
/// Soft cap on the append-only `curve_data` buffer (~4 MB of f32). Slug glyphs are only
/// used for very large text, so a realistic on-screen working set is far below this; the
/// generous headroom means a reset (rebuild of all currently-visible glyphs) only happens
/// after a lot of *distinct* large glyphs have scrolled through, and never thrashes.
const MAX_CURVE_DATA_FLOATS: usize = 1 << 20;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SlugGlyphKey {
    font_id: FontId,
    glyph_id: GlyphId,
}

#[derive(Clone, Copy, Debug)]
pub struct SlugGlyphInfo {
    pub origin_in_ems: Point<f32>,
    pub size_in_ems: Size<f32>,
    pub curve_offset: usize,
    pub curve_count: usize,
    pub band_offset: usize,
    pub band_count: usize,
    pub fill_flags: u32,
}

#[derive(Clone, Copy, Debug)]
struct CachedSlugGlyphInfo {
    generation: u64,
    info: SlugGlyphInfo,
}

#[derive(Clone, Copy, Debug)]
pub enum SlugGlyphCacheResult {
    Ready(SlugGlyphInfo),
    NeedsUpload {
        generation: u64,
        glyph: SlugGlyphInfo,
    },
    Deferred,
    Unavailable,
    /// The glyph has no outline, like a space, so there's nothing to draw.
    Empty,
}

pub struct SlugAtlas {
    curve_data: Vec<f32>,
    band_data: Vec<f32>,
    // We keep these between glyphs so building a glyph's bands doesn't allocate.
    band_lists: [Vec<Vec<usize>>; 2],
    curve_reaches: Vec<f32>,
    curve_texture: Texture,
    band_texture: Texture,
    curve_dirty: bool,
    band_dirty: bool,
    curve_uploaded_floats: usize,
    band_uploaded_floats: usize,
    cache_generation: u64,
    uploaded_generation: u64,
    pub(super) glyphs_built: u64,
    pub(super) upload_bytes: u64,
    cached_glyphs: FxHashMap<SlugGlyphKey, CachedSlugGlyphInfo>,
    missing_glyphs: FxHashSet<SlugGlyphKey>,
    empty_glyphs: FxHashSet<SlugGlyphKey>,
    needs_reset: bool,
}

impl SlugAtlas {
    pub fn new(cx: &mut Cx) -> Self {
        let atlas = Self {
            curve_data: Vec::new(),
            band_data: Vec::new(),
            band_lists: [Vec::new(), Vec::new()],
            curve_reaches: Vec::new(),
            curve_texture: Texture::new_with_format(
                cx,
                TextureFormat::VecRGBAf32 {
                    width: 1,
                    height: 1,
                    data: None,
                    updated: TextureUpdated::Empty,
                },
            ),
            band_texture: Texture::new_with_format(
                cx,
                TextureFormat::VecRGBAf32 {
                    width: 1,
                    height: 1,
                    data: None,
                    updated: TextureUpdated::Empty,
                },
            ),
            curve_dirty: false,
            band_dirty: false,
            curve_uploaded_floats: 0,
            band_uploaded_floats: 0,
            cache_generation: 0,
            uploaded_generation: 0,
            glyphs_built: 0,
            upload_bytes: 0,
            cached_glyphs: FxHashMap::default(),
            missing_glyphs: FxHashSet::default(),
            empty_glyphs: FxHashSet::default(),
            needs_reset: false,
        };
        atlas.curve_texture.mark_as_append_only(cx);
        atlas.band_texture.mark_as_append_only(cx);
        atlas
    }

    pub fn needs_reset(&self) -> bool {
        self.needs_reset
    }

    /// Clears every cached glyph if the curve buffer passed its cap, so they rebuild from scratch.
    ///
    /// `Fonts::prepare_textures` calls this after a frame's upload, and the next frame redraws all.
    pub fn reset_if_needed(&mut self) {
        if !self.needs_reset {
            return;
        }
        self.needs_reset = false;
        self.curve_data.clear();
        self.band_data.clear();
        self.cached_glyphs.clear();
        self.missing_glyphs.clear();
        self.empty_glyphs.clear();
        self.curve_uploaded_floats = 0;
        self.band_uploaded_floats = 0;
        self.curve_dirty = true;
        self.band_dirty = true;
        self.cache_generation = self.cache_generation.wrapping_add(1);
    }

    pub fn curve_texture(&self) -> &Texture {
        &self.curve_texture
    }

    pub fn band_texture(&self) -> &Texture {
        &self.band_texture
    }

    pub fn cache_generation(&self) -> u64 {
        self.cache_generation
    }

    pub fn uploaded_generation(&self) -> u64 {
        self.uploaded_generation
    }

    pub fn get_or_cache_glyph(
        &mut self,
        font: &Font,
        glyph_id: GlyphId,
        can_build: bool,
    ) -> SlugGlyphCacheResult {
        let key = SlugGlyphKey {
            font_id: font.id(),
            glyph_id,
        };
        if let Some(cached) = self.cached_glyphs.get(&key).copied() {
            if cached.generation <= self.uploaded_generation {
                return SlugGlyphCacheResult::Ready(cached.info);
            }
            return SlugGlyphCacheResult::NeedsUpload {
                generation: cached.generation,
                glyph: cached.info,
            };
        }
        if self.empty_glyphs.contains(&key) {
            return SlugGlyphCacheResult::Empty;
        }
        if self.missing_glyphs.contains(&key) {
            return SlugGlyphCacheResult::Unavailable;
        }
        if !can_build {
            return SlugGlyphCacheResult::Deferred;
        }

        let Some(outline) = font.glyph_outline(glyph_id) else {
            self.empty_glyphs.insert(key);
            return SlugGlyphCacheResult::Empty;
        };
        let Some(info) = self.build_glyph(font, &outline) else {
            self.missing_glyphs.insert(key);
            return SlugGlyphCacheResult::Unavailable;
        };
        self.glyphs_built += 1;
        let generation = self.cache_generation;
        self.cached_glyphs
            .insert(key, CachedSlugGlyphInfo { generation, info });
        SlugGlyphCacheResult::NeedsUpload {
            generation,
            glyph: info,
        }
    }

    pub fn prepare_textures(&mut self, cx: &mut Cx) {
        let new_floats = self.curve_data.len().saturating_sub(self.curve_uploaded_floats)
            + self.band_data.len().saturating_sub(self.band_uploaded_floats);
        self.upload_bytes += (new_floats * std::mem::size_of::<f32>()) as u64;
        let is_dirty = self.curve_dirty || self.band_dirty;

        if self.curve_dirty {
            Self::prepare_append_only_rgba_f32_texture(
                cx,
                &self.curve_texture,
                CURVE_TEX_WIDTH,
                &self.curve_data,
                &mut self.curve_uploaded_floats,
            );
            self.curve_dirty = false;
        }

        if self.band_dirty {
            Self::prepare_append_only_rgba_f32_texture(
                cx,
                &self.band_texture,
                BAND_TEX_WIDTH,
                &self.band_data,
                &mut self.band_uploaded_floats,
            );
            self.band_dirty = false;
        }

        if is_dirty {
            self.uploaded_generation = self.cache_generation;
        }
    }

    fn prepare_append_only_rgba_f32_texture(
        cx: &mut Cx,
        texture: &Texture,
        preferred_width: usize,
        source: &[f32],
        uploaded_floats: &mut usize,
    ) {
        debug_assert_eq!(source.len() % RGBA_F32_TEXEL_FLOATS, 0);

        let new_width = if source.is_empty() {
            1
        } else {
            preferred_width.max(1)
        };
        let new_texels = (source.len() / RGBA_F32_TEXEL_FLOATS).max(1);
        let new_height = new_texels.div_ceil(new_width);
        let old_uploaded_floats = (*uploaded_floats).min(source.len());

        let format = texture.get_format(cx);
        let (width, height, data, updated) = match format {
            TextureFormat::VecRGBAf32 {
                width,
                height,
                data,
                updated,
            } => (width, height, data, updated),
            _ => panic!("expected VecRGBAf32 texture format for SLUG atlas"),
        };

        // Only a WIDTH change requires re-laying-out the whole texture; the atlas is append-only,
        // so a height GROWTH leaves all existing rows in place and only the appended rows are
        // dirty. Backends give this texture spare rows (see `CxTexture::is_append_only`), so they
        // upload just those rows instead of the whole atlas.
        let width_changed = *width != new_width;
        let mut texture_data = data.take().unwrap_or_default();
        let had_texture_data = !texture_data.is_empty();
        let new_capacity = new_width * new_height * RGBA_F32_TEXEL_FLOATS;
        texture_data.resize(new_capacity, 0.0);

        if !had_texture_data || *width != new_width || old_uploaded_floats > source.len() {
            texture_data.fill(0.0);
            if !source.is_empty() {
                texture_data[..source.len()].copy_from_slice(source);
            }
        } else if old_uploaded_floats < source.len() {
            texture_data[old_uploaded_floats..source.len()]
                .copy_from_slice(&source[old_uploaded_floats..]);
        }

        *width = new_width;
        *height = new_height;
        *data = Some(texture_data);
        *updated = if !had_texture_data || width_changed || old_uploaded_floats > source.len() {
            TextureUpdated::Full
        } else {
            updated.update(Self::appended_dirty_rect(
                new_width,
                old_uploaded_floats / RGBA_F32_TEXEL_FLOATS,
                source.len() / RGBA_F32_TEXEL_FLOATS,
            ))
        };
        *uploaded_floats = source.len();
    }

    fn appended_dirty_rect(
        width: usize,
        old_texels: usize,
        new_texels: usize,
    ) -> Option<RectUsize> {
        if width == 0 || new_texels <= old_texels {
            return None;
        }

        let start_row = old_texels / width;
        let start_col = old_texels % width;
        let end_row = (new_texels - 1) / width;

        if start_row == end_row {
            return Some(RectUsize::new(
                PointUsize::new(start_col, start_row),
                SizeUsize::new(new_texels - old_texels, 1),
            ));
        }

        Some(RectUsize::new(
            PointUsize::new(0, start_row),
            SizeUsize::new(width, end_row - start_row + 1),
        ))
    }

    fn build_glyph(&mut self, font: &Font, outline: &GlyphOutline) -> Option<SlugGlyphInfo> {
        let bounds = outline.bounds_in_ems();
        if bounds.size.width <= 0.000001 || bounds.size.height <= 0.000001 {
            return None;
        }

        let curves = outline_to_normalized_quads(outline, bounds, font.units_per_em());
        if curves.is_empty() {
            return None;
        }

        // The text shader reads a glyph's curves from one texture row. We end them with a point
        // quad that no ray ever crosses, which fills out the last group of each band list.
        let texel_count = (curves.len() + 1) * 2;
        // DrawTextSlug on Linux and Windows scans every curve, so it needs no bands or row limit.
        let uses_bands = cfg!(not(any(target_os = "linux", target_os = "windows")));
        if uses_bands && texel_count > CURVE_TEX_WIDTH {
            return None;
        }
        start_new_row_if_needed(&mut self.curve_data, texel_count, CURVE_TEX_WIDTH);
        let curve_offset = self.curve_data.len() / 8;
        for curve in &curves {
            self.curve_data.extend_from_slice(&[
                curve.p0.x, curve.p0.y, curve.p1.x, curve.p1.y, curve.p2.x, curve.p2.y, 0.0, 0.0,
            ]);
        }
        self.curve_data.extend_from_slice(&[
            PAD_POINT, PAD_POINT, PAD_POINT, PAD_POINT, PAD_POINT, PAD_POINT, 0.0, 0.0,
        ]);

        // The glyph's band block sits within one texture row. A band's texel holds where its
        // H and V lists start and how many groups of 4 they have.
        let (band_offset, band_count) = 'bands: {
            if !uses_bands {
                break 'bands (0, 0);
            }
            // List entries are curve texel offsets from the glyph's first curve.
            let pad_entry = (curves.len() * 2) as f32;
            // Fewer bands make a smaller block; one band always fits a glyph that fits in a curve row.
            for band_count in [8, 4, 2, 1] {
                let [h_lists, v_lists] = &mut self.band_lists;
                let reaches = &mut self.curve_reaches;
                build_band_lists(&curves, band_count, |p| p.y, |p| p.x, h_lists, reaches);
                build_band_lists(&curves, band_count, |p| p.x, |p| p.y, v_lists, reaches);
                let (h_lists, v_lists) = (&h_lists[..band_count], &v_lists[..band_count]);
                let list_texels: usize = h_lists
                    .iter()
                    .chain(v_lists)
                    .map(|list| list.len().div_ceil(4))
                    .sum();
                if band_count + list_texels > BAND_TEX_WIDTH {
                    continue;
                }

                start_new_row_if_needed(
                    &mut self.band_data,
                    band_count + list_texels,
                    BAND_TEX_WIDTH,
                );
                let band_offset = self.band_data.len() / RGBA_F32_TEXEL_FLOATS;
                let meta_start = self.band_data.len();
                self.band_data
                    .resize(meta_start + band_count * RGBA_F32_TEXEL_FLOATS, 0.0);
                let mut list_texel = band_count;
                for (axis, lists) in [h_lists, v_lists].into_iter().enumerate() {
                    for (band, list) in lists.iter().enumerate() {
                        let groups = list.len().div_ceil(4);
                        let meta = meta_start + band * RGBA_F32_TEXEL_FLOATS + axis * 2;
                        self.band_data[meta] = list_texel as f32;
                        self.band_data[meta + 1] = groups as f32;
                        for chunk in list.chunks(4) {
                            let mut texel = [pad_entry; 4];
                            for (entry, index) in texel.iter_mut().zip(chunk) {
                                *entry = (index * 2) as f32;
                            }
                            self.band_data.extend_from_slice(&texel);
                        }
                        list_texel += groups;
                    }
                }
                self.band_dirty = true;
                break 'bands (band_offset, band_count);
            }
            (0, 0)
        };
        self.curve_dirty = true;
        self.cache_generation = self.cache_generation.wrapping_add(1);

        // This glyph is still valid for the current frame; request a reset (performed at the
        // next frame boundary) once the append-only buffer grows past its cap so it doesn't
        // accumulate every distinct large glyph ever rendered.
        if self.curve_data.len() > MAX_CURVE_DATA_FLOATS {
            self.needs_reset = true;
        }

        Some(SlugGlyphInfo {
            origin_in_ems: bounds.origin,
            size_in_ems: bounds.size,
            curve_offset,
            curve_count: curves.len(),
            band_offset,
            band_count,
            fill_flags: 0,
        })
    }
}

/// Pads `data` out to the next texture row when the next `texel_count` texels won't fit in this one.
fn start_new_row_if_needed(data: &mut Vec<f32>, texel_count: usize, row_texels: usize) {
    let row_texels_left = row_texels - (data.len() / RGBA_F32_TEXEL_FLOATS) % row_texels;
    if texel_count > row_texels_left {
        data.resize(data.len() + row_texels_left * RGBA_F32_TEXEL_FLOATS, 0.0);
    }
}

/// Lists the curves a ray along `along` can cross in each band of `across`, furthest-reaching
/// first, so the shader can stop once the rest of a list sits behind its sample.
fn build_band_lists(
    curves: &[QuadCurve],
    band_count: usize,
    across: impl Fn(P2) -> f32,
    along: impl Fn(P2) -> f32,
    lists: &mut Vec<Vec<usize>>,
    reaches: &mut Vec<f32>,
) {
    if lists.len() < band_count {
        lists.resize_with(band_count, Vec::new);
    }
    for list in &mut lists[..band_count] {
        list.clear();
    }
    reaches.clear();
    for (index, curve) in curves.iter().enumerate() {
        reaches.push(along(curve.p0).max(along(curve.p1)).max(along(curve.p2)));
        let [a, b, c] = [curve.p0, curve.p1, curve.p2].map(&across);
        let (min, max) = (a.min(b).min(c), a.max(b).max(c));
        // A ray only crosses a curve with points on both sides of it, so flat ones never count.
        if min == max {
            continue;
        }
        if let Some((lo, hi)) = band_range(
            min - BAND_EPSILON,
            max + BAND_EPSILON,
            band_count as f32,
            band_count,
        ) {
            for list in &mut lists[lo..=hi] {
                list.push(index);
            }
        }
    }
    for list in &mut lists[..band_count] {
        list.sort_by(|a, b| reaches[*b].total_cmp(&reaches[*a]));
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct P2 {
    x: f32,
    y: f32,
}

#[derive(Clone, Copy, Debug)]
struct QuadCurve {
    p0: P2,
    p1: P2,
    p2: P2,
}

fn outline_to_normalized_quads(
    outline: &GlyphOutline,
    bounds: Rect<f32>,
    units_per_em: f32,
) -> Vec<QuadCurve> {
    let mut curves = Vec::new();
    let inv_units = 1.0 / units_per_em.max(0.000001);
    let inv_w = 1.0 / bounds.size.width.max(0.000001);
    let inv_h = 1.0 / bounds.size.height.max(0.000001);
    let mut current = None::<P2>;
    let mut contour_start = None::<P2>;

    for command in outline.commands().iter().copied() {
        match command {
            Command::MoveTo(p) => {
                let point = scale_point(p, inv_units);
                current = Some(point);
                contour_start = Some(point);
            }
            Command::LineTo(p) => {
                let Some(p0) = current else {
                    continue;
                };
                let p2 = scale_point(p, inv_units);
                let p1 = midpoint(p0, p2);
                curves.push(QuadCurve {
                    p0: normalize_point(p0, bounds, inv_w, inv_h),
                    p1: normalize_point(p1, bounds, inv_w, inv_h),
                    p2: normalize_point(p2, bounds, inv_w, inv_h),
                });
                current = Some(p2);
            }
            Command::QuadTo(c, p) => {
                let Some(p0) = current else {
                    continue;
                };
                let p1 = scale_point(c, inv_units);
                let p2 = scale_point(p, inv_units);
                curves.push(QuadCurve {
                    p0: normalize_point(p0, bounds, inv_w, inv_h),
                    p1: normalize_point(p1, bounds, inv_w, inv_h),
                    p2: normalize_point(p2, bounds, inv_w, inv_h),
                });
                current = Some(p2);
            }
            Command::CurveTo(c1, c2, p) => {
                let Some(p0) = current else {
                    continue;
                };
                let p1 = scale_point(c1, inv_units);
                let p2 = scale_point(c2, inv_units);
                let p3 = scale_point(p, inv_units);
                cubic_to_quads_recursive(p0, p1, p2, p3, 0, bounds, inv_w, inv_h, &mut curves);
                current = Some(p3);
            }
            Command::Close => {
                if let (Some(p0), Some(ps)) = (current, contour_start) {
                    if !same_point(p0, ps) {
                        let p1 = midpoint(p0, ps);
                        curves.push(QuadCurve {
                            p0: normalize_point(p0, bounds, inv_w, inv_h),
                            p1: normalize_point(p1, bounds, inv_w, inv_h),
                            p2: normalize_point(ps, bounds, inv_w, inv_h),
                        });
                    }
                    current = Some(ps);
                }
            }
        }
    }

    curves
}

/// Cx-free font publication used by the retained code renderer. This shares the
/// editor's outline conversion, including cubic tolerance and contour closure.
pub fn prepare_code_curves(font: &Font, outline: &GlyphOutline) -> Vec<f32> {
    outline_to_normalized_quads(outline, outline.bounds_in_ems(), font.units_per_em())
        .iter()
        .flat_map(|c| [c.p0.x, c.p0.y, c.p1.x, c.p1.y, c.p2.x, c.p2.y, 0.0, 0.0])
        .collect()
}

fn scale_point(point: Point<f32>, inv_units: f32) -> P2 {
    P2 {
        x: point.x * inv_units,
        y: point.y * inv_units,
    }
}

fn normalize_point(point: P2, bounds: Rect<f32>, inv_w: f32, inv_h: f32) -> P2 {
    P2 {
        x: (point.x - bounds.origin.x) * inv_w,
        // Font outlines are Y-up, but DrawGlyph normalized quad space is Y-down.
        y: (bounds.origin.y + bounds.size.height - point.y) * inv_h,
    }
}

fn same_point(a: P2, b: P2) -> bool {
    (a.x - b.x).abs() <= 0.000001 && (a.y - b.y).abs() <= 0.000001
}

fn midpoint(a: P2, b: P2) -> P2 {
    P2 {
        x: (a.x + b.x) * 0.5,
        y: (a.y + b.y) * 0.5,
    }
}

fn eval_quad(p0: P2, p1: P2, p2: P2, t: f32) -> P2 {
    let s = 1.0 - t;
    P2 {
        x: s * s * p0.x + 2.0 * s * t * p1.x + t * t * p2.x,
        y: s * s * p0.y + 2.0 * s * t * p1.y + t * t * p2.y,
    }
}

fn eval_cubic(p0: P2, p1: P2, p2: P2, p3: P2, t: f32) -> P2 {
    let s = 1.0 - t;
    let s2 = s * s;
    let t2 = t * t;
    P2 {
        x: p0.x * s2 * s + 3.0 * p1.x * s2 * t + 3.0 * p2.x * s * t2 + p3.x * t2 * t,
        y: p0.y * s2 * s + 3.0 * p1.y * s2 * t + 3.0 * p2.y * s * t2 + p3.y * t2 * t,
    }
}

fn distance(a: P2, b: P2) -> f32 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}

fn cubic_to_quad_control(p0: P2, p1: P2, p2: P2, p3: P2) -> P2 {
    P2 {
        x: (3.0 * (p1.x + p2.x) - p0.x - p3.x) * 0.25,
        y: (3.0 * (p1.y + p2.y) - p0.y - p3.y) * 0.25,
    }
}

fn cubic_to_quads_recursive(
    p0: P2,
    p1: P2,
    p2: P2,
    p3: P2,
    depth: usize,
    bounds: Rect<f32>,
    inv_w: f32,
    inv_h: f32,
    out: &mut Vec<QuadCurve>,
) {
    let qc = cubic_to_quad_control(p0, p1, p2, p3);
    let q = QuadCurve { p0, p1: qc, p2: p3 };
    let e25 = distance(
        eval_cubic(p0, p1, p2, p3, 0.25),
        eval_quad(q.p0, q.p1, q.p2, 0.25),
    );
    let e50 = distance(
        eval_cubic(p0, p1, p2, p3, 0.50),
        eval_quad(q.p0, q.p1, q.p2, 0.50),
    );
    let e75 = distance(
        eval_cubic(p0, p1, p2, p3, 0.75),
        eval_quad(q.p0, q.p1, q.p2, 0.75),
    );
    let max_err = e25.max(e50).max(e75);

    if max_err <= CUBIC_TO_QUAD_TOLERANCE || depth >= MAX_CUBIC_SPLIT_DEPTH {
        out.push(QuadCurve {
            p0: normalize_point(q.p0, bounds, inv_w, inv_h),
            p1: normalize_point(q.p1, bounds, inv_w, inv_h),
            p2: normalize_point(q.p2, bounds, inv_w, inv_h),
        });
        return;
    }

    let p01 = midpoint(p0, p1);
    let p12 = midpoint(p1, p2);
    let p23 = midpoint(p2, p3);
    let p012 = midpoint(p01, p12);
    let p123 = midpoint(p12, p23);
    let p0123 = midpoint(p012, p123);

    cubic_to_quads_recursive(p0, p01, p012, p0123, depth + 1, bounds, inv_w, inv_h, out);
    cubic_to_quads_recursive(p0123, p123, p23, p3, depth + 1, bounds, inv_w, inv_h, out);
}

#[cfg(test)]
mod tests {
    use super::{SlugAtlas, SlugGlyphCacheResult};
    use crate::{
        makepad_platform::{Cx, SharedBytes},
        text::{
            font::FontId,
            layouter,
            loader::{FontDefinition, Loader},
        },
    };
    use std::path::PathBuf;

    #[derive(Clone, Copy, Debug)]
    struct TestCurve {
        p0: (f32, f32),
        p1: (f32, f32),
        p2: (f32, f32),
    }

    fn calc_root_code(y1: f32, y2: f32, y3: f32) -> u32 {
        let i1 = y1.to_bits() >> 31;
        let i2 = y2.to_bits() >> 30;
        let i3 = y3.to_bits() >> 29;
        let shift = (i1 & 1) | (i2 & 2) | (i3 & 4);
        (11892u32 >> shift) & 257
    }

    fn solve_horiz_poly(p12: [f32; 4], p3: [f32; 2]) -> (f32, f32) {
        let a = [p12[0] - p12[2] * 2.0 + p3[0], p12[1] - p12[3] * 2.0 + p3[1]];
        let b = [p12[0] - p12[2], p12[1] - p12[3]];
        let ra = 1.0 / a[1];
        let rb = 0.5 / b[1];
        let d = (b[1] * b[1] - a[1] * p12[1]).max(0.0).sqrt();
        let (mut t1, mut t2) = ((b[1] - d) * ra, (b[1] + d) * ra);
        if a[1].abs() < 1.0 / 65536.0 {
            t1 = p12[1] * rb;
            t2 = t1;
        }
        (
            (a[0] * t1 - b[0] * 2.0) * t1 + p12[0],
            (a[0] * t2 - b[0] * 2.0) * t2 + p12[0],
        )
    }

    fn solve_vert_poly(p12: [f32; 4], p3: [f32; 2]) -> (f32, f32) {
        let a = [p12[0] - p12[2] * 2.0 + p3[0], p12[1] - p12[3] * 2.0 + p3[1]];
        let b = [p12[0] - p12[2], p12[1] - p12[3]];
        let ra = 1.0 / a[0];
        let rb = 0.5 / b[0];
        let d = (b[0] * b[0] - a[0] * p12[0]).max(0.0).sqrt();
        let (mut t1, mut t2) = ((b[0] - d) * ra, (b[0] + d) * ra);
        if a[0].abs() < 1.0 / 65536.0 {
            t1 = p12[0] * rb;
            t2 = t1;
        }
        (
            (a[1] * t1 - b[1] * 2.0) * t1 + p12[1],
            (a[1] * t2 - b[1] * 2.0) * t2 + p12[1],
        )
    }

    fn saturate(v: f32) -> f32 {
        v.clamp(0.0, 1.0)
    }

    fn calc_coverage(xcov: f32, ycov: f32, xwgt: f32, ywgt: f32) -> f32 {
        let coverage = ((xcov * xwgt + ycov * ywgt).abs() / (xwgt + ywgt).max(1.0 / 65536.0))
            .max(xcov.abs().min(ycov.abs()));
        saturate(coverage)
    }

    fn alpha_at_full_scan(curves: &[TestCurve], sample: (f32, f32), px_x: f32, px_y: f32) -> f32 {
        let mut coverage_x = 0.0;
        let mut weight_x: f32 = 0.0;
        let mut coverage_y = 0.0;
        let mut weight_y: f32 = 0.0;

        for curve in curves {
            let p12 = [
                curve.p0.0 - sample.0,
                curve.p0.1 - sample.1,
                curve.p1.0 - sample.0,
                curve.p1.1 - sample.1,
            ];
            let p3 = [curve.p2.0 - sample.0, curve.p2.1 - sample.1];

            let h_code = calc_root_code(p12[1], p12[3], p3[1]);
            if h_code != 0 {
                let (r0, r1) = solve_horiz_poly(p12, p3);
                let r0 = r0 / px_x;
                let r1 = r1 / px_x;
                if (h_code & 1) != 0 {
                    coverage_x += saturate(r0 + 0.5);
                    weight_x = weight_x.max(saturate(1.0 - r0.abs() * 2.0));
                }
                if h_code > 1 {
                    coverage_x -= saturate(r1 + 0.5);
                    weight_x = weight_x.max(saturate(1.0 - r1.abs() * 2.0));
                }
            }

            let v_code = calc_root_code(p12[0], p12[2], p3[0]);
            if v_code != 0 {
                let (r0, r1) = solve_vert_poly(p12, p3);
                let r0 = r0 / px_y;
                let r1 = r1 / px_y;
                if (v_code & 1) != 0 {
                    coverage_y -= saturate(r0 + 0.5);
                    weight_y = weight_y.max(saturate(1.0 - r0.abs() * 2.0));
                }
                if v_code > 1 {
                    coverage_y += saturate(r1 + 0.5);
                    weight_y = weight_y.max(saturate(1.0 - r1.abs() * 2.0));
                }
            }
        }

        calc_coverage(coverage_x, coverage_y, weight_x, weight_y)
    }

    fn curves_for_glyph(
        atlas: &SlugAtlas,
        curve_offset: usize,
        curve_count: usize,
    ) -> Vec<TestCurve> {
        let mut curves = Vec::with_capacity(curve_count);
        for i in 0..curve_count {
            let base = (curve_offset + i) * 8;
            curves.push(TestCurve {
                p0: (atlas.curve_data[base], atlas.curve_data[base + 1]),
                p1: (atlas.curve_data[base + 2], atlas.curve_data[base + 3]),
                p2: (atlas.curve_data[base + 4], atlas.curve_data[base + 5]),
            });
        }
        curves
    }

    fn bundled_font_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../widgets/resources/IBMPlexSans-Text.ttf")
    }

    fn load_test_font() -> std::rc::Rc<crate::text::font::Font> {
        let mut loader = Loader::new(layouter::Settings::default().loader);
        let font_id: FontId = 0x5151_0001_u64.into();
        let font_data = SharedBytes::from_file_mmap_or_read(bundled_font_path())
            .expect("font bytes should load");
        loader.define_font(
            font_id,
            FontDefinition {
                data: font_data,
                index: 0,
                ascender_fudge_in_ems: -0.1,
                descender_fudge_in_ems: 0.0,
                weight: None,
                variations: Vec::new(),
            },
        );
        loader.get_or_load_font(font_id).clone()
    }

    #[test]
    fn appended_dirty_rect_stays_tight_within_one_row() {
        let rect = SlugAtlas::appended_dirty_rect(8, 3, 6).expect("expected dirty rect");
        assert_eq!(rect.origin.x, 3);
        assert_eq!(rect.origin.y, 0);
        assert_eq!(rect.size.width, 3);
        assert_eq!(rect.size.height, 1);
    }

    #[test]
    fn appended_dirty_rect_expands_to_full_rows_when_tail_crosses_rows() {
        let rect = SlugAtlas::appended_dirty_rect(8, 6, 11).expect("expected dirty rect");
        assert_eq!(rect.origin.x, 0);
        assert_eq!(rect.origin.y, 0);
        assert_eq!(rect.size.width, 8);
        assert_eq!(rect.size.height, 2);
    }

    #[test]
    fn builds_slug_glyphs_for_uizoo_demo_letters() {
        let font = load_test_font();
        let face = rustybuzz::ttf_parser::Face::parse(font.data().as_slice(), 0)
            .expect("font face should parse");
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut atlas = SlugAtlas::new(&mut cx);

        for ch in ['A', 'g', 'W', 'S', 'L'] {
            let glyph_id = face
                .glyph_index(ch)
                .unwrap_or_else(|| panic!("missing glyph for {ch:?}"))
                .0;
            let result = atlas.get_or_cache_glyph(font.as_ref(), glyph_id, true);
            match result {
                SlugGlyphCacheResult::NeedsUpload { glyph, .. }
                | SlugGlyphCacheResult::Ready(glyph) => {
                    assert!(glyph.curve_count > 0, "expected curves for {ch:?}");
                    assert!(glyph.size_in_ems.width > 0.0, "expected width for {ch:?}");
                    assert!(glyph.size_in_ems.height > 0.0, "expected height for {ch:?}");
                }
                SlugGlyphCacheResult::Deferred => {
                    panic!("unexpected deferred result for {ch:?}");
                }
                SlugGlyphCacheResult::Unavailable | SlugGlyphCacheResult::Empty => {
                    panic!("unexpected unavailable result for {ch:?}");
                }
            }
        }
    }

    #[test]
    fn uizoo_demo_letters_produce_nonzero_slug_coverage() {
        let font = load_test_font();
        let face = rustybuzz::ttf_parser::Face::parse(font.data().as_slice(), 0)
            .expect("font face should parse");
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut atlas = SlugAtlas::new(&mut cx);

        for ch in ['A', 'g', 'W', 'S', 'L'] {
            let glyph_id = face
                .glyph_index(ch)
                .unwrap_or_else(|| panic!("missing glyph for {ch:?}"))
                .0;
            let glyph = match atlas.get_or_cache_glyph(font.as_ref(), glyph_id, true) {
                SlugGlyphCacheResult::NeedsUpload { glyph, .. }
                | SlugGlyphCacheResult::Ready(glyph) => glyph,
                other => panic!("unexpected glyph build result for {ch:?}: {other:?}"),
            };
            let curves = curves_for_glyph(&atlas, glyph.curve_offset, glyph.curve_count);
            let mut max_alpha: f32 = 0.0;
            for y in 0..33 {
                for x in 0..33 {
                    let sample = (x as f32 / 32.0, y as f32 / 32.0);
                    max_alpha = max_alpha.max(alpha_at_full_scan(
                        &curves,
                        sample,
                        1.0 / 192.0,
                        1.0 / 192.0,
                    ));
                }
            }
            assert!(
                max_alpha > 0.2,
                "expected visible SLUG coverage for {ch:?}, got {max_alpha}"
            );
        }
    }
}
