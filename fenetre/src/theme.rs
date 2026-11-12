//! Window styling in CN Technology colours: night-blue top bar, very light
//! grey background, white cards, cyan for gains, orange for the main action
//! of each screen.
//! Embedded fonts (OFL licences, see NOTICE), reduced to the characters
//! actually used by polices/reduire.sh: Poppins for titles and the logo,
//! Inter for text, Noto Sans CJK and Noto Sans Arabic.

use eframe::egui::{self, Color32, FontFamily, FontId, Pos2, Rect, Rounding, Stroke, TextStyle, Vec2};

const fn rvb(r: u8, v: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, v, b)
}

pub const NUIT: Color32 = rvb(0x04, 0x1D, 0x45);
pub const NUIT_2: Color32 = rvb(0x0F, 0x2A, 0x5C);
pub const NUIT_TRAIT: Color32 = rvb(0x23, 0x40, 0x6E);
pub const NUIT_TEXTE: Color32 = rvb(0xA9, 0xBB, 0xD6);
pub const NUIT_CLAIR: Color32 = rvb(0xDC, 0xE6, 0xF5);
pub const FOND: Color32 = rvb(0xF4, 0xF6, 0xF9);
pub const BLANC: Color32 = Color32::WHITE;
pub const TRAIT: Color32 = rvb(0xE2, 0xE5, 0xEA);
pub const BORD: Color32 = rvb(0xC9, 0xD2, 0xDF);
pub const ENCRE: Color32 = rvb(0x0F, 0x1B, 0x2E);
pub const DOUX: Color32 = rvb(0x4A, 0x5A, 0x72);
pub const PALE: Color32 = rvb(0x5B, 0x6B, 0x82);
pub const CYAN: Color32 = rvb(0x01, 0xE3, 0xFD);
pub const TURQUOISE: Color32 = rvb(0x00, 0xB8, 0xCF);
pub const TURQUOISE_FONCE: Color32 = rvb(0x0F, 0x6B, 0x78);
pub const TURQUOISE_PALE: Color32 = rvb(0xE3, 0xF6, 0xF9);
pub const ORANGE: Color32 = rvb(0xFD, 0x70, 0x1A);
pub const ORANGE_FONCE: Color32 = rvb(0xB4, 0x47, 0x0C);
pub const ORANGE_PALE: Color32 = rvb(0xFF, 0xF1, 0xE8);
pub const BLEU_PALE: Color32 = rvb(0xEE, 0xF3, 0xFA);
pub const SELECTION: Color32 = rvb(0xEA, 0xF2, 0xFC);
pub const BLEU_MOYEN: Color32 = rvb(0x7F, 0xA6, 0xE0);
pub const BLEU_GRIS: Color32 = rvb(0x3C, 0x5A, 0x8A);
pub const VIOLET_PALE: Color32 = rvb(0xEE, 0xF0, 0xFF);
pub const VIOLET: Color32 = rvb(0x3B, 0x3F, 0xA0);
pub const GRIS_PALE: Color32 = rvb(0xE8, 0xED, 0xF5);

/// Titles (Poppins).
pub fn titre(taille: f32) -> FontId {
    FontId::new(taille, FontFamily::Name("titre".into()))
}

/// The "CNz" name in the top bar, always in Poppins.
pub fn logo(taille: f32) -> FontId {
    FontId::new(taille, FontFamily::Name("logo".into()))
}

/// Semi-bold text (Inter).
pub fn gras(taille: f32) -> FontId {
    FontId::new(taille, FontFamily::Name("gras".into()))
}

pub fn texte(taille: f32) -> FontId {
    FontId::proportional(taille)
}

