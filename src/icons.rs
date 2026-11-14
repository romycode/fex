//! Iconos SVG propios embebidos en el binario (sin I/O en runtime).
//! GPUI los rasteriza como máscara alfa y los tiñe con `text_color`, así que siguen el tema.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString, Styled, Svg, px, svg};

macro_rules! icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub enum Icon { $($variant),* }

        impl Icon {
            pub fn path(self) -> &'static str {
                match self { $(Icon::$variant => concat!("icons/", $file, ".svg")),* }
            }
        }

        fn embedded(path: &str) -> Option<&'static [u8]> {
            match path {
                $(concat!("icons/", $file, ".svg") =>
                    Some(include_bytes!(concat!("../assets/icons/", $file, ".svg"))),)*
                _ => None,
            }
        }

        const ALL: &[&str] = &[$(concat!("icons/", $file, ".svg")),*];
    };
}

icons! {
    Folder => "folder",
    FolderLink => "folder-link",
    File => "file",
    FileCode => "file-code",
    FileText => "file-text",
    FileImage => "file-image",
    FileMedia => "file-media",
    FileArchive => "file-archive",
    Link => "link",
    Unknown => "unknown",
    Home => "home",
    Desktop => "desktop",
    Documents => "documents",
    Download => "download",
    Pictures => "pictures",
    Music => "music",
    Videos => "videos",
    System => "system",
    ArrowUp => "arrow-up",
    ChevronRight => "chevron-right",
    Eye => "eye",
    Sun => "sun",
    Moon => "moon",
    Contrast => "contrast",
    Globe => "globe",
    Copy => "copy",
    Cut => "cut",
    Paste => "paste",
    Pencil => "pencil",
    Trash => "trash",
    DeleteForever => "delete-forever",
    FolderPlus => "folder-plus",
    Refresh => "refresh",
    Open => "open",
    SelectAll => "select-all",
    Check => "check",
    Alert => "alert",
    Undo => "undo",
}

impl Icon {
    pub fn el(self, size: f32) -> Svg {
        svg().path(self.path()).size(px(size)).flex_none()
    }
}

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(embedded(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ALL
            .iter()
            .filter(|p| p.starts_with(path))
            .map(|p| SharedString::from(*p))
            .collect())
    }
}
