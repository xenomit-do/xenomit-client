use fontdb::{Family, Query, Stretch, Style, Weight};
use ruffle_core::backend::navigator::OwnedFuture;
use ruffle_core::backend::ui::{
    DialogLoaderError, FileDialogResult, FileFilter, FontDefinition, FullscreenError,
    LanguageIdentifier, MouseCursor, NullUiBackend, UiBackend,
};
use ruffle_core::font::{FontFileData, FontQuery};
use url::Url;

/// Ruffle's null UI cannot provide device fonts used by the game interface.
pub struct DesktopUi {
    fonts: fontdb::Database,
    fallback: NullUiBackend,
}

impl DesktopUi {
    pub fn new() -> Self {
        let mut fonts = fontdb::Database::new();
        fonts.load_system_fonts();
        fonts.set_sans_serif_family("Arial");
        fonts.set_serif_family("Times New Roman");
        fonts.set_monospace_family("Consolas");
        Self {
            fonts,
            fallback: NullUiBackend::new(),
        }
    }
}

impl UiBackend for DesktopUi {
    fn mouse_visible(&self) -> bool {
        self.fallback.mouse_visible()
    }
    fn set_mouse_visible(&mut self, visible: bool) {
        self.fallback.set_mouse_visible(visible);
    }
    fn set_mouse_cursor(&mut self, cursor: MouseCursor) {
        self.fallback.set_mouse_cursor(cursor);
    }
    fn clipboard_content(&mut self) -> String {
        self.fallback.clipboard_content()
    }
    fn set_clipboard_content(&mut self, content: String) {
        self.fallback.set_clipboard_content(content);
    }
    fn set_fullscreen(&mut self, is_full: bool) -> Result<(), FullscreenError> {
        self.fallback.set_fullscreen(is_full)
    }
    fn display_root_movie_download_failed_message(&self, invalid_swf: bool, error: String) {
        self.fallback
            .display_root_movie_download_failed_message(invalid_swf, error);
    }
    fn message(&self, message: &str) {
        self.fallback.message(message);
    }
    fn open_virtual_keyboard(&self) {
        self.fallback.open_virtual_keyboard();
    }
    fn close_virtual_keyboard(&self) {
        self.fallback.close_virtual_keyboard();
    }
    fn language(&self) -> LanguageIdentifier {
        self.fallback.language()
    }
    fn display_unsupported_video(&self, url: Url) {
        self.fallback.display_unsupported_video(url);
    }

    fn load_device_font(&self, query: &FontQuery, register: &mut dyn FnMut(FontDefinition)) {
        let fallback = match query.name.to_ascii_lowercase().as_str() {
            "_serif" => Family::Serif,
            "_typewriter" => Family::Monospace,
            _ => Family::SansSerif,
        };
        let request = Query {
            families: &[Family::Name(&query.name), fallback],
            weight: if query.is_bold {
                Weight::BOLD
            } else {
                Weight::NORMAL
            },
            stretch: Stretch::Normal,
            style: if query.is_italic {
                Style::Italic
            } else {
                Style::Normal
            },
        };
        if let Some(id) = self.fonts.query(&request) {
            self.fonts.with_face_data(id, |bytes, index| {
                register(FontDefinition::FontFile {
                    name: query.name.clone(),
                    is_bold: query.is_bold,
                    is_italic: query.is_italic,
                    data: FontFileData::new(bytes.to_vec()),
                    index,
                });
            });
        }
    }

    fn sort_device_fonts(
        &self,
        query: &FontQuery,
        register: &mut dyn FnMut(FontDefinition),
    ) -> Vec<FontQuery> {
        let mut found = false;
        self.load_device_font(query, &mut |font| {
            found = true;
            register(font);
        });
        if found {
            vec![query.clone()]
        } else {
            Vec::new()
        }
    }

    fn display_file_open_dialog(
        &mut self,
        filters: Vec<FileFilter>,
    ) -> Option<OwnedFuture<FileDialogResult, DialogLoaderError>> {
        self.fallback.display_file_open_dialog(filters)
    }
    fn display_file_open_dialog_multiple(
        &mut self,
        filters: Vec<FileFilter>,
    ) -> Option<OwnedFuture<ruffle_core::backend::ui::MultiFileDialogResult, DialogLoaderError>>
    {
        self.fallback.display_file_open_dialog_multiple(filters)
    }
    fn display_file_save_dialog(
        &mut self,
        file_name: String,
        title: String,
    ) -> Option<OwnedFuture<FileDialogResult, DialogLoaderError>> {
        self.fallback.display_file_save_dialog(file_name, title)
    }
    fn close_file_dialog(&mut self) {
        self.fallback.close_file_dialog();
    }
}
