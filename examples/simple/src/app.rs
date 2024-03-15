use std::rc::Rc;

use makepad_widgets::{icon_atlas::HtmlWalker, makepad_html::{parse_html, HtmlDoc}, text_flow::TextFlow, *};
 
live_design!{
    import makepad_draw::shader::std::*;
    import makepad_widgets::base::*;
    import makepad_widgets::theme_desktop_dark::*; 


    RobrixHtml = {{RobrixHtml}}<Html> { }

    App = {{App}} {

        ui: <Window>{
            show_bg: true
            width: Fill,
            height: Fill
            
            draw_bg: {
                fn pixel(self) -> vec4 {
                    //return #000
                    // test
                    return mix(#7, #3, self.pos.y);
                }
            }
            
            body = <ScrollXYView>{
                flow: Down,
                spacing:10,
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
                    draw_text: {d
                        color: #f
                    },
                    text: "Counter: 0"
                }
            


                html = <RobrixHtml> {
                    // font_size: 13,
                    // flow: RightWrap,
                    // width: 300.0,
                    // height: Fit,
                    // padding: 5,
                    // line_spacing: 10,
                    Button = <Button> {
                        text: "Hello world"
                    }
                    a = <Button> {
                        text: "Link here"
                    }
                    // before link <a href=\"https://www.google.com/\">Google</a> after link
                    body: "
                        this is <b>BOLD text</b>&nbsp;<br/><i>italic</i>  <Button>Hi</Button><br/><b><i>Bold italic</i></b>
                        <blockquote>\n<p>quote block single line</p>\n</blockquote>
                        text at the beginning <br /> <blockquote> <br /> quote block single line <br /> </blockquote> <br />
                        top text asldkja sldkja sldkja sldkjas dlkajs ldkajsdl laksjd laksjd laksjd laksjdlkahsldkjhaskj dhkljh lkjh lkldkjahsdlkja   lklkjadhs dl llkj h kll kdjjh  <br /> regular newline, no paragraph <p> after paragraph start </p> after paragraph end <br /> after newline <p> paragraph 1 </p> <p> paragraph 2 </p>
                        text at beginning <Button>My Button</Button> text after button before link <a href=\"https://www.google.com/me/\">Google2</a> after link before quote <blockquote>Quoted Text</blockquote> <h1>Header 1</h1>text after h1<h2>Header 2</h2>text after h2 <p> paragraph </p> <h3>Header 3</h3>text after h3<h4>Header 4</h4>text after h4<h5>Header 5</h5>text after h5<h6>Header 6</h6>text after h6 <br />
                        <li> <code> Inline code </code> </li>
                        this is <br/>
                            <li>one asdf asdfsadflkjalsdkfjl f  alsdkfjalsdkjflakj f sd lkafjldkfjaslkdjf sdflakdjlfjlksajaf asdlkfjlasdkfjlaskdjfla sdlfkjasldkfja sdlfkj asldkfja sldkfjas ldkfjlasdk </li>
                            <li>two</li>
                        <br/>
                        <code>let x = 1.0; this is a very long inline code to test wrapping </code> <pre> test block </pre> <br>
                        <b>BOLD text</b>&nbsp;normal&nbsp;&nbsp;after nbsp <hr/>Next line normal text button: <Button>Hi</Button><br/><blockquote>blockquote<br/><blockquote> nested blockquote</blockquote> after nested</blockquote><i>  <strong>Bold italic</i></strong>
                        <br>text at beginning <pre> this is a code block </pre> text after code block
                        <br /> this <hr> is <br/><li>one</li><br/><li>two</li><br/><code>let x = 1.0;</code><b>BOLD text</b>&nbsp;italic<br/><sep/>Next line normal text button: <Button>Hi</Button><br/><block_quote>blockquote<br/><block_quote>blockquote</block_quote></block_quote><i>Bold italic</i>
                    "

                }
                
                <Html>{
                    
                    Button = <Button> {
                        text: "Helloworld"
                    }  
                    // body:"before link <a href=\"https://www.google.com/\">Google</a> after link"

                    body:"
                    Normal <u>underlined html</u> <s>strike</s> text hello world <br/>
                    <li>one</li><br/>
                    <li>two</li><br/>
                    <code>let x = 1.0; testing another very long line of inline code to see if it wraps properly or not here i'm still going on a single line blah blah blah </code>
                    <b>BOLD text</b>&nbsp;<i>italic</i><br/>
                    <sep/>
                    Next line normal text button:<Button>Hi</Button><br/>
                    <block_quote>block<b>quote</b><br/><block_quote>blockquote</block_quote><br/>
                    Next line <br/>
                    <sep/>
                    </block_quote><b><i>Bold italic</i><br/>
                    <sep/></br>
                    "
                }
                <Markdown>{
                    
                    body:"
                    # MD H1 
                    ## H2 **Bold** *italic*
                    
                    1. aitem
                    1. item
                      2. item  
                      1. test   
                    4. item               
                                          
                    > block
                    > next
                    >> hi
                    continuation
                    
                    [link](https://image)
                    ![image](https://link)
                    Normal
                    Next line
                    
                    ---
                    ~~single newline~~ becomes space
                    *hello*hello world
                    
                        inline code
                        more inline code
                    Double newline
                    `inline code` text after
                    ```
                    let x = 10
                    let y = 10
                    ```
                    *italic* **Bold** normal _italic_ __bold__ ***Bolditalic*** normal
                    123
                    "
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

////////////////////////////////////////////////////////////////////////////////////
////////////////////////////////////////////////////////////////////////////////////
//////////////////////////// NEW ROBRIX HTML WIDGET ////////////////////////////////
////////////////////////////////////////////////////////////////////////////////////
////////////////////////////////////////////////////////////////////////////////////


fn open_header_tag(cx: &mut Cx2d, tf: &mut TextFlow, scale: f64) {
    tf.push_bold();
    tf.push_size_abs_scale(scale);
    cx.turtle_new_line();
}

#[derive(Live, Widget)]
pub struct RobrixHtml{
    #[deref] pub text_flow: TextFlow,
    #[live] pub body: Rc<String>,
    #[rust] pub doc: HtmlDoc
}

// alright lets parse the HTML
impl LiveHook for RobrixHtml{
    fn after_apply_from(&mut self, _cx: &mut Cx, _apply:&mut Apply) {
        let mut errors = Some(Vec::new());
        self.doc = parse_html(&*self.body, &mut errors);
        if errors.as_ref().unwrap().len()>0{
            log!("HTML parser returned errors {:?}", errors)
        }
    }
}

impl RobrixHtml{
    pub fn handle_custom_widget(cx: &mut Cx2d, scope: &mut Scope, tf: &mut TextFlow, node: &mut HtmlWalker, auto_id: &mut u64){
        let id = if let Some(id) = node.find_attr_lc(live_id!(id)){
            LiveId::from_str(id)
        } 
        else{
            *auto_id += 1;
            LiveId(*auto_id) 
        }; 
        let template = node.open_tag_nc().unwrap();
        if let Some(item) = tf.item(cx, id, template){
            item.set_text(node.find_text().unwrap_or(""));
            item.draw_all(cx, scope);
        }
        *node = node.jump_to_close();
    }
    

    /*
        * TODO: Implement the following tags:
        *  font
        *  a
        *  ul
        *  ol
        *  sup
        *  sub
        *  div
        *  table
        *  thead
        *  tbody
        *  tr
        *  th
        *  td
        *  caption
        *  span
        *  img
        *  details
        *  summary.
        */
    
    pub fn handle_open_tag(cx: &mut Cx2d, tf: &mut TextFlow, node: &mut HtmlWalker)->Option<LiveId>{
        match node.open_tag_lc(){
            some_id!(a) => {
                log!("{:?}", node.find_attr_lc(live_id!(href)));
                log!("{:?}", node.find_text());
                *node = node.jump_to_close();
            }
            some_id!(h1) => open_header_tag(cx, tf, 2.0),
            some_id!(h2) => open_header_tag(cx, tf, 1.5),
            some_id!(h3) => open_header_tag(cx, tf, 1.17),
            some_id!(h4) => open_header_tag(cx, tf, 1.0),
            some_id!(h5) => open_header_tag(cx, tf, 0.83),
            some_id!(h6) => open_header_tag(cx, tf, 0.67),

            some_id!(p) => {
                // there's probably a better way to do this by setting margins...
                cx.turtle_new_line();
                cx.turtle_new_line();
            }
            some_id!(code) => {
                tf.push_fixed();
                tf.begin_inline_code(cx);
            }
            some_id!(pre) => {
                cx.turtle_new_line();
                tf.push_fixed();
                tf.begin_code(cx);
            }
            some_id!(blockquote) => {
                cx.turtle_new_line();
                tf.begin_quote(cx);
            }
            some_id!(br) => cx.turtle_new_line(),
            some_id!(hr)
            | some_id!(sep) => {
                cx.turtle_new_line();
                tf.sep(cx);
                cx.turtle_new_line();
            }
            some_id!(u) => tf.push_underline(),
            some_id!(del)
            | some_id!(strike) => tf.push_strikethrough(),

            some_id!(b)
            | some_id!(strong) => tf.push_bold(),
            some_id!(i)
            | some_id!(em) => tf.push_italic(),

            some_id!(li) => {
                cx.turtle_new_line();
                tf.begin_list_item(cx, "\u{2022}", 1.0);
            }
            Some(x) => { return Some(x) }
            _ => ()
        } 
        None
    }
    
    pub fn handle_close_tag(cx: &mut Cx2d, tf:&mut TextFlow, node:&mut HtmlWalker)->Option<LiveId>{
        match node.close_tag_lc() {
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
            some_id!(p) => {
                cx.turtle_new_line();
                cx.turtle_new_line();
            }
            some_id!(blockquote) => tf.end_quote(cx),
            some_id!(code) => {
                tf.pop_fixed();
                tf.end_inline_code(cx);
            }
            some_id!(pre) => {
                tf.pop_fixed();
                tf.end_code(cx);     
            }
            some_id!(li) => tf.end_list_item(cx),            
            some_id!(u) => tf.pop_underline(),
            some_id!(del)
            | some_id!(strike) => tf.pop_strikethrough(),
            _ => ()
        }
        None
    }
    
    pub fn handle_text_node(cx: &mut Cx2d, tf: &mut TextFlow, node: &mut HtmlWalker) -> bool {
        if let Some(text) = node.text() {
            tf.draw_text(cx, text);
            true
        }
        else {
            false
        }
    }
}

impl Widget for RobrixHtml {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.text_flow.handle_event(cx, event, scope);
    }
    
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let tf = &mut self.text_flow;
        tf.begin(cx, walk); 
        // alright lets iterate the html doc and draw it
        let mut node = self.doc.walk();
        let mut auto_id = 0;
        while !node.empty(){
            match Self::handle_open_tag(cx, tf, &mut node){
                Some(_)=>{
                    Self::handle_custom_widget(cx, scope, tf, &mut node, &mut auto_id); 
                }
                _=>()
            }
            match Self::handle_close_tag(cx, tf, &mut  node){
                _=>()
            }
            Self::handle_text_node(cx, tf, &mut node);
            node = node.walk();
        }
        tf.end(cx);
        DrawStep::done()
    }  
     
    fn text(&self) -> String {
        self.body.as_ref().to_string()
    }
    
    fn set_text(&mut self, v:&str){
        self.body = Rc::new(v.to_string());
        let mut errors = Some(Vec::new());
        self.doc = parse_html(&*self.body, &mut errors);
        if errors.as_ref().unwrap().len()>0{
            log!("HTML parser returned errors {:?}", errors)
        }
    }
} 
 