/// Embedded fonts: (name, bytes).
const POLICES: &[(&str, &[u8])] = &[
    ("inter", include_bytes!("../polices/Inter-Regular.otf")),
    ("inter-gras", include_bytes!("../polices/Inter-SemiBold.otf")),
    ("poppins", include_bytes!("../polices/Poppins-Bold.ttf")),
    ("sc", include_bytes!("../polices/NotoSansSC-Regular.otf")),
    ("sc-gras", include_bytes!("../polices/NotoSansSC-Bold.otf")),
    ("jp", include_bytes!("../polices/NotoSansJP-Regular.otf")),
    ("jp-gras", include_bytes!("../polices/NotoSansJP-Bold.otf")),
    ("ko", include_bytes!("../polices/NotoSansKR-Regular.otf")),
    ("ko-gras", include_bytes!("../polices/NotoSansKR-Bold.otf")),
    ("arabe", include_bytes!("../polices/NotoSansArabic-Regular.ttf")),
    ("arabe-gras", include_bytes!("../polices/NotoSansArabic-Bold.ttf")),
];

/// Complete Chinese/Japanese system font, if there is one: the embedded
/// fonts only have the interface's characters, not those of file names.
fn police_systeme() -> Option<&'static [u8]> {
    static LUE: std::sync::OnceLock<Option<&'static [u8]>> = std::sync::OnceLock::new();
    *LUE.get_or_init(|| {
        let chemins: &[&str] = if cfg!(windows) {
            &["C:\\Windows\\Fonts\\msyh.ttc", "C:\\Windows\\Fonts\\YuGothR.ttc", "C:\\Windows\\Fonts\\meiryo.ttc"]
        } else {
            &["/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", "/System/Library/Fonts/PingFang.ttc"]
        };
        chemins.iter().find_map(|c| std::fs::read(c).ok()).map(|v| &*Box::leak(v.into_boxed_slice()))
    })
}

/// Fonts, in the order of the chosen language (Chinese, Japanese or Korean
/// first: some characters are drawn slightly differently). To redo when the
/// language changes.
pub fn polices(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (nom, octets) in POLICES {
        fonts.font_data.insert(nom.to_string(), egui::FontData::from_static(octets));
    }
    let systeme = police_systeme().map(|o| fonts.font_data.insert("systeme".into(), egui::FontData::from_static(o))).is_some();
    use crate::langue::{actuelle, COREEN, JAPONAIS, RUSSE, UKRAINIEN, VIETNAMIEN};
    let (cjk, cjk_gras) = match actuelle() {
        JAPONAIS => (["jp", "sc", "ko"], ["jp-gras", "sc-gras", "ko-gras"]),
        COREEN => (["ko", "sc", "jp"], ["ko-gras", "sc-gras", "jp-gras"]),
        _ => (["sc", "jp", "ko"], ["sc-gras", "jp-gras", "ko-gras"]),
    };
    // Titles: Inter when Poppins lacks some letters of the language (Cyrillic,
    // Vietnamese accents), so that two fonts are never mixed within one word.
    let titres: &[&str] = if [RUSSE, UKRAINIEN, VIETNAMIEN].contains(&actuelle()) { &["inter-gras"] } else { &["poppins", "inter-gras"] };
    // egui's original fonts stay as a fallback (symbols).
    let secours = fonts.families[&FontFamily::Proportional].clone();
    let famille = |tete: &[&str]| {
        let mut liste: Vec<String> = tete.iter().map(|n| n.to_string()).collect();
        if systeme {
            liste.push("systeme".into());
        }
        liste.extend(secours.iter().cloned());
        liste
    };
    let normal = famille(&["inter", cjk[0], cjk[1], cjk[2], "arabe"]);
    let gras = famille(&["inter-gras", cjk_gras[0], cjk_gras[1], cjk_gras[2], "arabe-gras"]);
    let mut tete_titre = titres.to_vec();
    tete_titre.extend([cjk_gras[0], cjk_gras[1], cjk_gras[2], "arabe-gras"]);
    fonts.families.insert(FontFamily::Proportional, normal);
    fonts.families.insert(FontFamily::Name("gras".into()), gras);
    fonts.families.insert(FontFamily::Name("titre".into()), famille(&tete_titre));
    fonts.families.insert(FontFamily::Name("logo".into()), famille(&["poppins"]));
    ctx.set_fonts(fonts);
}

