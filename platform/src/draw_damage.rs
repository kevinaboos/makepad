//! `CxDrawDamage`, which works out the pixels of a pass that change from one frame to the next.
//!
//! We walk the pass's draw lists in paint order and compare each draw call with what it drew last frame,
//! so a backend that keeps the rest of the last frame on screen only repaints those rects.

use crate::{
    cx::Cx,
    draw_list::{CxDrawCall, CxDrawItem, CxDrawKind, DrawListId, DrawListUniforms},
    draw_pass::DrawPassId,
    draw_shader::{CxDrawShaderMapping, VertexExtent},
    texture::TextureId,
};

/// A backend paints each damage rect separately, so we keep at most this many.
const MAX_DAMAGE_RECTS: usize = 4;

/// Pixels of a framebuffer, with a top-left origin and exclusive right and bottom edges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DamageRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl DamageRect {
    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    pub fn intersects(&self, other: &DamageRect) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }

    pub fn union(&self, other: &DamageRect) -> DamageRect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        DamageRect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    pub fn area(&self) -> i64 {
        if self.is_empty() {
            return 0;
        }
        (self.x1 - self.x0) as i64 * (self.y1 - self.y0) as i64
    }

    /// The `[x, y, width, height]` that GL and EGL take, measured from the bottom-left corner.
    pub fn to_bottom_left(&self, framebuffer_height: i32) -> [i32; 4] {
        [self.x0, framebuffer_height - self.y1, self.x1 - self.x0, self.y1 - self.y0]
    }
}

/// Adds `rect` to a list of rects that don't overlap, merging rects to keep at most `MAX_DAMAGE_RECTS`.
pub fn add_damage_rect(rects: &mut Vec<DamageRect>, mut rect: DamageRect) {
    if rect.is_empty() {
        return;
    }
    // A backend paints the rects one by one, so an overlap would blend its pixels twice.
    while let Some(index) = rects.iter().position(|other| other.intersects(&rect)) {
        rect = rect.union(&rects.swap_remove(index));
    }
    rects.push(rect);
    if rects.len() <= MAX_DAMAGE_RECTS {
        return;
    }
    let mut cheapest = (0, 1, i64::MAX);
    for a in 0..rects.len() {
        for b in a + 1..rects.len() {
            let wasted_area = rects[a].union(&rects[b]).area() - rects[a].area() - rects[b].area();
            if wasted_area < cheapest.2 {
                cheapest = (a, b, wasted_area);
            }
        }
    }
    let merged = rects[cheapest.0].union(&rects[cheapest.1]);
    rects.swap_remove(cheapest.1);
    rects.swap_remove(cheapest.0);
    add_damage_rect(rects, merged);
}

/// What a draw call drew in the last frame we walked.
#[derive(Default)]
struct DrawCallRecord {
    /// The walk that last reached this draw call, or 0 when nothing of it is on screen.
    walk_id: u64,
    shader_index: Option<usize>,
    key: Vec<u64>,
    uniforms_gen: u64,
    uniforms: Vec<u32>,
    instances: Vec<u32>,
    bound: DamageRect,
}

#[derive(Default)]
struct DrawListRecord {
    generation: u64,
    pass_id: Option<DrawPassId>,
    walk_id: u64,
    uniforms: Vec<f32>,
    draw_calls: Vec<DrawCallRecord>,
}

#[derive(Default)]
struct PassRecord {
    key: Vec<u64>,
    /// Whether the draw call records match what the last frame showed.
    is_valid: bool,
    draw_list_slots: Vec<usize>,
}

/// What each pass showed last frame, so we can tell which of its pixels the next frame changes.
#[derive(Default)]
pub struct CxDrawDamage {
    pub(crate) is_disabled: bool,
    walk_id: u64,
    passes: Vec<PassRecord>,
    draw_lists: Vec<DrawListRecord>,
    /// The pass a backend is repainting part of, its framebuffer height, and the rects it repaints.
    repaint: Option<(DrawPassId, i32, Vec<DamageRect>)>,
}

