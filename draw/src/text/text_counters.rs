//! `TextCounters`, the text work done since startup, see [`super::fonts::Fonts::text_counters`].

use crate::makepad_platform::makepad_micro_serde::*;

/// Text work done since startup, so two of these taken a frame or a second apart
/// give the work done in between, see [`TextCounters::since`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, SerJson)]
pub struct TextCounters {
    pub slug_glyphs_built: u64,
    /// New Slug curve and band data the atlas handed to its textures.
    pub slug_upload_bytes: u64,
    /// SDF glyphs rasterized on the UI thread.
    pub sdf_glyphs_rasterized: u64,
    pub msdf_jobs_queued: u64,
    pub msdf_jobs_completed: u64,
    /// Draws that ended with the text atlases asking to redraw everything.
    pub redraw_all_requests: u64,
    /// Glyphs the combined DrawText shader (Android, Apple and web) drew, by its texture_index:
    /// SDF, color, MSDF and Slug.
    pub glyphs_drawn: [u64; 4],
}

impl TextCounters {
    pub fn since(&self, earlier: &TextCounters) -> TextCounters {
        TextCounters {
            slug_glyphs_built: self.slug_glyphs_built.saturating_sub(earlier.slug_glyphs_built),
            slug_upload_bytes: self.slug_upload_bytes.saturating_sub(earlier.slug_upload_bytes),
            sdf_glyphs_rasterized: self.sdf_glyphs_rasterized.saturating_sub(earlier.sdf_glyphs_rasterized),
            msdf_jobs_queued: self.msdf_jobs_queued.saturating_sub(earlier.msdf_jobs_queued),
            msdf_jobs_completed: self.msdf_jobs_completed.saturating_sub(earlier.msdf_jobs_completed),
            redraw_all_requests: self.redraw_all_requests.saturating_sub(earlier.redraw_all_requests),
            glyphs_drawn: std::array::from_fn(|kind| self.glyphs_drawn[kind].saturating_sub(earlier.glyphs_drawn[kind])),
        }
    }
}
