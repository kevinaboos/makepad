use std::rc::Rc;

use makepad_widgets::{makepad_html::{parse_html, HtmlDoc}, text_flow::TextFlow, *};
 
live_design!{
    import makepad_draw::shader::std::*;
    import makepad_widgets::base::*;
    import makepad_widgets::theme_desktop_dark::*; 


    CustomHtml = {{CustomHtml}}<Html> { }

    App = {{App}} {

        ui: <Window>{
            show_bg: true
            width: Fill,
            height: Fill
            
            draw_bg: {
                fn pixel(self) -> vec4 {
                    //return #000
                    return mix(#7, #3, self.pos.y);
                }
            }
            
            body = <View>{
                flow: Down,
                spacing: 20,
                align: {
                    x: 0.5,
                    y: 0.5
                },
                button1 = <Button> {
                    text: "Hello world"
                }
                input1 = <TextInput> {
                    width: 100, height: 30
                    text: "Click to count"
                }
                label1 = <Label> {
                    draw_text: {
                        color: #f
                    },
                    text: "Counter: 0"
                }
                html_view = <View> {
                    align: {
                        x: 0.0,
                        y: 0.5
                    },


                    html = <CustomHtml> {
                        font_size: 13,
                        flow: RightWrap,
                        width: 300.0,
                        height: Fit,
                        padding: 5,
                        line_spacing: 10,
                        Button = <Button> {
                            text: "Hello world"
                        }
                        a = <Button> {
                            text: "Link here"
                        }
                        // body:"this is <b>BOLD text</b>&nbsp;<br/><i>italic</i>  <Button>Hi</Button><br/><b><i>Bold italic</i></b>"
                        // body:"<blockquote>\n<p>quote block single line</p>\n</blockquote>"
                        // body:"text at the beginning <br /> <blockquote> <br /> quote block single line <br /> </blockquote> <br />"
                        // body:"top text asldkja sldkja sldkja sldkjas dlkajs ldkajsdl laksjd laksjd laksjd laksjdlkahsldkjhaskj dhkljh lkjh lkldkjahsdlkja   lklkjadhs dl llkj h kll kdjjh  <br /> regular newline, no paragraph <p> after paragraph start </p> after paragraph end <br /> after newline <p> paragraph 1 </p> <p> paragraph 2 </p>"
                        // body:"text at beginning <Button>My Button</Button> text after button before link <a href=\"https://www.google.com/\">Google</a> after link before quote <blockquote>Quoted Text</blockquote> <h1>Header 1</h1>text after h1<h2>Header 2</h2>text after h2 <p> paragraph </p> <h3>Header 3</h3>text after h3<h4>Header 4</h4>text after h4<h5>Header 5</h5>text after h5<h6>Header 6</h6>text after h6"
                        body:"before link <a href=\"https://www.google.com/\">Google</a> after link"
                        // body: "<li> <code> Inline code </code> </li>"
                        // body:"this is <br/><li>one asdf asdfsadflkjalsdkfjl f  alsdkfjalsdkjflakj f sd lkafjldkfjaslkdjf sdflakdjlfjlksajaf asdlkfjlasdkfjlaskdjfla sdlfkjasldkfja sdlfkj asldkfja sldkfjas ldkfjlasdk </li><br/><li>two</li><br/><code>let x = 1.0;</code><b>BOLD text</b>&nbsp;italic<br/><sep/>Next line normal text button: <Button>Hi</Button><br/><block_quote>blockquote<br/><block_quote>blockquote</block_quote></block_quote><i><strong>Bold italic</i></strong>"
                        // body: "text at beginning <code> this is code </code> text after code"
                        // body:"this <hr> is <br/><li>one</li><br/><li>two</li><br/><code>let x = 1.0;</code><b>BOLD text</b>&nbsp;italic<br/><sep/>Next line normal text button: <Button>Hi</Button><br/><block_quote>blockquote<br/><block_quote>blockquote</block_quote></block_quote><i>Bold italic</i>"

                    }
                }
            }
        }
    }
} 
     
app_main!(App); 
 
#[derive(Live, LiveHook)]
pub struct App {
    #[live] ui: WidgetRef,
    #[rust] counter: usize,
}

impl LiveRegister for App {
    fn live_register(cx: &mut Cx) {
        crate::makepad_widgets::live_design(cx);
    }
}

impl MatchEvent for App {
    fn handle_actions(&mut self, cx: &mut Cx, actions:&Actions){
        if self.ui.button(id!(button1)).clicked(&actions) {
            log!("BUTTON CLICKED {}", self.counter); 
            self.counter += 1;
            let label = self.ui.label(id!(label1));
            label.set_text_and_redraw(cx,&format!("Counter: {}", self.counter));
        }
    }
}

impl AppMain for App {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        self.match_event(cx, event);
        self.ui.handle_event(cx, event, &mut Scope::empty());
    }
} 
/*
// This is our custom allocator!
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicU64, Ordering},
};

pub struct TrackingHeapWrap{
    count: AtomicU64,
    total: AtomicU64,
}

impl TrackingHeapWrap {
    // A const initializer that starts the count at 0.
    pub const fn new() -> Self {
        Self{
            count: AtomicU64::new(0),
            total: AtomicU64::new(0)
        }
    }
    
    // Returns the current count.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
    
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

unsafe impl GlobalAlloc for TrackingHeapWrap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Pass everything to System.
        self.count.fetch_add(1, Ordering::Relaxed); 
        self.total.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc(layout)
    }
        
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.count.fetch_sub(1, Ordering::Relaxed); 
        self.total.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
}

// Register our custom allocator.
#[global_allocator]
static TrackingHeap: TrackingHeapWrap = TrackingHeapWrap::new();*/