/// How a pass's camera puts logical coordinates on its framebuffer: `pixel = x * scale + offset`.
#[derive(Clone, Copy)]
struct PixelMapping {
    scale: (f32, f32),
    offset: (f32, f32),
    framebuffer: DamageRect,
}

/// Where a draw list's instances end up before the pass's camera maps them to pixels.
struct ListSpace {
    view_shift: (f32, f32),
    view_clip: (f32, f32, f32, f32),
    /// We only map rects through a view transform that just moves things.
    translation: Option<(f32, f32)>,
}

struct DamageWalk<'a, F> {
    cx: &'a Cx,
    pass_id: DrawPassId,
    shader_draws: &'a F,
    pixels: PixelMapping,
    painted_texture_ids: Vec<TextureId>,
    rects: Vec<DamageRect>,
    is_full: bool,
    /// The backend couldn't tell whether it'll draw a shader, so we can't trust this walk's records.
    is_uncertain: bool,
    paint_index: u64,
    key: Vec<u64>,
    draw_list_slots: Vec<usize>,
}

impl<'a, F> DamageWalk<'a, F> {
    fn add(&mut self, rect: DamageRect) {
        if rect == self.pixels.framebuffer {
            self.is_full = true;
        } else if !self.is_full {
            add_damage_rect(&mut self.rects, rect);
        }
    }

    fn get_instance_bound(
        &self,
        instance: &[u32],
        mapping: &CxDrawShaderMapping,
        space: &ListSpace,
    ) -> DamageRect {
        let full = self.pixels.framebuffer;
        let (Some(pos), Some(size), Some((tx, ty))) = (mapping.rect_pos, mapping.rect_size, space.translation)
        else {
            return full;
        };
        let value = |index: usize| f32::from_bits(instance[index]);
        let (x, y, w, h) = (value(pos), value(pos + 1), value(size), value(size + 1));
        if ![x, y, w, h].iter().all(|v| v.is_finite()) {
            return full;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (x.min(x + w), y.min(y + h), x.max(x + w), y.max(y + h));
        if let Some(clip) = mapping.draw_clip {
            x0 = x0.max(value(clip));
            y0 = y0.max(value(clip + 1));
            x1 = x1.min(value(clip + 2));
            y1 = y1.min(value(clip + 3));
        }
        // The same clamps the stock vertex functions do, then the list's translation.
        let x0 = (x0 + space.view_shift.0).max(space.view_clip.0) + tx;
        let y0 = (y0 + space.view_shift.1).max(space.view_clip.1) + ty;
        let x1 = (x1 + space.view_shift.0).min(space.view_clip.2) + tx;
        let y1 = (y1 + space.view_shift.1).min(space.view_clip.3) + ty;
        if [x0, y0, x1, y1].iter().any(|v| v.is_nan()) {
            return full;
        }
        if x0 >= x1 || y0 >= y1 {
            return DamageRect::default();
        }
        let pixels = &self.pixels;
        let (left, right) = (x0 * pixels.scale.0 + pixels.offset.0, x1 * pixels.scale.0 + pixels.offset.0);
        let (top, bottom) = (y0 * pixels.scale.1 + pixels.offset.1, y1 * pixels.scale.1 + pixels.offset.1);
        // One pixel of margin covers the rasterizer snapping a vertex onto the next pixel.
        DamageRect {
            x0: (left.min(right).floor() as i32).saturating_sub(1).max(full.x0),
            y0: (top.min(bottom).floor() as i32).saturating_sub(1).max(full.y0),
            x1: (left.max(right).ceil() as i32).saturating_add(1).min(full.x1),
            y1: (top.max(bottom).ceil() as i32).saturating_add(1).min(full.y1),
        }
    }
}

// Only Android's OpenGL backend repaints part of a frame so far.
#[cfg_attr(not(all(target_os = "android", not(use_vulkan))), allow(dead_code))]
impl CxDrawDamage {
    /// Turning partial repaints on or off forgets every frame, so the next one repaints in full.
    pub(crate) fn set_disabled(&mut self, is_disabled: bool) {
        if self.is_disabled != is_disabled {
            *self = Self { is_disabled, ..Default::default() };
        }
    }

