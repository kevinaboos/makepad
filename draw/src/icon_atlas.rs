pub use {
    std::{
        rc::Rc,
        cell::RefCell,
        io::prelude::*,
        fs::File,
        collections::HashMap,
    },
    crate::{
        shader::draw_trapezoid::DrawTrapezoidVector,
        makepad_platform::*,
        cx_2d::Cx2d,
        turtle::{Walk, Layout},
        view::{ManyInstances, View, ViewRedrawingApi},
        geometry::GeometryQuad2D,
        makepad_vector::trapezoidator::Trapezoidator,
        makepad_vector::geometry::{AffineTransformation, Transform, Vector, Point},
        makepad_vector::internal_iter::*,
        makepad_vector::path::{PathIterator,PathCommand},
    }
};

#[derive(Clone, Copy)]
pub struct CxIconAtlasPos {
    pub t1: Vec2,
    pub t2: Vec2,
    pub chan: f32
}

#[derive(Clone)]
pub struct CxIconAtlasEntry {
    pub pos: CxIconAtlasPos,
    pub translate: DVec2,
    pub path: Vec<PathCommand>,
    pub subpixel_fract: Vec2,
    pub size: Vec2,
    pub scale: f64,
}


impl<'a> InternalIterator for &CxIconAtlasEntry {
    type Item = PathCommand;
    fn for_each<F>(self, f: &mut F) -> bool
    where
        F: FnMut(PathCommand) -> bool,
    {
        for item in &self.path{
            if !f(item.clone()){
                return false
            }
        }
        true
    }
}

pub struct CxIconAtlas {
    pub texture_id: TextureId,
    pub clear_buffer: bool,
    pub entries: HashMap<u64, CxIconAtlasEntry>,
    pub alloc: CxIconAtlasAlloc
}

#[derive(Default)]
pub struct CxIconAtlasAlloc {
    pub texture_size: DVec2,
    pub xpos: f64,
    pub ypos: f64,
    pub hmax: f64,
    pub todo: Vec<u64>,
}


impl CxIconAtlas {
    pub fn new(texture_id: TextureId) -> Self {
        Self {
            texture_id,
            clear_buffer: false,
            entries: HashMap::new(),
            alloc: CxIconAtlasAlloc {
                texture_size: DVec2 {x: 2048.0, y: 2048.0},
                xpos: 0.0,
                ypos: 0.0,
                hmax: 0.0,
                todo: Vec::new(),
            }
        }
    }
    
    pub fn get_icon_pos(&mut self, translate:DVec2, scale:f64, subpixel_fract: Vec2, size: Vec2, path: &str) -> Option<CxIconAtlasPos> {
        let hash = LiveId::from_str_unchecked(path)
            .bytes_append(&subpixel_fract.x.to_be_bytes())
            .bytes_append(&subpixel_fract.y.to_be_bytes())
            .bytes_append(&size.x.to_be_bytes())
            .bytes_append(&size.y.to_be_bytes())
            .bytes_append(&translate.x.to_be_bytes())
            .bytes_append(&translate.y.to_be_bytes())
            .bytes_append(&scale.to_be_bytes());
        
        if let Some(entry) = self.entries.get(&hash.0){
            return Some(entry.pos)
        }
        match parse_svg_path(path.as_bytes()){
            Ok(path)=>{
                let pos = self.alloc.alloc_icon_pos(size.x as f64, size.y as f64);
                self.entries.insert(
                    hash.0,
                    CxIconAtlasEntry{
                        translate,
                        scale,
                        pos,
                        path,
                        subpixel_fract,
                        size
                    }
                );
                self.alloc.todo.push(hash.0);
                return Some(pos)
            }
            Err(v)=>{
                error!("Error parsing svg path {:?}", v);
                None
            }
        }
    }
}
impl CxIconAtlasAlloc {
    pub fn alloc_icon_pos(&mut self, w: f64, h: f64) -> CxIconAtlasPos {
        if w + self.xpos >= self.texture_size.x {
            self.xpos = 0.0;
            self.ypos += self.hmax + 1.0;
            self.hmax = 0.0;
        }
        if h + self.ypos >= self.texture_size.y {
            println!("VECTOR ATLAS FULL, TODO FIX THIS {} > {},", h + self.ypos, self.texture_size.y);
        }
        if h > self.hmax {
            self.hmax = h;
        }
        
        let tx1 = self.xpos / self.texture_size.x;
        let ty1 = self.ypos / self.texture_size.y;
        
        self.xpos += w + 1.0;
        
        CxIconAtlasPos {
            chan: 0.0,
            t1: dvec2(tx1, ty1).into(),
            t2: dvec2(tx1 + (w / self.texture_size.x), ty1 + (h / self.texture_size.y)).into()
        }
    }
}