/// Fonts and colours, once at launch.
pub fn installer(ctx: &egui::Context) {
    polices(ctx);
    ctx.style_mut(|s| {
        s.text_styles = [
            (TextStyle::Heading, titre(22.0)),
            (TextStyle::Body, texte(14.0)),
            (TextStyle::Button, gras(14.0)),
            (TextStyle::Small, texte(12.0)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
        s.spacing.item_spacing = Vec2::new(8.0, 8.0);
        s.spacing.button_padding = Vec2::new(14.0, 7.0);
        s.spacing.interact_size.y = 32.0;
        s.spacing.slider_rail_height = 6.0;

        let v = &mut s.visuals;
        *v = egui::Visuals::light();
        v.panel_fill = FOND;
        v.window_fill = BLANC;
        v.extreme_bg_color = BLANC;
        v.faint_bg_color = FOND;
        v.window_stroke = Stroke::new(1.0_f32, TRAIT);
        v.window_rounding = Rounding::same(10.0);
        v.menu_rounding = Rounding::same(8.0);
        v.selection.bg_fill = rvb(0xBF, 0xD7, 0xF5);
        v.selection.stroke = Stroke::new(1.5_f32, NUIT);
        v.hyperlink_color = NUIT;
        v.warn_fg_color = ORANGE_FONCE;
        v.error_fg_color = ORANGE_FONCE;
        v.text_cursor.stroke = Stroke::new(2.0_f32, NUIT);
        let w = &mut v.widgets;
        w.noninteractive.fg_stroke = Stroke::new(1.0_f32, ENCRE);
        w.noninteractive.bg_stroke = Stroke::new(1.0_f32, TRAIT);
        w.noninteractive.bg_fill = BLANC;
        for (etat, fond, bord) in [(&mut w.inactive, BLANC, BORD), (&mut w.hovered, BLEU_PALE, NUIT), (&mut w.active, SELECTION, NUIT), (&mut w.open, BLEU_PALE, BORD)] {
            etat.bg_fill = fond;
            etat.weak_bg_fill = fond;
            etat.fg_stroke = Stroke::new(1.5_f32, NUIT);
            etat.bg_stroke = Stroke::new(1.0_f32, bord);
            etat.rounding = Rounding::same(8.0);
            etat.expansion = 0.0;
        }
    });
}

/// The "CNz" logo: CN in white, z in cyan.
pub fn logo_cnz(ui: &mut egui::Ui, taille: f32) -> egui::Response {
    let mut job = egui::text::LayoutJob::default();
    job.append("CN", 0.0, egui::TextFormat { font_id: logo(taille), color: BLANC, ..Default::default() });
    job.append("z", 0.0, egui::TextFormat { font_id: logo(taille), color: CYAN, ..Default::default() });
    ui.add(egui::Label::new(job).sense(egui::Sense::click()).selectable(false))
}

/// Orange button, the screen's main action (night-blue text: readable).
pub fn bouton_principal(ui: &mut egui::Ui, actif: bool, texte: &str) -> bool {
    let b = egui::Button::new(egui::RichText::new(texte).font(titre(15.0)).color(if actif { NUIT } else { PALE }))
        .fill(if actif { ORANGE } else { TRAIT })
        .stroke(Stroke::NONE)
        .rounding(8.0)
        .min_size(Vec2::new(150.0, 44.0));
    let r = ui.add_enabled(actif, b);
    if actif {
        r.clone().on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    r.clicked()
}

/// Secondary button: white, with a night-blue border.
pub fn bouton(ui: &mut egui::Ui, texte: &str) -> egui::Response {
    let b = egui::Button::new(egui::RichText::new(texte).font(gras(14.0)).color(NUIT)).fill(BLANC).stroke(Stroke::new(1.5_f32, NUIT)).rounding(8.0).min_size(Vec2::new(0.0, 42.0));
    ui.add(b)
}

/// Toolbar button, more discreet.
pub fn bouton_outil(ui: &mut egui::Ui, actif: bool, texte: &str) -> egui::Response {
    let b = egui::Button::new(egui::RichText::new(texte).font(gras(13.0)).color(if actif { NUIT } else { PALE }))
        .fill(BLANC)
        .stroke(Stroke::new(1.0_f32, BORD))
        .rounding(7.0)
        .min_size(Vec2::new(0.0, 34.0));
    ui.add_enabled(actif, b)
}

/// Discreet link ("Cancel").
pub fn lien(ui: &mut egui::Ui, texte: &str) -> egui::Response {
    ui.add(egui::Button::new(egui::RichText::new(texte).font(gras(14.0)).color(DOUX)).frame(false).min_size(Vec2::new(0.0, 44.0)))
}

/// White card with a thin border.
pub fn carte() -> egui::Frame {
    egui::Frame::none().fill(BLANC).stroke(Stroke::new(1.0_f32, TRAIT)).rounding(14.0).inner_margin(egui::Margin::same(20.0))
}

/// Type badge ("CNz", "ZIP", "PDF"…), colours by family.
pub fn pastille_type(ui: &mut egui::Ui, texte: &str, fond: Color32, encre: Color32, taille: Vec2) {
    let (r, _) = ui.allocate_exact_size(taille, egui::Sense::hover());
    ui.painter().rect_filled(r, if taille.y > 24.0 { 8.0 } else { 4.0 }, fond);
    let police = if taille.y > 24.0 { titre(11.0) } else { gras(9.0) };
    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, texte, police, encre);
}

/// Family of a file from its extension: (label, background, ink).
pub fn famille(nom: &str, dossier: bool) -> (String, Color32, Color32) {
    if dossier {
        return (String::new(), GRIS_PALE, NUIT);
    }
    let ext = nom.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let (fond, encre) = match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff" | "heic" | "avif" | "ico" | "raw" | "cr2" | "nef" => (TURQUOISE_PALE, TURQUOISE_FONCE),
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp" | "rtf" | "txt" | "md" | "csv" | "epub" => (ORANGE_PALE, rvb(0x9A, 0x3D, 0x0A)),
        "cnz" | "zip" | "7z" | "rar" | "gz" | "tgz" | "xz" | "bz2" | "zst" | "tar" | "lz" | "lzma" => (NUIT, CYAN),
        "" => (GRIS_PALE, DOUX),
        _ => (VIOLET_PALE, VIOLET),
    };
    let mut e = if ext.is_empty() { "—".to_string() } else { ext.to_uppercase() };
    if e.chars().count() > 4 {
        e = e.chars().take(4).collect();
    }
    (e, fond, encre)
}

/// Drawn checkbox (16 px).
pub fn case(painter: &egui::Painter, centre: Pos2, cochee: bool) {
    let r = Rect::from_center_size(centre, Vec2::splat(16.0));
    if cochee {
        painter.rect_filled(r, 4.0, NUIT);
        let t = Stroke::new(2.0_f32, BLANC);
        painter.add(egui::Shape::line(vec![centre + Vec2::new(-4.0, 0.5), centre + Vec2::new(-1.0, 3.5), centre + Vec2::new(4.5, -3.0)], t));
    } else {
        painter.rect(r, 4.0, BLANC, Stroke::new(1.5_f32, rvb(0xAE, 0xBB, 0xCD)));
    }
}

/// Line icons, drawn in code (no images to embed).
#[derive(Clone, Copy)]
pub enum Icone {
    Dossier,
    Fichier,
    Loupe,
    Alerte,
    Coche,
    Chevron,
    Cadenas,
}

pub fn icone(painter: &egui::Painter, r: Rect, quoi: Icone, couleur: Color32) {
    let s = r.width().min(r.height()) / 24.0;
    let p = |x: f32, y: f32| Pos2::new(r.center().x + (x - 12.0) * s, r.center().y + (y - 12.0) * s);
    let t = Stroke::new(2.0_f32 * s.max(0.75), couleur);
    let ligne = |pts: &[(f32, f32)]| egui::Shape::line(pts.iter().map(|&(x, y)| p(x, y)).collect(), t);
    match quoi {
        Icone::Dossier => {
            painter.add(egui::Shape::closed_line(
                [(3.0, 7.0), (3.0, 18.0), (21.0, 18.0), (21.0, 8.0), (11.0, 8.0), (9.0, 5.5), (3.0, 5.5)].iter().map(|&(x, y)| p(x, y)).collect(),
                t,
            ));
        }
        Icone::Fichier => {
            painter.add(egui::Shape::closed_line([(6.0, 3.0), (14.0, 3.0), (19.0, 8.0), (19.0, 21.0), (6.0, 21.0)].iter().map(|&(x, y)| p(x, y)).collect(), t));
            painter.add(ligne(&[(14.0, 3.0), (14.0, 8.0), (19.0, 8.0)]));
        }
        Icone::Loupe => {
            painter.circle_stroke(p(11.0, 11.0), 6.5 * s, t);
            painter.add(ligne(&[(16.0, 16.0), (20.0, 20.0)]));
        }
        Icone::Alerte => {
            painter.add(egui::Shape::closed_line([(12.0, 3.0), (21.0, 19.0), (3.0, 19.0)].iter().map(|&(x, y)| p(x, y)).collect(), t));
            painter.add(ligne(&[(12.0, 9.5), (12.0, 13.5)]));
            painter.circle_filled(p(12.0, 16.5), 1.2 * s, couleur);
        }
        Icone::Coche => {
            painter.add(ligne(&[(5.0, 12.5), (9.5, 17.0), (19.0, 7.5)]));
        }
        Icone::Chevron => {
            painter.add(ligne(&[(9.0, 6.0), (15.0, 12.0), (9.0, 18.0)]));
        }
        Icone::Cadenas => {
            painter.rect_stroke(Rect::from_min_max(p(5.0, 11.0), p(19.0, 21.0)), 2.0 * s, t);
            painter.add(ligne(&[(8.0, 11.0), (8.0, 7.5), (10.0, 4.5), (14.0, 4.5), (16.0, 7.5), (16.0, 11.0)]));
        }
    }
}

/// Icon placed in the layout.
pub fn icone_ui(ui: &mut egui::Ui, quoi: Icone, taille: f32, couleur: Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(taille), egui::Sense::hover());
    icone(ui.painter(), r, quoi, couleur);
}