#[derive(Live, Widget)]
pub struct CustomHtml {
    #[deref] text_flow: TextFlow,
    #[live] body: Rc<String>,
    #[rust] doc: HtmlDoc,
}

// alright lets parse the HTML
impl LiveHook for CustomHtml {
    fn after_apply_from(&mut self, _cx: &mut Cx, _apply:&mut Apply) {
        let mut errors = Some(Vec::new());
        self.doc = parse_html(&*self.body, &mut errors);
        if errors.as_ref().unwrap().len()>0{
            log!("HTML parser returned errors {:?}", errors)
        }
    }
}

impl Widget for CustomHtml {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.text_flow.handle_event(cx, event, scope);
    }
    
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk:Walk)->DrawStep{
        let tf = &mut self.text_flow;
        tf.begin(cx, walk);
        let mut auto_id = 0;
        // alright lets iterate the html doc and draw it
        let mut node = self.doc.walk();
         
        while !node.empty(){

            /*
             * TODO: Implement the following tags:
                *  font
                *  del
                *  p
                *  a
                *  ul
                *  ol
                *  sup
                *  sub
                *  u
                *  strike
                *  div
                *  table
                *  thead
                *  tbody
                *  tr
                *  th
                *  td
                *  caption
                *  pre
                *  span
                *  img
                *  details
                *  summary.
             */
            
            match node.open_tag_lc(){
                // some_id!(a)=>{
                //     log!("node: {node:#?}");
                //     if let Some(href) = node.find_attr_lc(live_id!(href)){
                //         log!("got HREF {}", href);
                //         let template = node.open_tag_nc().unwrap();
                //         log!("got template {:?}", template);
                //         if let Some(item) = tf.item(cx, live_id!(Link), template){
                //             let node_text = node.find_text();
                //             log!("got node text {:?}", node_text);
                //             item.set_text(node_text.unwrap_or("fail"));
                //             item.draw_all(cx, scope);
                //         }
                //     }
                //     node = node.jump_to_close();
                // }
                some_id!(h1) => open_header_tag(cx, tf, 2.0),
                some_id!(h2) => open_header_tag(cx, tf, 1.5),
                some_id!(h3) => open_header_tag(cx, tf, 1.17),
                some_id!(h4) => open_header_tag(cx, tf, 1.0),
                some_id!(h5) => open_header_tag(cx, tf, 0.83),
                some_id!(h6) => open_header_tag(cx, tf, 0.67),

                some_id!(b)
                | some_id!(strong) => tf.push_bold(),
                some_id!(i)
                | some_id!(em) => tf.push_italic(),
                some_id!(p)=> {
                    // there's probably a better way to do this by setting margins...
                    cx.turtle_new_line();
                    cx.turtle_new_line();
                }

                some_id!(code)=>{
                    tf.push_fixed();
                    tf.begin_code(cx);
                } 
                some_id!(blockquote)=>tf.begin_quote(cx),
                some_id!(br)=>cx.turtle_new_line(),
                some_id!(hr)
                | some_id!(sep)=> {
                    cx.turtle_new_line();
                    tf.sep(cx);
                    cx.turtle_new_line();
                }
                some_id!(li)=>tf.begin_list_item(cx),
                Some(_)=>{ // custom widget
                    let id = if let Some(id) = node.find_attr_lc(live_id!(id)){
                        LiveId::from_str(id)
                    } 
                    else{
                        auto_id += 1;
                        LiveId(auto_id) 
                    }; 
                    let template = node.open_tag_nc().unwrap();
                    if let Some(item) = tf.item(cx, id, template){
                        item.set_text(node.find_text().unwrap_or(""));
                        item.draw_all(cx, scope);
                    }
                    node = node.jump_to_close();
                }
                _=>()
            } 
            match node.close_tag_lc(){
                some_id!(h1)
                | some_id!(h2)
                | some_id!(h3)
                | some_id!(h4)
                | some_id!(h5)
                | some_id!(h6) => {
                    tf.pop_size();
                    tf.pop_bold();
                    cx.turtle_new_line();
                }
                some_id!(b)
                | some_id!(strong) => tf.pop_bold(),
                some_id!(i)
                | some_id!(em) => tf.pop_italic(),
                some_id!(p)=> {
                    cx.turtle_new_line();
                    cx.turtle_new_line();
                }
                some_id!(blockquote)=>tf.end_quote(cx),
                some_id!(code) => {
                    tf.pop_fixed();
                    tf.end_code(cx); 
                }
                some_id!(li)=>tf.end_list_item(cx),
                _=>()
            }
            if let Some(text) = node.text(){
                tf.draw_text(cx, text);
            }
            node = node.walk();
        }
        tf.end(cx);
        DrawStep::done()
    }  
     
    fn text(&self)->String{
        self.body.as_ref().to_string()
    } 
    
    fn set_text(&mut self, v:&str){
        self.body = Rc::new(v.to_string())
    }
} 

fn open_header_tag(cx: &mut Cx2d, tf: &mut TextFlow, scale: f64) {
    tf.push_bold();
    tf.push_size_abs_scale(1.5);
    cx.turtle_new_line();
}
