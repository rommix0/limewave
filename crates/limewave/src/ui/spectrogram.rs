//! The spectrogram in the main view: computed on a worker thread for each version of a file's
//! audio, coloured there, and shown as texture tiles in place of the waveform lanes.

use eframe::egui::{
    self, Align2, Color32, ColorImage, FontId, Painter, Rect, Stroke, TextureHandle,
    TextureOptions, pos2,
};
use limewave_core::dsp::spectrogram::{ROWS, Spectrogram, hz_y_fraction, spectrogram};
use limewave_core::notes::{note_hz, note_name};

use super::coords::ViewTransform;
use super::waveform::{BACKGROUND, lane_rect};
use crate::render_worker::{RenderJob, RenderKey, spawn_render};
use crate::theme::colors;

/// The most columns one texture tile holds.
const TILE_COLUMNS: usize = 2048;

/// The MIDI notes of C1 to C9, the Cs labelled on the frequency axis.
const LABELLED_CS: [u8; 9] = [24, 36, 48, 60, 72, 84, 96, 108, 120];

/// A spectrogram coloured into tiles on the worker, ready to upload.
struct Tiles {
    frames: usize,
    hop: usize,
    columns: usize,
    /// Per channel, the images of its tiles from left to right.
    images: Vec<Vec<ColorImage>>,
}

/// The spectrogram picture being shown.
pub struct Picture {
    /// The [`Session::audio_version`](crate::app::Session::audio_version) it was computed for.
    pub version: u64,
    /// The length of the audio it shows, in frames.
    pub frames: usize,
    /// Frames per column.
    pub hop: usize,
    /// The number of columns across all of a channel's tiles.
    pub columns: usize,
    /// Per channel, its tiles from left to right, each up to `TILE_COLUMNS` wide and `ROWS`
    /// tall.
    tiles: Vec<Vec<TextureHandle>>,
}

impl Picture {
    /// Uploads `tiles`, computed for audio version `version`, as textures.
    fn upload(ctx: &egui::Context, tiles: Tiles, version: u64) -> Picture {
        let textures = tiles
            .images
            .into_iter()
            .enumerate()
            .map(|(channel, images)| {
                images
                    .into_iter()
                    .enumerate()
                    .map(|(i, image)| {
                        let name = format!("spectrogram-{channel}-{i}");
                        ctx.load_texture(name, image, TextureOptions::LINEAR)
                    })
                    .collect()
            })
            .collect();
        Picture {
            version,
            frames: tiles.frames,
            hop: tiles.hop,
            columns: tiles.columns,
            tiles: textures,
        }
    }
}

/// One file's spectrogram: the job computing it for an audio version, and the picture shown.
#[derive(Default)]
pub struct SpectrogramCache {
    /// The running job; its key's `settings_hash` is the audio version it computes.
    job: Option<RenderJob<Tiles>>,
    picture: Option<Picture>,
}

/// The key of the job for audio version `version`. The cache belongs to one file, so only the
/// version tells jobs apart.
fn job_key(version: u64) -> RenderKey {
    RenderKey {
        file: 0,
        effects_len: 0,
        history_cursor: 0,
        settings_hash: version,
    }
}

/// Keeps `cache` on audio version `version` of `processed`: a finished job for it becomes the
/// picture, a job for an older version is dropped unseen, and a job is started when nothing
/// for `version` is running or shown.
pub fn update(
    cache: &mut SpectrogramCache,
    ctx: &egui::Context,
    processed: &[Vec<f32>],
    rate: u32,
    version: u64,
) {
    if let Some(job) = &cache.job {
        if job.key.settings_hash != version {
            cache.job = None;
        } else if let Some(tiles) = job.poll() {
            cache.job = None;
            cache.picture = Some(Picture::upload(ctx, tiles, version));
        }
    }
    let shown = cache.picture.as_ref().is_some_and(|p| p.version == version);
    if cache.job.is_none() && !shown {
        let audio = processed.to_vec();
        cache.job = Some(spawn_render(job_key(version), ctx.clone(), move || {
            color_tiles(&spectrogram(&audio, rate))
        }));
    }
}

/// For tests: waits for the running job and makes its result the picture, as [`update`]
/// would once it finished.
pub fn block_until_ready(cache: &mut SpectrogramCache, ctx: &egui::Context) {
    let Some(job) = cache.job.take() else { return };
    if let Some(tiles) = job.wait() {
        cache.picture = Some(Picture::upload(ctx, tiles, job.key.settings_hash));
    }
}

/// The picture to show for audio of `frames` frames: the one shown last if it is that long,
/// even when it is from an older version, so it stays up after a same-length edit until the
/// new one lands. `None` means the waveform is drawn instead.
pub fn picture_for(cache: &SpectrogramCache, frames: usize) -> Option<&Picture> {
    cache.picture.as_ref().filter(|p| p.frames == frames)
}

