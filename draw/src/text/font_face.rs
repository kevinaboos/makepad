use std::{cell::RefCell, fmt, rc::Rc};
use {super::loader::FontData, rustybuzz, rustybuzz::ttf_parser};

pub struct FontFace {
    parsed: Rc<ParsedFontFace>,
    variations: Vec<rustybuzz::Variation>,
    /// Cached `ttf_parser::Face` with the current variations applied.
    /// Invalidated when `set_variations` is called.
    cached_ttf_face: RefCell<Option<ttf_parser::Face<'static>>>,
    /// Cached `rustybuzz::Face` built from the parsed `ttf_parser::Face`.
    /// Invalidated when `set_variations` is called, since variations affect
    /// the rustybuzz shaping tables.
    ///
    /// # Safety
    /// Same lifetime considerations as `ParsedFontFace::face` — the rustybuzz
    /// face borrows from the same stable heap-allocated font data.
    cached_rb_face: RefCell<Option<rustybuzz::Face<'static>>>,
    /// Plans compiled against `cached_rb_face`. A plan bakes in the face's variation
    /// coordinates, so `set_variations` drops these along with the face.
    cached_shape_plans: RefCell<Vec<CachedShapePlan>>,
}

#[cfg(test)]
mod mobile_font_tests {
    use super::*;

    #[test]
    fn android_font_has_shaped_advances_and_outlines() {
        let bytes = include_bytes!("../../../widgets/resources/RobotoFlex.ttf");
        for weight in [400.0, 600.0] {
            let mut face = FontFace::from_data_and_index(FontData::from_vec(bytes.to_vec()), 0).unwrap();
            face.set_variations(&[(u32::from_be_bytes(*b"wght"), weight)]);
            face.with_ttf_parser_face(|f| {
                let id = f.glyph_index('B').unwrap();
                println!("Roboto weight={weight} axes={} coordinates={} glyph={} advance={:?} bounds={:?}", f.variation_axes().len(), f.variation_coordinates().len(), id.0, f.glyph_hor_advance(id), f.glyph_bounding_box(id));
                assert!(f.glyph_hor_advance(id).unwrap() > 0);
                assert!(f.glyph_bounding_box(id).is_some(), "Roboto outline at weight {weight}");
            });
            face.with_rustybuzz_face(|f| {
                let mut buffer = rustybuzz::UnicodeBuffer::new();
                buffer.push_str("Browser 12:34");
                let shaped = rustybuzz::shape(f, &[], buffer);
                assert!(shaped.glyph_positions().iter().map(|p| p.x_advance).sum::<i32>() > 0);
            });
        }
    }
}

#[cfg(test)]
mod shape_plan_tests {
    use super::*;
    use rustybuzz::Direction::{LeftToRight, RightToLeft};

    fn assert_shape_matches_rustybuzz(
        face: &FontFace,
        text: &str,
        direction: rustybuzz::Direction,
        features: &[rustybuzz::Feature],
    ) {
        let build_buffer = || {
            let mut buffer = rustybuzz::UnicodeBuffer::new();
            buffer.set_direction(direction);
            buffer.push_str(text);
            buffer
        };
        // The glyph types don't implement `PartialEq`, so we compare their Debug output.
        let format_glyphs = |glyphs: &rustybuzz::GlyphBuffer| {
            format!("{:?} {:?}", glyphs.glyph_infos(), glyphs.glyph_positions())
        };
        let cached = face.shape(features, build_buffer());
        let expected = face.with_rustybuzz_face(|f| rustybuzz::shape(f, features, build_buffer()));
        assert_eq!(format_glyphs(&cached), format_glyphs(&expected), "{text}");
    }

    #[test]
    fn shape_plan_cache_matches_rustybuzz() {
        let bytes = include_bytes!("../../../widgets/resources/RobotoFlex.ttf");
        let mut face = FontFace::from_data_and_index(FontData::from_vec(bytes.to_vec()), 0).unwrap();
        let no_ligatures = [rustybuzz::Feature::new(ttf_parser::Tag::from_bytes(b"liga"), 0, ..)];
        // Every case has its own plan key. The digits and punctuation leave the script unset.
        let cases: [(&str, rustybuzz::Direction, &[rustybuzz::Feature]); 5] = [
            ("office AVATAR To", LeftToRight, &[]),
            ("office AVATAR To", LeftToRight, &no_ligatures),
            ("12:34 $5.60 (7%)", LeftToRight, &[]),
            ("12:34 $5.60 (7%)", RightToLeft, &[]),
            ("Привет, мир", LeftToRight, &[]),
        ];
        for _ in 0..2 {
            for (text, direction, features) in cases {
                assert_shape_matches_rustybuzz(&face, text, direction, features);
            }
        }
        assert_eq!(face.cached_shape_plans.borrow().len(), cases.len());

        // A heavy weight swaps in a different `$` glyph, so a stale plan would show up here.
        face.set_variations(&[(u32::from_be_bytes(*b"wght"), 1000.0)]);
        assert!(face.cached_shape_plans.borrow().is_empty());
        for (text, direction, features) in cases {
            assert_shape_matches_rustybuzz(&face, text, direction, features);
        }
    }
}

struct ParsedFontFace {
    data: FontData,
    index: u32,
    face: ttf_parser::Face<'static>,
}

struct CachedShapePlan {
    direction: rustybuzz::Direction,
    script: Option<rustybuzz::Script>,
    language: Option<rustybuzz::Language>,
    features: Vec<rustybuzz::Feature>,
    plan: rustybuzz::ShapePlan,
}