#[derive(Clone)]
pub struct CxIconAtlasRc(pub Rc<RefCell<CxIconAtlas >>);

impl CxIconAtlas {
    pub fn reset_vector_atlas(&mut self) {
        self.alloc.xpos = 0.;
        self.alloc.ypos = 0.;
        self.alloc.hmax = 0.;
        self.clear_buffer = true;
    }
    
    pub fn get_internal_atlas_texture_id(&self) -> TextureId {
        self.texture_id
    }
}


impl DrawTrapezoidVector {
    // atlas drawing function used by CxAfterDraw
    pub fn draw_vector(&mut self, entry:&CxIconAtlasEntry,  many: &mut ManyInstances) {
        let trapezoids = {
            let mut trapezoids = Vec::new();
            //log_str(&format!("Serializing char {} {} {} {}", glyphtc.tx1 , cx.fonts_atlas.texture_size.x ,todo.subpixel_x_fract ,atlas_page.dpi_factor));
            let trapezoidate = self.trapezoidator.trapezoidate(
                entry.map({
                    move | cmd | {
                        let cmd = cmd.transform(
                            &AffineTransformation::identity()
                                .translate(Vector::new(entry.translate.x, entry.translate.y))
                                .uniform_scale(entry.scale)
                        );
                        //log!("GOT COMMAND {:?}", cmd);
                        match cmd{
                            PathCommand::MoveTo(_p)=>cmd,
                            PathCommand::LineTo(_p)=>cmd,
                            PathCommand::QuadraticTo(_p1, _p)=>cmd,
                            PathCommand::CubicTo(_p1,_p2,_p)=>cmd,
                            PathCommand::Close=>cmd
                        }
                    }
                }).linearize(0.5)
            );
            if let Some(trapezoidate) = trapezoidate {
                trapezoids.extend_from_internal_iter(
                    trapezoidate
                );
            }
            trapezoids
        };
        for trapezoid in trapezoids {
            self.a_xs = Vec2 {x: trapezoid.xs[0], y: trapezoid.xs[1]};
            self.a_ys = Vec4 {x: trapezoid.ys[0], y: trapezoid.ys[1], z: trapezoid.ys[2], w: trapezoid.ys[3]};
            self.chan = 0.0 as f32;
            many.instances.extend_from_slice(self.draw_vars.as_slice());
        }
    }
}

#[derive(Clone)]
pub struct CxDrawIconAtlasRc(pub Rc<RefCell<CxDrawIconAtlas >>);

pub struct CxDrawIconAtlas {
    pub draw_trapezoid: DrawTrapezoidVector,
    pub atlas_pass: Pass,
    pub atlas_view: View,
    pub atlas_texture: Texture,
}

impl CxDrawIconAtlas {
    pub fn new(cx: &mut Cx) -> Self {
        
        let atlas_texture = Texture::new(cx);
        
        //cx.fonts_atlas.texture_id = Some(atlas_texture.texture_id());
        
        let draw_trapezoid = DrawTrapezoidVector::new_local(cx);
        // ok we need to initialize drawtrapezoidtext from a live pointer.
        Self {
            draw_trapezoid,
            atlas_pass: Pass::new(cx),
            atlas_view: View::new(cx),
            atlas_texture: atlas_texture
        }
    }
}

impl<'a> Cx2d<'a> {
    pub fn lazy_construct_icon_atlas(cx: &mut Cx) {
        // ok lets fetch/instance our CxFontsAtlasRc
        if !cx.has_global::<CxIconAtlasRc>() {
            
            let draw_atlas = CxDrawIconAtlas::new(cx);
            let texture_id = draw_atlas.atlas_texture.texture_id();
            cx.set_global(CxDrawIconAtlasRc(Rc::new(RefCell::new(draw_atlas))));
            
            let atlas = CxIconAtlas::new(texture_id);
            cx.set_global(CxIconAtlasRc(Rc::new(RefCell::new(atlas))));
        }
    }
    
    pub fn reset_icon_atlas(cx: &mut Cx) {
        if cx.has_global::<CxIconAtlasRc>() {
            let mut fonts_atlas = cx.get_global::<CxIconAtlasRc>().0.borrow_mut();
            fonts_atlas.reset_vector_atlas();
        }
    }
    