/// Large home illustration: a night-blue archive and a cyan arrow.
pub fn dessin_depot(painter: &egui::Painter, centre: Pos2, survol: bool) {
    let s = if survol { 1.08 } else { 1.0 };
    let p = |x: f32, y: f32| Pos2::new(centre.x + (x - 44.0) * s, centre.y + (y - 44.0) * s);
    painter.rect_filled(Rect::from_min_max(p(10.0, 30.0), p(78.0, 78.0)), 6.0 * s, NUIT);
    painter.rect_filled(Rect::from_min_max(p(10.0, 30.0), p(78.0, 42.0)), 3.0 * s, NUIT_2);
    painter.rect_filled(Rect::from_min_max(p(34.0, 52.0), p(54.0, 57.0)), 2.5 * s, CYAN);
    let t = Stroke::new(7.0_f32 * s, CYAN);
    painter.line_segment([p(44.0, 6.0), p(44.0, 36.0)], t);
    painter.add(egui::Shape::line(vec![p(31.0, 19.0), p(44.0, 5.0), p(57.0, 19.0)], t));
    for c in [p(44.0, 6.0), p(44.0, 36.0), p(31.0, 19.0), p(57.0, 19.0), p(44.0, 5.0)] {
        painter.circle_filled(c, 3.5 * s, CYAN);
    }
}

/// Proportion bar: `part` (0 to 1) filled with `couleur` on `fond`.
pub fn barre(painter: &egui::Painter, r: Rect, part: f32, couleur: Color32, fond: Color32) {
    let ronde = r.height() / 2.0;
    painter.rect_filled(r, ronde, fond);
    if part > 0.0 {
        let w = (r.width() * part.clamp(0.0, 1.0)).max(r.height());
        painter.rect_filled(Rect::from_min_size(r.min, Vec2::new(w, r.height())), ronde, couleur);
    }
}

/// End circle: turquoise tick, or orange cross.
pub fn rond_resultat(ui: &mut egui::Ui, ok: bool) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(56.0), egui::Sense::hover());
    let (fond, encre) = if ok { (TURQUOISE_PALE, TURQUOISE_FONCE) } else { (ORANGE_PALE, ORANGE_FONCE) };
    ui.painter().rect_filled(r, 14.0, fond);
    icone(ui.painter(), r.shrink(14.0), if ok { Icone::Coche } else { Icone::Alerte }, encre);
}
