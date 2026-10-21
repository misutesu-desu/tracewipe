use eframe::egui::{
    self, style::ScrollAnimation, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, TextStyle, Theme, Vec2,
};
use std::sync::Arc;

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct Pal {
    pub strip: Color32,
    pub content: Color32,
    pub status: Color32,
    pub nav: Color32,
    pub panel: Color32,
    pub field: Color32,
    pub hover: Color32,
    pub divider: Color32,
    pub line: Color32,
    pub popup_stroke: Color32,
    pub sel: Color32,
    pub pressed: Color32,
    pub sel_hover: Color32,
    pub control_stroke: Color32,
    pub button: Color32,
    pub button_stroke: Color32,
    pub text: Color32,
    pub label: Color32,
    pub weak: Color32,
    pub disabled: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    pub on_accent: Color32,
    pub danger: Color32,
    pub danger_pressed: Color32,
    pub error_text: Color32,
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub const DARK: Pal = Pal {
    strip: hex(0x0A0A0A),
    content: hex(0x191919),
    status: hex(0x1C1C1C),
    nav: hex(0x1F1F1F),
    panel: hex(0x202020),
    field: hex(0x2C2C2C),
    hover: hex(0x2D2D2D),
    divider: hex(0x303030),
    line: hex(0x3A3A3A),
    popup_stroke: hex(0x3C3C3C),
    sel: hex(0x414141),
    pressed: hex(0x424242),
    sel_hover: hex(0x4D4D4D),
    control_stroke: hex(0x8A8A8A),
    button: hex(0x2D2D2D),
    button_stroke: hex(0x3A3A3A),
    text: hex(0xF0F0F0),
    label: hex(0xE1E1E1),
    weak: hex(0xC8C8C8),
    disabled: hex(0x6D6D6D),
    accent: hex(0x0078D4),
    accent_text: hex(0x60CDFF),
    on_accent: hex(0x000000),
    danger: hex(0xC42B1C),
    danger_pressed: hex(0x941610),
    error_text: hex(0xFF99A4),
};

pub const LIGHT: Pal = Pal {
    strip: hex(0xE8E8E8),
    content: hex(0xFFFFFF),
    status: hex(0xFFFFFF),
    nav: hex(0xF9F9F9),
    panel: hex(0xF3F3F3),
    field: hex(0xFFFFFF),
    hover: hex(0xF0F0F0),
    divider: hex(0xE5E5E5),
    line: hex(0xE5E5E5),
    popup_stroke: hex(0xD7D7D7),
    sel: hex(0xCCE8FF),
    pressed: hex(0xDEDEDE),
    sel_hover: hex(0xBEE1FF),
    control_stroke: hex(0x8A8A8A),
    button: hex(0xFBFBFB),
    button_stroke: hex(0xDEDEDE),
    text: hex(0x1A1A1A),
    label: hex(0x232323),
    weak: hex(0x606060),
    disabled: hex(0xA0A0A0),
    accent: hex(0x0078D4),
    accent_text: hex(0x005FB8),
    on_accent: hex(0xFFFFFF),
    danger: hex(0xC42B1C),
    danger_pressed: hex(0x941610),
    error_text: hex(0xC42B1C),
};

pub fn pal(ctx: &egui::Context) -> &'static Pal {
    match ctx.theme() {
        Theme::Dark => &DARK,
        Theme::Light => &LIGHT,
    }
}

pub const ICONS: &str = "icons";

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let dir = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into()).join("Fonts");
    if let Ok(b) = std::fs::read(dir.join("segoeui.ttf")) {
        fonts.font_data.insert("segoe".into(), Arc::new(FontData::from_owned(b)));
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "segoe".into());
    }
    if let Ok(b) = std::fs::read(dir.join("consola.ttf")) {
        fonts.font_data.insert("consolas".into(), Arc::new(FontData::from_owned(b)));
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "consolas".into());
    }
    let icon = std::fs::read(dir.join("SegoeIcons.ttf")).or_else(|_| std::fs::read(dir.join("segmdl2.ttf")));
    let mut icon_family = vec![];
    if let Ok(b) = icon {
        fonts.font_data.insert("fluent".into(), Arc::new(FontData::from_owned(b)));
        icon_family.push("fluent".to_string());
    }
    icon_family.extend(fonts.families[&FontFamily::Proportional].iter().cloned());
    fonts.families.insert(FontFamily::Name(ICONS.into()), icon_family);
    ctx.set_fonts(fonts);

    for (theme, p) in [(Theme::Dark, &DARK), (Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |s| apply(s, p, theme));
    }
}

fn apply(s: &mut egui::Style, p: &Pal, theme: Theme) {
    s.text_styles = [
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(14.0)),
        (TextStyle::Small, FontId::proportional(12.0)),
        (TextStyle::Heading, FontId::proportional(20.0)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
    ]
    .into();
    s.animation_time = 0.0;
    s.scroll_animation = ScrollAnimation::none();
    s.spacing.item_spacing = Vec2::new(6.0, 4.0);
    s.spacing.button_padding = Vec2::new(8.0, 4.0);
    s.spacing.interact_size.y = 26.0;
    s.spacing.menu_margin = Margin::same(6);

    let v = &mut s.visuals;
    v.dark_mode = theme == Theme::Dark;
    v.panel_fill = p.panel;
    v.window_fill = p.field;
    v.extreme_bg_color = p.field;
    v.faint_bg_color = p.hover;
    v.window_stroke = Stroke::new(1.0, p.popup_stroke);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);
    let sh = Shadow { offset: [0, 8], blur: 16, spread: 0, color: Color32::from_black_alpha(71) };
    v.window_shadow = sh;
    v.popup_shadow = sh;
    v.hyperlink_color = p.accent_text;
    v.error_fg_color = p.error_text;
    v.warn_fg_color = p.error_text;
    v.selection.bg_fill = p.sel;
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.text_cursor.stroke = Stroke::new(2.0, p.accent_text);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.panel;
    w.noninteractive.weak_bg_fill = p.panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.divider);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = CornerRadius::same(5);
    for (st, fill) in [(&mut w.inactive, p.button), (&mut w.hovered, p.hover), (&mut w.active, p.pressed), (&mut w.open, p.pressed)] {
        st.bg_fill = fill;
        st.weak_bg_fill = fill;
        st.fg_stroke = Stroke::new(1.0, p.text);
        st.corner_radius = CornerRadius::same(5);
        st.expansion = 0.0;
        st.bg_stroke = Stroke::NONE;
    }
    w.inactive.bg_stroke = Stroke::new(1.0, p.button_stroke);
    w.open.bg_stroke = Stroke::new(1.0, p.button_stroke);
}
