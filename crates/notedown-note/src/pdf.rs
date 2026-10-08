//! PDF output: one page per note page, each page one gray image.

use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref, TextStr};

use crate::error::NoteError;
use crate::notebook::Notebook;
use crate::render::RenderOptions;

/// Changes whenever the output of [`Notebook::render_pdf`] or [`Notebook::render_page`]
/// changes, so a store of rendered files knows to render them again.
pub const RENDERER_ID: &str = "2026-10-08";

fn dim(n: u32) -> Result<i32, NoteError> {
    i32::try_from(n).map_err(|_| NoteError::Decode("page too large for a PDF".into()))
}

impl Notebook {
    /// Pixels per inch of the device that wrote the note: 226 for the A5 X, 300 for the
    /// Nomad, the Manta and anything else (docs/note-format.md 4.6 and 9.4).
    pub fn ppi(&self) -> u32 {
        match self.equipment.as_deref() {
            Some("A5X") => 226,
            _ => 300,
        }
    }

    /// The note as a PDF: one page per note page, in order, each the image of
    /// [`Notebook::render_page`] as 8-bit gray with transparent pixels white. The page size is
    /// the pixel size at [`Notebook::ppi`]. No text layer. The same note gives the same bytes.
    pub fn render_pdf(&self, opts: &RenderOptions) -> Result<Vec<u8>, NoteError> {
        let mut pdf = Pdf::new();
        let catalog = Ref::new(1);
        let tree = Ref::new(2);
        let info = Ref::new(3);
        let mut next = 4;
        let mut alloc = || {
            let r = Ref::new(next);
            next += 1;
            r
        };
        let count = self.pages.len();
        let ids: Vec<(Ref, Ref, Ref)> = (0..count).map(|_| (alloc(), alloc(), alloc())).collect();
        pdf.catalog(catalog).pages(tree);
        pdf.pages(tree)
            .kids(ids.iter().map(|(page, _, _)| *page))
            .count(i32::try_from(count).map_err(|_| NoteError::Decode("too many pages".into()))?);
        let scale = 72.0 / f64::from(self.ppi());
        for (index, (page_id, image_id, content_id)) in ids.into_iter().enumerate() {
            let img = self.render_page(index, opts)?;
            let (w, h) = img.dimensions();
            let gray: Vec<u8> = img
                .pixels()
                .map(|p| if p.0[3] == 0 { 255 } else { p.0[0] })
                .collect();
            let packed = miniz_oxide::deflate::compress_to_vec_zlib(&gray, 6);
            let mut image = pdf.image_xobject(image_id, &packed);
            image.filter(Filter::FlateDecode);
            image.width(dim(w)?).height(dim(h)?).bits_per_component(8);
            image.color_space().device_gray();
            image.finish();
            let pw = (f64::from(w) * scale) as f32;
            let ph = (f64::from(h) * scale) as f32;
            let mut content = Content::new();
            content.save_state();
            content.transform([pw, 0.0, 0.0, ph, 0.0, 0.0]);
            content.x_object(Name(b"Im0"));
            content.restore_state();
            pdf.stream(content_id, &content.finish());
            let mut page = pdf.page(page_id);
            page.parent(tree)
                .media_box(Rect::new(0.0, 0.0, pw, ph))
                .contents(content_id);
            page.resources().x_objects().pair(Name(b"Im0"), image_id);
            page.finish();
        }
        pdf.document_info(info)
            .producer(TextStr(concat!("notedown ", env!("CARGO_PKG_VERSION"))));
        Ok(pdf.finish())
    }
}