    pub fn draw_icon_atlas(&mut self) {
        let draw_atlas_rc = self.cx.get_global::<CxDrawIconAtlasRc>().clone();
        let mut draw_atlas = draw_atlas_rc.0.borrow_mut();
        let atlas_rc = self.icon_atlas_rc.clone();
        let mut atlas = atlas_rc.0.borrow_mut();
        let atlas = &mut*atlas;
        //let start = Cx::profile_time_ns();
        // we need to start a pass that just uses the texture
        if atlas.alloc.todo.len()>0 {
            self.begin_pass(&draw_atlas.atlas_pass);

            let texture_size = atlas.alloc.texture_size;
            draw_atlas.atlas_pass.set_size(self.cx, texture_size);
            
            let clear = if atlas.clear_buffer {
                atlas.clear_buffer = false;
                PassClearColor::ClearWith(Vec4::default())
            }
            else {
                PassClearColor::InitWith(Vec4::default())
            };
            
            draw_atlas.atlas_pass.clear_color_textures(self.cx);
            draw_atlas.atlas_pass.add_color_texture(self.cx, &draw_atlas.atlas_texture, clear);
            draw_atlas.atlas_view.begin_always(self);

            let mut atlas_todo = Vec::new();
            std::mem::swap(&mut atlas.alloc.todo, &mut atlas_todo);
            
            if let Some(mut many) = self.begin_many_instances(&draw_atlas.draw_trapezoid.draw_vars) {
                for todo in atlas_todo {
                    let entry = atlas.entries.get(&todo).unwrap();
                    
                    draw_atlas.draw_trapezoid.draw_vector(entry, &mut many);
                }
                
                self.end_many_instances(many);
            }
            draw_atlas.atlas_view.end(self);
            self.end_pass(&draw_atlas.atlas_pass);
        }
    }
}


