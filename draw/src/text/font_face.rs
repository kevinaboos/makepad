use {
    super::loader::FontData,
    rustybuzz,
    rustybuzz::ttf_parser,
    std::cell::RefCell,
    std::fmt,
};

pub struct FontFace {
    data: FontData,
    index: u32,
    variations: Vec<rustybuzz::Variation>,
    /// Cached parsed `ttf_parser::Face` to avoid re-parsing TTF tables on every access.
    ///
    /// # Safety
    /// The `Face` borrows from `self.data`, which is heap-allocated via `Rc`
    /// inside `SharedBytes` and is never mutated after construction. The borrow
    /// therefore remains valid for the lifetime of this `FontFace`. We erase
    /// the lifetime to `'static` so the parsed face can be stored alongside
    /// the data it borrows from.
    cached_ttf_face: RefCell<Option<ttf_parser::Face<'static>>>,
    /// Cached `rustybuzz::Face` built from the cached `ttf_parser::Face`.
    /// Invalidated when `set_variations` is called, since variations affect
    /// the rustybuzz shaping tables.
    ///
    /// # Safety
    /// Same lifetime considerations as `cached_ttf_face` above.
    cached_rb_face: RefCell<Option<rustybuzz::Face<'static>>>,
}

impl Clone for FontFace {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            index: self.index,
            variations: self.variations.clone(),
            // Don't clone the cached faces; they will be lazily re-populated
            // from the cloned data on next access.
            cached_ttf_face: RefCell::new(None),
            cached_rb_face: RefCell::new(None),
        }
    }
}

impl fmt::Debug for FontFace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FontFace")
            .field("data", &self.data)
            .field("index", &self.index)
            .field("variations", &self.variations)
            .finish()
    }
}

impl FontFace {
    pub fn from_data_and_index(data: FontData, index: u32) -> Option<Self> {
        // Validate upfront so subsequent parse calls are expected to succeed.
        ttf_parser::Face::parse(data.as_slice(), index).ok()?;
        Some(Self {
            data,
            index,
            variations: Vec::new(),
            cached_ttf_face: RefCell::new(None),
            cached_rb_face: RefCell::new(None),
        })
    }

    /// Ensures the cached `ttf_parser::Face` is populated.
    fn ensure_ttf_face_cached(&self) {
        let mut cache = self.cached_ttf_face.borrow_mut();
        if cache.is_none() {
            let slice = self.data.as_slice();
            // Safety: `self.data` is heap-allocated behind `Rc` (SharedBytes)
            // and is never mutated, so the slice has a stable address for
            // the lifetime of this FontFace. We extend the slice lifetime
            // to 'static so the parsed Face can be stored in the cache.
            let static_slice: &'static [u8] = unsafe {
                std::slice::from_raw_parts(slice.as_ptr(), slice.len())
            };
            let face = ttf_parser::Face::parse(static_slice, self.index)
                .expect("font face became invalid after initial validation");
            *cache = Some(face);
        }
    }

    pub fn with_ttf_parser_face<R>(&self, f: impl FnOnce(&ttf_parser::Face<'_>) -> R) -> R {
        self.ensure_ttf_face_cached();
        let cache = self.cached_ttf_face.borrow();
        f(cache.as_ref().unwrap())
    }

    pub fn with_rustybuzz_face<R>(&self, f: impl FnOnce(&rustybuzz::Face<'_>) -> R) -> R {
        // Populate the rustybuzz cache if empty.
        {
            let mut rb_cache = self.cached_rb_face.borrow_mut();
            if rb_cache.is_none() {
                // Ensure the ttf_parser face is cached first, then build
                // the rustybuzz face from it.
                self.ensure_ttf_face_cached();
                let ttf_cache = self.cached_ttf_face.borrow();
                let ttf_face = ttf_cache.as_ref().unwrap();
                let mut rb_face = rustybuzz::Face::from_face(ttf_face.clone());
                if !self.variations.is_empty() {
                    rb_face.set_variations(&self.variations);
                }
                *rb_cache = Some(rb_face);
            }
        }
        let rb_cache = self.cached_rb_face.borrow();
        f(rb_cache.as_ref().unwrap())
    }

    pub fn data(&self) -> &FontData {
        &self.data
    }

    pub fn set_variations(&mut self, variations: &[(u32, f32)]) {
        self.variations.clear();
        self.variations
            .extend(variations.iter().map(|&(tag, value)| rustybuzz::Variation {
                tag: ttf_parser::Tag::from_bytes(&tag.to_be_bytes()),
                value,
            }));
        // Invalidate the cached rustybuzz face since variations affect shaping.
        *self.cached_rb_face.borrow_mut() = None;
    }
}
