//! Tokens de color para tema claro y oscuro. Contraste alto en texto (≥ 7:1 sobre el fondo).

use crate::icons::Icon;
use gpui::{Rgba, WindowAppearance, rgb, rgba};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    System,
    Light,
    Dark,
}

impl Mode {
    pub fn next(self) -> Self {
        match self {
            Mode::System => Mode::Light,
            Mode::Light => Mode::Dark,
            Mode::Dark => Mode::System,
        }
    }

    pub fn label(self, t: &crate::i18n::Strings) -> (Icon, &'static str) {
        match self {
            Mode::System => (Icon::Contrast, t.theme_auto),
            Mode::Light => (Icon::Sun, t.theme_light),
            Mode::Dark => (Icon::Moon, t.theme_dark),
        }
    }

    pub fn resolve(self, appearance: WindowAppearance) -> Theme {
        let dark = match self {
            Mode::Light => false,
            Mode::Dark => true,
            Mode::System => matches!(
                appearance,
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
        };
        if dark { Theme::dark() } else { Theme::light() }
    }
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Rgba,
    pub frame: Rgba,
    pub path_bg: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub faint: Rgba,
    pub accent: Rgba,
    pub selection: Rgba,
    pub hover: Rgba,
    pub control: Rgba,
    pub error: Rgba,
    pub popover: Rgba,
    /// Fondo de botones destructivos (texto blanco encima, contraste ≥ 4.5:1).
    pub danger_bg: Rgba,
    pub scrim: Rgba,
    pub text_selection: Rgba,
    pub tile_file: Rgba,
    pub tile_file_fg: Rgba,
    // Tintes por categoría de fichero
    pub code: Rgba,
    pub image: Rgba,
    pub media: Rgba,
    pub archive: Rgba,
    pub doc: Rgba,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            bg: rgb(0x171b24),
            frame: rgb(0x10131b),
            path_bg: rgb(0x1a1f2a),
            border: rgb(0x2c3342),
            text: rgb(0xeceef1),
            muted: rgb(0xa5adba),
            faint: rgb(0x8993a5),
            accent: rgb(0x85a6ff),
            selection: rgba(0x789aff30),
            hover: rgba(0xffffff0f),
            control: rgba(0xffffff12),
            error: rgb(0xff7a8a),
            popover: rgb(0x202634),
            danger_bg: rgb(0xc9303f),
            scrim: rgba(0x00000088),
            text_selection: rgba(0x6e9bff59),
            tile_file: rgb(0x1f232b),
            tile_file_fg: rgb(0xb3b9c2),
            code: rgb(0xb18cff),
            image: rgb(0x4fd18b),
            media: rgb(0xff8fb8),
            archive: rgb(0xffb257),
            doc: rgb(0x5cc8ff),
        }
    }

    pub fn light() -> Self {
        Self {
            bg: rgb(0xffffff),
            frame: rgb(0xf1f4f9),
            path_bg: rgb(0xffffff),
            border: rgb(0xdde4ee),
            text: rgb(0x111318),
            muted: rgb(0x555c68),
            faint: rgb(0x606d80),
            accent: rgb(0x2563eb),
            selection: rgba(0x2563eb1f),
            hover: rgba(0x0000000a),
            control: rgba(0x0000000d),
            error: rgb(0xc8102e),
            popover: rgb(0xffffff),
            danger_bg: rgb(0xc8102e),
            scrim: rgba(0x0000004d),
            text_selection: rgba(0x2563eb40),
            tile_file: rgb(0xeef0f3),
            tile_file_fg: rgb(0x555c68),
            code: rgb(0x7c3aed),
            image: rgb(0x0f9d58),
            media: rgb(0xd6336c),
            archive: rgb(0xd9730d),
            doc: rgb(0x0b7bc4),
        }
    }

    /// Icono y color de la "tile" de un fichero según su extensión.
    pub fn file_style(&self, ext: &str) -> (Icon, Option<Rgba>) {
        match self.file_tint(ext) {
            Some(c) if c == self.code => (Icon::FileCode, Some(c)),
            Some(c) if c == self.image => (Icon::FileImage, Some(c)),
            Some(c) if c == self.media => (Icon::FileMedia, Some(c)),
            Some(c) if c == self.archive => (Icon::FileArchive, Some(c)),
            Some(c) => (Icon::FileText, Some(c)),
            None => (Icon::File, None),
        }
    }

    fn file_tint(&self, ext: &str) -> Option<Rgba> {
        Some(match ext {
            "rs" | "go" | "ts" | "tsx" | "js" | "jsx" | "py" | "php" | "vue" | "c" | "h"
            | "cpp" | "hpp" | "cs" | "java" | "kt" | "swift" | "rb" | "sh" | "lua" | "zig"
            | "toml" | "json" | "yaml" | "yml" | "xml" | "html" | "css" | "sql" => self.code,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "avif" | "bmp" | "ico" | "heic" => {
                self.image
            }
            "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" | "mp4" | "mkv" | "webm" | "mov"
            | "avi" => self.media,
            "zip" | "tar" | "gz" | "xz" | "zst" | "bz2" | "7z" | "rar" | "deb" | "rpm" => {
                self.archive
            }
            "md" | "txt" | "pdf" | "doc" | "docx" | "odt" | "rtf" | "csv" | "xlsx" | "epub" => {
                self.doc
            }
            _ => return None,
        })
    }
}

pub fn alpha(c: Rgba, a: f32) -> Rgba {
    Rgba { a, ..c }
}