/// The colour of spectrogram level `v`: from the view's background at 0 to lime at 255, on a
/// curve that keeps quiet levels dark.
pub fn spectro_color(v: u8) -> Color32 {
    let t = (f32::from(v) / 255.0).powf(1.5);
    BACKGROUND.lerp_to_gamma(colors::LIME, t)
}

/// Colours every channel of `sg` into tiles of up to `TILE_COLUMNS` columns.
fn color_tiles(sg: &Spectrogram) -> Tiles {
    let palette: [Color32; 256] = std::array::from_fn(|v| spectro_color(v as u8));
    let images = sg
        .channels
        .iter()
        .map(|image| {
            (0..sg.columns)
                .step_by(TILE_COLUMNS)
                .map(|first| {
                    let end = (first + TILE_COLUMNS).min(sg.columns);
                    let pixels = (0..ROWS)
                        .flat_map(|row| &image[row * sg.columns + first..row * sg.columns + end])
                        .map(|&v| palette[usize::from(v)])
                        .collect();
                    ColorImage::new([end - first, ROWS], pixels)
                })
                .collect()
        })
        .collect();
    Tiles {
        frames: sg.frames,
        hop: sg.hop,
        columns: sg.columns,
        images,
    }
}

/// Draws `picture` in the lanes of the main view `rect`, one per channel of `channels`: the
/// tiles over the frames in view, and a faint line labelled with its note at each C below the
/// Nyquist frequency of `rate`.
pub fn draw_lanes(
    painter: &Painter,
    rect: Rect,
    view: &ViewTransform,
    picture: &Picture,
    channels: usize,
    rate: u32,
) {
    let left = rect.left();
    let line = Stroke::new(1.0, Color32::from_white_alpha(24));
    let label = Color32::from_white_alpha(110);
    let nyquist = f64::from(rate) / 2.0;
    for i in 0..channels {
        let lane = lane_rect(rect, channels, i);
        if i > 0 {
            painter.hline(lane.x_range(), lane.top(), Stroke::new(1.0, colors::PANEL));
        }
        for (t, tile) in picture.tiles.get(i).into_iter().flatten().enumerate() {
            // The tile's columns start at frame `first`; its last column may run past the end
            // of the audio, so it is clipped there.
            let first = t * TILE_COLUMNS * picture.hop;
            let span = tile.size()[0] * picture.hop;
            let end = (first + span).min(picture.frames);
            let x0 = view.frame_to_x(first as f64, left).max(lane.left());
            let x1 = view.frame_to_x(end as f64, left).min(lane.right());
            if x1 <= x0 {
                continue;
            }
            let u = |x: f32| ((view.x_to_frame(x, left) - first as f64) / span as f64) as f32;
            painter.image(
                tile.id(),
                Rect::from_x_y_ranges(x0..=x1, lane.y_range()),
                Rect::from_min_max(pos2(u(x0), 0.0), pos2(u(x1), 1.0)),
                Color32::WHITE,
            );
        }
        for note in LABELLED_CS {
            let hz = note_hz(note);
            if hz >= nyquist {
                break;
            }
            let y = lane.top() + hz_y_fraction(hz, rate) * lane.height();
            painter.hline(lane.x_range(), y, line);
            painter.text(
                pos2(lane.left() + 3.0, y - 1.0),
                Align2::LEFT_BOTTOM,
                note_name(note),
                FontId::proportional(9.0),
                label,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_run_from_the_background_to_lime() {
        assert_eq!(spectro_color(0), BACKGROUND);
        assert_eq!(spectro_color(255), colors::LIME);
    }

    #[test]
    fn a_picture_of_another_length_is_not_shown() {
        let cache = SpectrogramCache {
            job: None,
            picture: Some(Picture {
                version: 3,
                frames: 1000,
                hop: 64,
                columns: 16,
                tiles: Vec::new(),
            }),
        };
        assert_eq!(picture_for(&cache, 1000).map(|p| p.version), Some(3));
        assert!(picture_for(&cache, 999).is_none());
    }

    #[test]
    fn tiles_split_the_columns_and_keep_rows_in_order() {
        let columns = TILE_COLUMNS + 5;
        let mut image = vec![0u8; ROWS * columns];
        // A full level in the top row of the last column.
        image[columns - 1] = 255;
        let sg = Spectrogram {
            columns,
            hop: 64,
            rate: 44100,
            frames: columns * 64 - 10,
            channels: vec![image],
        };
        let tiles = color_tiles(&sg);
        let sizes: Vec<_> = tiles.images[0].iter().map(|i| i.size).collect();
        assert_eq!(sizes, [[TILE_COLUMNS, ROWS], [5, ROWS]]);
        let last = &tiles.images[0][1];
        assert_eq!(last.pixels[4], colors::LIME);
        assert_eq!(last.pixels[4 + 5], BACKGROUND, "the next row down");
    }
}
