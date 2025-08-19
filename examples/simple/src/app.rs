
use makepad_widgets::*;

live_design!{
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;

    pub ICON_CHECKMARK       = dep("crate://self/resources/checkmark.svg")
    pub ICON_CLOSE           = dep("crate://self/resources/close.svg")
    pub ICON_TRASH           = dep("crate://self/resources/trash.svg")

    pub COLOR_PRIMARY = #ffffff
    pub COLOR_PRIMARY_DARKER = #fefefe
    pub COLOR_SECONDARY = #eef2f4

    pub COLOR_ACTIVE_PRIMARY = #0f88fe
    pub COLOR_ACTIVE_PRIMARY_DARKER = #106fcc

    pub COLOR_AVATAR_BG = #52b2ac
    pub COLOR_AVATAR_BG_IDLE = #d8d8d8

    pub COLOR_UNREAD_MESSAGE_BADGE = (COLOR_AVATAR_BG)

    pub COLOR_TEXT_IDLE = #d8d8d8
    pub COLOR_TEXT = #1C274C
    pub COLOR_TEXT_INPUT_IDLE = #d8d8d8
    pub MESSAGE_TEXT_COLOR = #xEEEEEE
    pub COLOR_DIVIDER = #00000018
    pub COLOR_DIVIDER_DARK = #00000044

    pub COLOR_FG_ACCEPT_GREEN = #138808
    pub COLOR_BG_ACCEPT_GREEN = #F0FFF0
    pub COLOR_FG_DANGER_RED = #DC0005
    pub COLOR_BG_DANGER_RED = #FFF0F0
    pub COLOR_FG_DISABLED = #B3B3B3
    pub COLOR_BG_DISABLED = #E0E0E0


    pub LineH = <RoundedView> {
        width: Fill,
        height: 2.0,
        margin: 0.0,
        padding: 0.0, spacing: 0.0
        show_bg: true
        draw_bg: {color: (#CCC)}
    }

    // Customized button widget, based on the RoundedView shaders with some modifications
    // which is a better fit with our application UI design
    pub RobrixIconButton = <Button> {
        width: Fit,
        height: Fit,
        spacing: 10,
        padding: 10,
        align: {x: 0, y: 0.5}

        draw_bg: {
            instance color: #FFFFFF
            // We set a mid-gray hover color, which gets mixed with the bg color itself
            // in order to create a "lightening" effect upon hover.
            instance color_hover: #A
            instance border_size: 0.0
            instance border_color: #D0D5DD
            instance border_radius: 3.0

            fn get_color(self) -> vec4 {
                return mix(self.color, mix(self.color, self.color_hover, 0.2), self.hover)
            }

            fn pixel(self) -> vec4 {
                let sdf = Sdf2d::viewport(self.pos * self.rect_size)
                sdf.box(
                    self.border_size,
                    self.border_size,
                    self.rect_size.x - (self.border_size * 2.0),
                    self.rect_size.y - (self.border_size * 2.0),
                    max(1.0, self.border_radius)
                )
                sdf.fill_keep(self.get_color())
                if self.border_size > 0.0 {
                    sdf.stroke(self.border_color, self.border_size)
                }
                return sdf.result;
            }
        }

        draw_icon: {
            instance color: #000
            instance color_hover: #000
            fn get_color(self) -> vec4 {
                return mix(self.color, mix(self.color, self.color_hover, 0.2), self.hover)
            }
        }
        icon_walk: {width: 16, height: 16}

        draw_text: {
            text_style: {font_size: 10},
            color: #000
            fn get_color(self) -> vec4 {
                return self.color;
            }
        }
        text: ""
    }

    pub WalletEntry = <View> {
        width: Fill, height: Fit
        flow: Down

        wrapper = <View> {
            width: Fill, height: Fit
            flow: RightWrap,
            padding: 10

            wallet_name = <Label> {
                width: Fit, height: Fit
                flow: Right,
                margin: {top: 2.4, left: 0}
                draw_text: {
                    color: (MESSAGE_TEXT_COLOR),
                    text_style: <THEME_FONT_BOLD>{ font_size: 12 },
                }
                text: "[Wallet Name]"
            }

            wallet_path = <Label> {
                width: Fit, height: Fit
                flow: Right,
                margin: {top: 2.9, left: 8, bottom: 2}
                draw_text: {
                    color: (MESSAGE_TEXT_COLOR),
                    text_style: <THEME_FONT_REGULAR>{ font_size: 11 },
                }
                text: "[Wallet Path/URL]"
            }

            is_default_label_view = <View> {
                visible: false,
                width: Fit, height: Fit
                margin: {left: 20}
                <Label> {
                    margin: {top: 2.9}
                    width: Fit, height: Fit
                    flow: Right,
                    draw_text: {
                        color: (COLOR_FG_ACCEPT_GREEN),
                        text_style: <THEME_FONT_BOLD>{ font_size: 11 },
                    }
                    text: "✅ Default"
                }
            }

            not_found_label_view = <View> {
                visible: false,
                width: Fit, height: Fit
                margin: {left: 20}
                <Label> {
                    margin: {top: 2.9}
                    width: Fit, height: Fit
                    flow: Right,
                    draw_text: {
                        color: (COLOR_FG_DANGER_RED),
                        text_style: { font_size: 11 },
                    }
                    text: "Wallet not found!"
                }
            }

            set_default_wallet_button = <RobrixIconButton> {
                padding: {top: 10, bottom: 10, left: 12, right: 15}
                margin: {left: 20}
                draw_bg: {
                    color: (COLOR_ACTIVE_PRIMARY)
                }
                draw_icon: {
                    svg_file: (ICON_CHECKMARK)
                    color: (COLOR_PRIMARY)
                }
                draw_text: {
                    color: (COLOR_PRIMARY)
                }
                icon_walk: {width: 16, height: 16}
                text: "Set As Default"
            }

            remove_wallet_button = <RobrixIconButton> {
                padding: {top: 10, bottom: 10, left: 12, right: 15}
                margin: {left: 20}
                draw_bg: {
                    color: (COLOR_BG_DANGER_RED)
                    border_color: (COLOR_FG_DANGER_RED)
                }
                draw_icon: {
                    svg_file: (ICON_CLOSE),
                    color: (COLOR_FG_DANGER_RED),
                }
                draw_text: {
                    color: (COLOR_FG_DANGER_RED),
                }
                icon_walk: { width: 16, height: 16 }
                text: "Remove From List"
            }

            delete_wallet_button = <RobrixIconButton> {
                padding: {top: 10, bottom: 10, left: 12, right: 15}
                margin: {left: 20}
                draw_bg: {
                    color: (COLOR_BG_DANGER_RED)
                    border_color: (COLOR_FG_DANGER_RED)
                }
                draw_icon: {
                    svg_file: (ICON_TRASH),
                    color: (COLOR_FG_DANGER_RED),
                }
                draw_text: {
                    color: (COLOR_FG_DANGER_RED),
                }
                icon_walk: { width: 16, height: 16 }
                text: "Delete Wallet"
            }
        }

        <LineH> { padding: 10, margin: {left: 5, right: 5} }
    }


    App = {{App}} {
        ui: <Root>{
            main_window = <Window>{
                body = <View>{
                    flow: Down,
                    spacing: 20,

                    <View> { height: 100, width: Fill}

                    <WalletEntry> {
                        wrapper = {
                            is_default_label_view = { visible: true}
                        }
                    }
                    <WalletEntry> {
                        wrapper = {
                            wallet_path = {
                                text: "sqlite:////Users/kevinboos/Library/Application Support/org.robius.robrix/tsp_wallets/Kevin's_Wallet.sqlite",
                            }
                        }
                    }
                    <WalletEntry> {
                        wrapper = {
                            not_found_label_view = { visible: true }
                        }
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

impl MatchEvent for App{
    fn handle_startup(&mut self, _cx:&mut Cx){
    }
        
    fn handle_actions(&mut self, cx: &mut Cx, actions:&Actions){
        if self.ui.button(id!(button_1)).clicked(&actions) {
            self.ui.button(id!(button_1)).set_text(cx, "Clicked 😀");
            log!("hi");
            self.counter += 1;
        }
    }
}

impl AppMain for App {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        if let Event::XrUpdate(_e) = event{
            //log!("{:?}", e.now.left.trigger.analog);
        }
        self.match_event(cx, event);
        self.ui.handle_event(cx, event, &mut Scope::empty());
    }
}