impl Clone for FontFace {
    fn clone(&self) -> Self {
        Self {
            parsed: self.parsed.clone(),
            variations: self.variations.clone(),
            cached_ttf_face: RefCell::new(None),
            cached_rb_face: RefCell::new(None),
            cached_shape_plans: RefCell::new(Vec::new()),
        }
    }
}

impl fmt::Debug for ParsedFontFace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedFontFace")
            .field("index", &self.index)
            .field("len", &self.data.len())
            .finish()
    }
}

impl fmt::Debug for FontFace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FontFace")
            .field("parsed", &self.parsed)
            .field("variation_count", &self.variations.len())
            .finish()
    }
}

impl FontFace {
    pub(super) fn worker_source(&self) -> (Vec<u8>, u32, Vec<(u32, f32)>) {
        (
            self.parsed.data.as_slice().to_vec(),
            self.parsed.index,
            self.variations.iter().map(|v| (v.tag.0, v.value)).collect(),
        )
    }

    pub fn from_data_and_index(data: FontData, index: u32) -> Option<Self> {
        let parsed_data = data.clone();
        let face = ttf_parser::Face::parse(parsed_data.as_slice(), index).ok()?;
        let parsed = ParsedFontFace {
            data,
            index,
            // SAFETY: `ttf_parser::Face` only borrows the bytes inside `data`.
            // `ParsedFontFace` owns `data`, which is backed by heap/mmap storage
            // (via `Rc` inside `SharedBytes`) whose address remains stable even if
            // `ParsedFontFace` itself moves. The transmuted face never outlives the
            // owned bytes because both are stored in the same `Rc<ParsedFontFace>`.
            face: unsafe {
                std::mem::transmute::<ttf_parser::Face<'_>, ttf_parser::Face<'static>>(face)
            },
        };
        Some(Self {
            parsed: Rc::new(parsed),
            variations: Vec::new(),
            cached_ttf_face: RefCell::new(None),
            cached_rb_face: RefCell::new(None),
            cached_shape_plans: RefCell::new(Vec::new()),
        })
    }

    pub fn with_ttf_parser_face<R>(&self, f: impl FnOnce(&ttf_parser::Face<'_>) -> R) -> R {
        if self.variations.is_empty() {
            return f(&self.parsed.face);
        }

        {
            let mut ttf_cache = self.cached_ttf_face.borrow_mut();
            if ttf_cache.is_none() {
                let mut face = self.parsed.face.clone();
                for variation in &self.variations {
                    let _ = face.set_variation(variation.tag, variation.value);
                }
                *ttf_cache = Some(face);
            }
        }

        let ttf_cache = self.cached_ttf_face.borrow();
        f(ttf_cache.as_ref().unwrap())
    }

    pub fn with_rustybuzz_face<R>(&self, f: impl FnOnce(&rustybuzz::Face<'_>) -> R) -> R {
        // Populate the rustybuzz cache if empty.
        {
            let mut rb_cache = self.cached_rb_face.borrow_mut();
            if rb_cache.is_none() {
                let mut rb_face = rustybuzz::Face::from_face(self.parsed.face.clone());
                if !self.variations.is_empty() {
                    rb_face.set_variations(&self.variations);
                }
                *rb_cache = Some(rb_face);
            }
        }
        let rb_cache = self.cached_rb_face.borrow();
        f(rb_cache.as_ref().unwrap())
    }

    /// Same output as `rustybuzz::shape`, but compiles each distinct plan only once.
    pub fn shape(
        &self,
        features: &[rustybuzz::Feature],
        mut buffer: rustybuzz::UnicodeBuffer,
    ) -> rustybuzz::GlyphBuffer {
        // This is the guess `rustybuzz::shape` makes before compiling its plan. A guess
        // never stores `UNKNOWN`, so reading that back means the script is unset.
        buffer.guess_segment_properties();
        let direction = buffer.direction();
        let script = Some(buffer.script()).filter(|&script| script != rustybuzz::script::UNKNOWN);
        let language = buffer.language();
        self.with_rustybuzz_face(|face| {
            let mut plans = self.cached_shape_plans.borrow_mut();
            let index = match plans.iter().position(|cached| {
                cached.direction == direction
                    && cached.script == script
                    && cached.language == language
                    && cached.features == features
            }) {
                Some(index) => index,
                None => {
                    let plan = rustybuzz::ShapePlan::new(
                        face,
                        direction,
                        script,
                        language.as_ref(),
                        features,
                    );
                    plans.push(CachedShapePlan {
                        direction,
                        script,
                        language,
                        features: features.to_vec(),
                        plan,
                    });
                    plans.len() - 1
                }
            };
            rustybuzz::shape_with_plan(face, &plans[index].plan, buffer)
        })
    }

    pub fn data(&self) -> &FontData {
        &self.parsed.data
    }

    pub fn set_variations(&mut self, variations: &[(u32, f32)]) {
        self.variations.clear();
        self.variations
            .extend(variations.iter().map(|&(tag, value)| rustybuzz::Variation {
                tag: ttf_parser::Tag::from_bytes(&tag.to_be_bytes()),
                value,
            }));
        *self.cached_ttf_face.borrow_mut() = None;
        // Invalidate the cached rustybuzz face since variations affect shaping.
        *self.cached_rb_face.borrow_mut() = None;
        self.cached_shape_plans.borrow_mut().clear();
    }
}
