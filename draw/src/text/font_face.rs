use {super::loader::FontData, rustybuzz, rustybuzz::ttf_parser};
use std::{fmt, rc::Rc};

#[derive(Clone)]
pub struct FontFace {
    parsed: Rc<ParsedFontFace>,
    variations: Vec<rustybuzz::Variation>,
}

struct ParsedFontFace {
    data: FontData,
    index: u32,
    face: ttf_parser::Face<'static>,
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
    pub fn from_data_and_index(data: FontData, index: u32) -> Option<Self> {
        let parsed_data = data.clone();
        let face = ttf_parser::Face::parse(parsed_data.as_slice(), index).ok()?;
        let parsed = ParsedFontFace {
            data,
            index,
            // SAFETY: `ttf_parser::Face` only borrows the bytes inside `data`.
            // `ParsedFontFace` owns `data`, which is backed by heap/mmap storage
            // whose address remains stable even if `ParsedFontFace` itself moves.
            // The transmuted face never outlives the owned bytes because both are
            // stored in the same `Rc<ParsedFontFace>`.
            face: unsafe {
                std::mem::transmute::<ttf_parser::Face<'_>, ttf_parser::Face<'static>>(face)
            },
        };
        Some(Self {
            parsed: Rc::new(parsed),
            variations: Vec::new(),
        })
    }

    pub fn with_ttf_parser_face<R>(&self, f: impl FnOnce(&ttf_parser::Face<'_>) -> R) -> R {
        f(&self.parsed.face)
    }

    pub fn with_rustybuzz_face<R>(&self, f: impl FnOnce(&rustybuzz::Face<'_>) -> R) -> R {
        let mut rb_face = rustybuzz::Face::from_face(self.parsed.face.clone());
        if !self.variations.is_empty() {
            rb_face.set_variations(&self.variations);
        }
        f(&rb_face)
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
    }
}