    /// Repaints of `pass_id` cover only `rects` until `end_repaint`, so a draw call can skip the rects it misses.
    pub(crate) fn start_repaint(&mut self, pass_id: DrawPassId, framebuffer_height: i32, rects: Vec<DamageRect>) {
        self.repaint = Some((pass_id, framebuffer_height, rects));
    }

    pub(crate) fn end_repaint(&mut self) {
        self.repaint = None;
    }

    /// One bit for each rect being repainted that a draw call overlaps, or `None` when we repaint the whole pass.
    pub(crate) fn get_repaint_mask(&self, pass_id: DrawPassId, list_id: DrawListId, item_id: usize) -> Option<u32> {
        let (repaint_pass_id, _, rects) = self.repaint.as_ref()?;
        if *repaint_pass_id != pass_id {
            return None;
        }
        let bound = self
            .draw_lists
            .get(list_id.index())
            .filter(|list| list.generation == list_id.generation() && list.walk_id == self.walk_id)
            .and_then(|list| list.draw_calls.get(item_id))
            .filter(|call| call.walk_id == self.walk_id)
            .map(|call| call.bound);
        Some(rects.iter().enumerate().fold(0, |mask, (index, rect)| {
            if bound.is_none_or(|bound| bound.intersects(rect)) {
                mask | 1 << index
            } else {
                mask
            }
        }))
    }

    /// The scissor boxes of the repainted rects in `mask`, or a single `None` when we repaint everything.
    pub(crate) fn get_scissors(&self, mask: Option<u32>) -> impl Iterator<Item = Option<[i32; 4]>> + '_ {
        let (height, rects) = match &self.repaint {
            Some((_, height, rects)) if mask.is_some() => (*height, &rects[..]),
            _ => (0, &[][..]),
        };
        mask.is_none().then_some(None).into_iter().chain(
            rects
                .iter()
                .enumerate()
                .filter(move |(index, _)| mask.is_some_and(|mask| mask & 1 << index != 0))
                .map(move |(_, rect)| Some(rect.to_bottom_left(height))),
        )
    }

    /// The next walk of `pass_id` reports all of it as damaged, since something changed that we didn't see.
    pub(crate) fn invalidate_pass(&mut self, pass_id: DrawPassId) {
        if let Some(pass) = self.passes.get_mut(pass_id.0) {
            pass.is_valid = false;
        }
    }