fn parse_svg_path(path: &[u8]) -> Result<Vec<PathCommand>, String> {
    #[derive(Debug)]
    enum Cmd {
        Unknown,
        Move(bool),
        Hor(bool),
        Vert(bool),
        Line(bool),
        Cubic(bool),
        Quadratic(bool),
        Close
    }
    impl Default for Cmd {fn default() -> Self {Self::Unknown}}
    
    #[derive(Default)]
    struct ParseState {
        cmd: Cmd,
        expect_nums: usize,
        chain: bool,
        nums: [f64; 6],
        num_count: usize,
        last_pt: Point,
        out: Vec<PathCommand>,
        num_state: Option<NumState>
    }
    
    struct NumState {
        num: f64,
        mul: f64,
        has_dot: bool,
    }
    
    impl NumState {
        fn new_pos(v: f64) -> Self {Self {num: v, mul: 1.0, has_dot: false}}
        fn new_min() -> Self {Self {num: 0.0, mul: -1.0, has_dot: false}}
        fn finalize(self) -> f64 {self.num * self.mul}
        fn add_digit(&mut self, digit: f64) {
            self.num *= 10.0;
            self.num += digit;
            if self.has_dot {
                self.mul *= 0.1;
            }
        }
    }
    
    impl ParseState {
        fn next_cmd(&mut self, cmd: Cmd) -> Result<(), String> {
            self.finalize_cmd() ?;
            self.chain = false;
            self.expect_nums = match cmd {
                Cmd::Unknown => panic!(),
                Cmd::Move(_) => 2,
                Cmd::Hor(_) => 1,
                Cmd::Vert(_) => 1,
                Cmd::Line(_) => 2,
                Cmd::Cubic(_) => 6,
                Cmd::Quadratic(_) => 4,
                Cmd::Close => 0
            };
            self.cmd = cmd;
            Ok(())
        }
        
        fn add_min(&mut self) -> Result<(), String> {
            if self.expect_nums == self.num_count {
                self.finalize_cmd() ?;
            }
            if self.expect_nums == 0 {
                return Err(format!("Unexpected minus"));
            }
            self.num_state = Some(NumState::new_min());
            Ok(())
        }
        
        fn add_digit(&mut self, digit: f64) -> Result<(), String> {
            if let Some(num_state) = &mut self.num_state {
                num_state.add_digit(digit);
            }
            else {
                if self.expect_nums == self.num_count {
                    self.finalize_cmd() ?;
                }
                if self.expect_nums == 0 {
                    return Err(format!("Unexpected digit"));
                }
                self.num_state = Some(NumState::new_pos(digit))
            }
            Ok(())
        }
        
        fn add_dot(&mut self) -> Result<(), String> {
            if let Some(num_state) = &mut self.num_state {
                if num_state.has_dot {
                    return Err(format!("Unexpected ."));
                }
                num_state.has_dot = true;
            }
            else {
                return Err(format!("Unexpected ."));
            }
            Ok(())
        }
        
        fn finalize_num(&mut self) {
            if let Some(num_state) = self.num_state.take() {
                self.nums[self.num_count] = num_state.finalize();
                self.num_count += 1;
            }
        }
        
        fn whitespace(&mut self)-> Result<(), String> {
            self.finalize_num();
            if self.expect_nums == self.num_count {
                self.finalize_cmd() ?;
            }
            Ok(())
        }
        
        fn finalize_cmd(&mut self) -> Result<(), String> {
            self.finalize_num();
            if self.chain && self.num_count == 0{
                return Ok(())
            }
            if self.expect_nums != self.num_count {
                return Err(format!("SVG Path command {:?} expected {} points, got {}", self.cmd, self.expect_nums, self.num_count));
            }
            match self.cmd {
                Cmd::Unknown => (),
                Cmd::Move(abs) => {
                    if abs {
                        self.last_pt = Point{x:self.nums[0], y:self.nums[1]};
                    }
                    else {
                        self.last_pt += Vector{x:self.nums[0], y:self.nums[1]};
                    }
                    self.out.push(PathCommand::MoveTo(self.last_pt));
                },
                Cmd::Hor(abs) => {
                    if abs {
                        self.last_pt = Point{x:self.nums[0], y:self.last_pt.y};
                    }
                    else {
                        self.last_pt += Vector{x:self.nums[0], y:0.0};
                    }
                    self.out.push(PathCommand::LineTo(self.last_pt));
                }
                Cmd::Vert(abs) => {
                    if abs {
                        self.last_pt = Point{x:self.last_pt.x, y:self.nums[0]};
                    }
                    else {
                        self.last_pt += Vector{x:0.0, y:self.nums[0]};
                    }
                    self.out.push(PathCommand::LineTo(self.last_pt));
                }
                Cmd::Line(abs) => {
                    if abs {
                        self.last_pt = Point{x:self.nums[0], y:self.nums[1]};
                    }
                    else {
                        self.last_pt += Vector{x:self.nums[0], y:self.nums[1]};
                    }
                    self.out.push(PathCommand::LineTo(self.last_pt));
                },
                Cmd::Cubic(abs) => {
                    if abs {
                        self.last_pt = Point{x:self.nums[4], y:self.nums[5]};
                    }
                    else {
                        self.last_pt += Vector{x:self.nums[4], y:self.nums[5]};
                    }
                    self.out.push(PathCommand::CubicTo(
                        Point{x:self.nums[0], y:self.nums[1]},
                        Point{x:self.nums[1], y:self.nums[2]},
                        self.last_pt,
                    ))
                },
                Cmd::Quadratic(abs) => {
                    if abs {
                        self.last_pt = Point{x:self.nums[2], y:self.nums[3]};
                    }
                    else {
                        self.last_pt += Vector{x:self.nums[2], y:self.nums[3]};
                    }
                    self.out.push(PathCommand::QuadraticTo(
                        Point{x:self.nums[0], y:self.nums[1]},
                        self.last_pt
                    ));
                }
                Cmd::Close => {
                    self.out.push(PathCommand::Close);
                }
            }
            self.num_count = 0;
            self.chain = true;
            Ok(())
        }
    }
    
    let mut state = ParseState::default();
    for i in 0..path.len() {
        match path[i]  {
            b'M' => state.next_cmd(Cmd::Move(true)) ?,
            b'm' => state.next_cmd(Cmd::Move(false)) ?,
            b'Q' => state.next_cmd(Cmd::Quadratic(true)) ?,
            b'q' => state.next_cmd(Cmd::Quadratic(false)) ?,
            b'C' => state.next_cmd(Cmd::Cubic(true)) ?,
            b'c' => state.next_cmd(Cmd::Cubic(false)) ?,
            b'H' => state.next_cmd(Cmd::Hor(true)) ?,
            b'h' => state.next_cmd(Cmd::Hor(false)) ?,
            b'V' => state.next_cmd(Cmd::Vert(true)) ?,
            b'v' => state.next_cmd(Cmd::Vert(false)) ?,
            b'L' => state.next_cmd(Cmd::Line(true)) ?,
            b'l' => state.next_cmd(Cmd::Line(false)) ?,
            b'Z' | b'z' => state.next_cmd(Cmd::Close) ?,
            b'-' => state.add_min() ?,
            b'0'..=b'9' => state.add_digit((path[i] - b'0') as f64) ?,
            b'.' => state.add_dot() ?,
            b',' | b' ' | b'\r' | b'\n' | b'\t' => state.whitespace()?,
            x => {
                return Err(format!("Unexpected character {} - {}", x, x as char))
            }
        }
    }
    state.finalize_cmd() ?;
    Ok(state.out)
}