    fn walk_pass<F: Fn(&Cx, usize) -> Option<bool>>(
        &mut self,
        cx: &Cx,
        pass_id: DrawPassId,
        framebuffer: (i32, i32),
        painted_pass_ids: &[DrawPassId],
        shader_draws: &F,
    ) -> Option<Vec<DamageRect>> {
        self.walk_id += 1;
        let pass = &cx.passes[pass_id];
        let (Some(main_list_id), Some(dpi_factor)) = (pass.main_draw_list_id, pass.dpi_factor) else {
            self.invalidate_pass(pass_id);
            return None;
        };
        let Some(pass_rect) = cx.get_pass_rect(pass_id, dpi_factor) else {
            self.invalidate_pass(pass_id);
            return None;
        };
        // What the pass's camera shows, the way `CxDrawPass::set_ortho_matrix` sets it up.
        let origin = pass_rect.pos + pass.view_shift;
        let size = pass_rect.size * pass.view_scale;
        let scale = (framebuffer.0 as f64 / size.x, framebuffer.1 as f64 / size.y);
        let pixels = PixelMapping {
            scale: (scale.0 as f32, scale.1 as f32),
            offset: ((-origin.x * scale.0) as f32, (-origin.y * scale.1) as f32),
            framebuffer: DamageRect { x0: 0, y0: 0, x1: framebuffer.0, y1: framebuffer.1 },
        };
        let clear = pass.clear_color;
        let mut key: Vec<u64> = [origin.x, origin.y, size.x, size.y, dpi_factor, pass.display_dpi_factor.unwrap_or(0.0)]
            .iter()
            .map(|v| v.to_bits())
            .collect();
        key.extend(
            [clear.x, clear.y, clear.z, clear.w, pass.zbias_step, pass.pass_uniforms.shiny_time].map(|v| v.to_bits() as u64),
        );
        key.extend([
            framebuffer.0 as u64,
            framebuffer.1 as u64,
            main_list_id.index() as u64,
            main_list_id.generation(),
        ]);
        // Turning one of these off changes the key too, so that frame also repaints in full.
        key.push(pass.dont_clear as u64 | (pass.keep_camera_matrix as u64) << 1 | (pass.sploded.is_some() as u64) << 2);

        if self.passes.len() <= pass_id.0 {
            self.passes.resize_with(pass_id.0 + 1, Default::default);
        }
        let record = &mut self.passes[pass_id.0];
        let is_pass_changed = record.key != key;
        let is_full = !record.is_valid
            || is_pass_changed
            || !scale.0.is_finite()
            || !scale.1.is_finite()
            || pass.dont_clear
            || pass.keep_camera_matrix
            || pass.sploded.is_some();
        record.key = key;
        let old_draw_list_slots = std::mem::take(&mut record.draw_list_slots);
        // The pass may map instances to other pixels now, so we drop every draw call's record of where it drew.
        if is_pass_changed {
            for &slot in &old_draw_list_slots {
                let list = &mut self.draw_lists[slot];
                if list.pass_id == Some(pass_id) {
                    list.draw_calls.clear();
                }
            }
        }

        // Textures that other passes paint this frame, so whatever samples them changes too.
        let painted_texture_ids = painted_pass_ids
            .iter()
            .filter(|painted_pass_id| **painted_pass_id != pass_id)
            .flat_map(|painted_pass_id| {
                let painted_pass = &cx.passes[*painted_pass_id];
                painted_pass
                    .color_textures
                    .iter()
                    .map(|color| color.texture.texture_id())
                    .chain(painted_pass.depth_texture.as_ref().map(|depth| depth.texture_id()))
            })
            .collect();
        let mut walk = DamageWalk {
            cx,
            pass_id,
            shader_draws,
            pixels,
            painted_texture_ids,
            rects: Vec::new(),
            is_full,
            is_uncertain: false,
            paint_index: 0,
            key: Vec::new(),
            draw_list_slots: Vec::new(),
        };
        let mut zbias = 0.0;
        self.walk_draw_list(&mut walk, main_list_id, &mut zbias, pass.zbias_step);

        // A list the pass doesn't reach anymore leaves nothing of itself on screen.
        for slot in old_draw_list_slots {
            let list = &mut self.draw_lists[slot];
            if list.walk_id == self.walk_id || list.pass_id != Some(pass_id) {
                continue;
            }
            for call in list.draw_calls.drain(..) {
                if call.walk_id != 0 {
                    walk.add(call.bound);
                }
            }
            list.pass_id = None;
        }
        let record = &mut self.passes[pass_id.0];
        record.draw_list_slots = std::mem::take(&mut walk.draw_list_slots);
        record.is_valid = !walk.is_uncertain;
        (!walk.is_full && !walk.is_uncertain).then_some(walk.rects)
    }

    fn walk_draw_list<F: Fn(&Cx, usize) -> Option<bool>>(
        &mut self,
        walk: &mut DamageWalk<F>,
        list_id: DrawListId,
        zbias: &mut f32,
        zbias_step: f32,
    ) {
        let cx = walk.cx;
        let slot = list_id.index();
        if self.draw_lists.len() <= slot {
            self.draw_lists.resize_with(slot + 1, Default::default);
        }
        let record = &mut self.draw_lists[slot];
        if record.generation != list_id.generation() || record.pass_id != Some(walk.pass_id) {
            match record.pass_id {
                // The other pass can't account for what this list drew there anymore.
                Some(other_pass_id) if other_pass_id != walk.pass_id => {
                    if let Some(other_pass) = self.passes.get_mut(other_pass_id.0) {
                        other_pass.is_valid = false;
                    }
                }
                _ => {
                    for call in &record.draw_calls {
                        if call.walk_id != 0 {
                            walk.add(call.bound);
                        }
                    }
                }
            }
            let record = &mut self.draw_lists[slot];
            record.draw_calls.clear();
            record.uniforms.clear();
            record.generation = list_id.generation();
            record.pass_id = Some(walk.pass_id);
        }
        let record = &mut self.draw_lists[slot];
        // A list painted twice in one pass doesn't fit in one record, so we give up on this frame.
        if record.walk_id == self.walk_id {
            walk.is_full = true;
            walk.is_uncertain = true;
        }
        record.walk_id = self.walk_id;
        walk.draw_list_slots.push(slot);

        let list = &cx.draw_lists[list_id];
        let uniforms = list.draw_list_uniforms.as_slice();
        let is_list_changed = record.uniforms[..] != uniforms[..];
        if is_list_changed {
            record.uniforms.clear();
            record.uniforms.extend_from_slice(uniforms);
        }
        let DrawListUniforms { view_transform, view_shift, view_clip, .. } = &list.draw_list_uniforms;
        let m = &view_transform.v;
        let is_translation = m[0] == 1.0 && m[5] == 1.0 && m[15] == 1.0
            && [1, 3, 4, 7, 8, 9, 11].iter().all(|&index| m[index] == 0.0);
        let space = ListSpace {
            view_shift: (view_shift.x, view_shift.y),
            view_clip: (view_clip.x, view_clip.y, view_clip.z, view_clip.w),
            translation: is_translation.then_some((m[12], m[13])),
        };

        for order_index in 0..list.draw_item_order_len() {
            let Some(item_id) = list.draw_item_id_at_order_index(order_index) else {
                continue;
            };
            let item = &list.draw_items[item_id];
            match &item.kind {
                CxDrawKind::SubList(sub_list_id) => {
                    if cx.draw_lists.is_id_freed(*sub_list_id) {
                        continue;
                    }
                    // The same depth counter every backend's render_view keeps.
                    let sub_list = &cx.draw_lists[*sub_list_id];
                    let mut own_zbias = 0.0f32;
                    let child_zbias = if sub_list.reset_zbias {
                        &mut own_zbias
                    } else {
                        &mut *zbias
                    };
                    sub_list.raise_zbias_to_floor(child_zbias);
                    if let Some(steps) = sub_list.zbias_hold {
                        let mut held = *child_zbias;
                        self.walk_draw_list(walk, *sub_list_id, &mut held, 0.0);
                        *child_zbias += steps as f32 * zbias_step;
                    } else {
                        self.walk_draw_list(walk, *sub_list_id, child_zbias, zbias_step);
                    }
                }
                CxDrawKind::DrawCall(call) => {
                    self.walk_draw_call(walk, slot, item_id, item, call, &space, is_list_changed, zbias, zbias_step);
                }
                CxDrawKind::Empty => {}
            }
        }

        // What a draw call drew last frame is gone once it's out of this list's paint order.
        let walk_id = self.walk_id;
        for call in &mut self.draw_lists[slot].draw_calls {
            if call.walk_id != 0 && call.walk_id != walk_id {
                walk.add(call.bound);
                call.walk_id = 0;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_draw_call<F: Fn(&Cx, usize) -> Option<bool>>(
        &mut self,
        walk: &mut DamageWalk<F>,
        slot: usize,
        item_id: usize,
        item: &CxDrawItem,
        call: &CxDrawCall,
        space: &ListSpace,
        is_list_changed: bool,
        zbias: &mut f32,
        zbias_step: f32,
    ) {
        let cx = walk.cx;
        let shader_index = call.draw_shader_id.index;
        let sh = &cx.draw_shaders.shaders[shader_index];
        let will_draw = sh.os_shader_id.is_some_and(|os_shader_id| {
            let will_draw = (walk.shader_draws)(cx, os_shader_id);
            walk.is_uncertain |= will_draw.is_none();
            will_draw == Some(true)
        });
        let item_zbias = *zbias;
        if will_draw {
            *zbias += zbias_step;
        }
        // Retained and shared instances can show without being recorded, which we don't follow.
        if item.retained_instances.is_some() || item.shared.is_some() {
            walk.is_full = true;
        }
        let mapping = &sh.mapping;
        let slots = mapping.instances.total_slots;
        let instances = item.instances.as_deref().unwrap_or(&[]);
        let count = if slots == 0 { 0 } else { instances.len() / slots };
        let geometry_id = call.geometry_id.filter(|id| {
            cx.geometries.0.pool.get(id.slot_index()).is_some_and(|geometry| geometry.generation == id.generation())
        });
        let is_drawn = will_draw && count != 0 && geometry_id.is_some();

        let key = &mut walk.key;
        key.clear();
        key.extend([is_drawn as u64, walk.paint_index]);
        walk.paint_index += 1;
        let mut is_volatile = false;
        if let (true, Some(geometry_id)) = (is_drawn, geometry_id) {
            let options = &call.options;
            key.extend([
                item_zbias.to_bits() as u64,
                shader_index as u64,
                sh.os_shader_id.unwrap_or(0) as u64,
                mapping.scope_uniforms_gen,
                geometry_id.slot_index() as u64,
                geometry_id.generation(),
                cx.geometries[geometry_id].data_generation,
                options.alpha_blend as u64 | (options.depth_write as u64) << 1 | (options.backface_culling as u64) << 2,
            ]);
            for texture in &call.texture_slots[..mapping.textures.len().min(call.texture_slots.len())] {
                let Some(texture) = texture else {
                    key.push(u64::MAX);
                    continue;
                };
                let id = texture.texture_id();
                let cxtexture = &cx.textures[id];
                key.extend([id.0 as u64, cx.textures.0.pool[id.0].generation]);
                if cxtexture.format.is_vec() {
                    // A pending update goes up to the GPU while this frame paints.
                    let is_change_pending = cxtexture.can_change_drawn_texels(cxtexture.updated());
                    key.push(cxtexture.texel_generation + is_change_pending as u64);
                } else if cxtexture.format.is_render()
                    && cxtexture.producer_serial != 0
                    && !walk.painted_texture_ids.contains(&id)
                {
                    key.extend([cxtexture.producer_serial, cxtexture.allocation_generation]);
                } else {
                    // Video frames, shared and depth textures change without telling us.
                    is_volatile = true;
                }
            }
            for buffer in &call.uniform_buffer_slots[..mapping.uniform_buffers.len().min(call.uniform_buffer_slots.len())] {
                let Some(buffer) = buffer else {
                    key.push(u64::MAX);
                    continue;
                };
                let id = buffer.uniform_buffer_id();
                key.extend([id.0 as u64, cx.uniform_buffers.0.pool[id.0].generation, cx.uniform_buffers[id].generation]);
            }
            is_volatile |= mapping.uses_time;
        }
        let extent = if call.geometry_id == mapping.geometry_id {
            mapping.vertex_extent
        } else {
            VertexExtent::Unknown
        };
        // SAFETY: f32 and u32 have the same size and alignment, and every bit pattern is a valid u32.
        let instances: &[u32] = if is_drawn {
            unsafe { std::slice::from_raw_parts(instances.as_ptr().cast(), count * slots) }
        } else {
            &[]
        };

        let calls = &mut self.draw_lists[slot].draw_calls;
        if calls.len() <= item_id {
            calls.resize_with(item_id + 1, Default::default);
        }
        let record = &mut calls[item_id];
        let was_drawn = record.walk_id != 0;
        let is_same_shader = record.shader_index == Some(shader_index);
        // Every write to a draw call's uniforms gives it a new generation, so we only read them after one.
        let mut are_uniforms_changed = false;
        if record.uniforms_gen != call.uniforms_gen {
            record.uniforms_gen = call.uniforms_gen;
            let uniforms = &call.dyn_uniforms[..mapping.dyn_uniforms.total_slots.min(call.dyn_uniforms.len())];
            if !record.uniforms.iter().copied().eq(uniforms.iter().map(|v| v.to_bits())) {
                are_uniforms_changed = true;
                record.uniforms.clear();
                record.uniforms.extend(uniforms.iter().map(|v| v.to_bits()));
            }
        }
        let is_key_changed =
            !was_drawn || is_list_changed || is_volatile || are_uniforms_changed || record.key != walk.key;
        if is_key_changed
            || record.instances.len() != instances.len()
            || (call.instance_dirty && record.instances[..] != instances[..])
        {
            // Old instances only map to where they were drawn if the list and shader are the same.
            let can_diff_instances = was_drawn && !is_list_changed && is_same_shader;
            if was_drawn && !can_diff_instances {
                walk.add(record.bound);
            }
            let mut bound = DamageRect::default();
            if extent == VertexExtent::Rect || (instances.is_empty() && record.instances.is_empty()) {
                let old_count = if can_diff_instances && slots != 0 { record.instances.len() / slots } else { 0 };
                for index in 0..count.max(old_count) {
                    let range = index * slots..(index + 1) * slots;
                    let new = instances.get(range.clone());
                    let old = record.instances.get(range).filter(|_| can_diff_instances);
                    let new_bound = new.map(|new| walk.get_instance_bound(new, mapping, space));
                    if is_key_changed || new != old {
                        if let Some(old) = old {
                            walk.add(walk.get_instance_bound(old, mapping, space));
                        }
                        if let Some(new_bound) = new_bound {
                            walk.add(new_bound);
                        }
                    }
                    bound = bound.union(&new_bound.unwrap_or_default());
                }
            } else {
                // We can't tell where this shader draws, so all of the pass might have changed.
                if !instances.is_empty() {
                    bound = walk.pixels.framebuffer;
                }
                walk.add(walk.pixels.framebuffer);
            }
            record.shader_index = Some(shader_index);
            record.key.clone_from(&walk.key);
            record.instances.clear();
            record.instances.extend_from_slice(instances);
            record.bound = bound;
        }
        record.walk_id = self.walk_id;
    }
}

#[cfg_attr(not(all(target_os = "android", not(use_vulkan))), allow(dead_code))]
impl Cx {
    /// The rects of the pass's `framebuffer` that change once it's painted, or `None` when any of them might.
    /// `shader_draws` says if the backend draws a shader and steps the depth counter for it, `None` if it can't tell.
    pub(crate) fn compute_pass_damage(
        &mut self,
        pass_id: DrawPassId,
        framebuffer: (i32, i32),
        painted_pass_ids: &[DrawPassId],
        shader_draws: impl Fn(&Cx, usize) -> Option<bool>,
    ) -> Option<Vec<DamageRect>> {
        let mut damage = std::mem::take(&mut self.draw_damage);
        let rects = damage.walk_pass(self, pass_id, framebuffer, painted_pass_ids, &shader_draws);
        self.draw_damage = damage;
        rects
    }
}
