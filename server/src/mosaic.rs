//! Several photos as one picture for the card, as FxEmbed's mosaic shows them: Telegram's card
//! has room for one image, and a post's first photo alone hides that there are more. The first
//! four photos are laid out in rows and columns, the squarest arrangement that shows them at
//! similar scales, and drawn into one JPEG.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use futures_util::future::try_join_all;
use image::imageops::{self, FilterType};
use image::{ColorType, ImageDecoder, ImageReader, Limits, RgbImage};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::extract::{Media, MediaKind, Post, Target};
use crate::settings::Settings;

/// Photos in one mosaic, as FxEmbed: more would be too small to make out on a card.
const MAX_PHOTOS: usize = 4;
const GAP: u32 = 10;
/// The longest side: plenty for a card, and small enough to stay well under a megabyte.
const MAX_SIDE: u32 = 2048;
/// A photo over this is refused (the CDN's are a few hundred kilobytes).
const MAX_PHOTO_BYTES: u64 = 15 * 1024 * 1024;
/// What one decoded photo may take, its copy as RGB included: a 50-megapixel JPEG fits, a
/// decompression bomb doesn't.
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PHOTO_SIDE: u32 = 16_384;
/// How long a build waits for a slot.
const SLOT_WAIT: Duration = Duration::from_secs(30);
const QUALITY: u8 = 85;

/// Whether a card shows a mosaic: on, two photos or more, no video (which the card plays), no
/// `img_index` asking for one item, and media the card may show.
pub fn wanted(post: &Post, target: &Target, settings: &Settings) -> bool {
    settings.photo_mosaic
        && !post.sensitive
        && target.item_index().is_none()
        && post.media.iter().all(|m| m.kind == MediaKind::Photo)
        && post.media.len() >= 2
}

/// Where each photo goes, in the order given, and the whole picture's size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub tiles: Vec<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A photo, or photos side by side (a row) or one above the other (a column).
enum Node {
    Photo(usize),
    Row(Vec<Node>),
    Column(Vec<Node>),
}

use Node::{Column, Photo, Row};

/// The arrangements tried for `n` photos. Four in a single row or column are left out: too thin.
fn arrangements(n: usize) -> Vec<Node> {
    let p = Photo;
    match n {
        2 => vec![Row(vec![p(0), p(1)]), Column(vec![p(0), p(1)])],
        3 => vec![
            Row(vec![p(0), p(1), p(2)]),
            Column(vec![p(0), p(1), p(2)]),
            Row(vec![p(0), Column(vec![p(1), p(2)])]),
            Row(vec![Column(vec![p(0), p(1)]), p(2)]),
            Column(vec![p(0), Row(vec![p(1), p(2)])]),
            Column(vec![Row(vec![p(0), p(1)]), p(2)]),
        ],
        4 => vec![
            Column(vec![Row(vec![p(0), p(1)]), Row(vec![p(2), p(3)])]),
            Row(vec![Column(vec![p(0), p(1)]), Column(vec![p(2), p(3)])]),
            Row(vec![p(0), Column(vec![p(1), p(2), p(3)])]),
            Row(vec![Column(vec![p(0), p(1), p(2)]), p(3)]),
            Column(vec![p(0), Row(vec![p(1), p(2), p(3)])]),
            Column(vec![Row(vec![p(0), p(1), p(2)]), p(3)]),
        ],
        _ => Vec::new(),
    }
}

/// Width over height, gaps left out.
fn aspect(node: &Node, sizes: &[(u32, u32)]) -> f64 {
    match node {
        Photo(i) => f64::from(sizes[*i].0) / f64::from(sizes[*i].1),
        Row(children) => children.iter().map(|c| aspect(c, sizes)).sum(),
        Column(children) => 1.0 / children.iter().map(|c| 1.0 / aspect(c, sizes)).sum::<f64>(),
    }
}

/// Each photo's width in the arrangement when the whole is 1 wide, gaps left out.
fn widths(node: &Node, sizes: &[(u32, u32)], width: f64, out: &mut [f64]) {
    match node {
        Photo(i) => out[*i] = width,
        // A row shares one height, so widths go by aspect; a column shares its width.
        Row(children) => {
            let total = aspect(node, sizes);
            for child in children {
                widths(child, sizes, width * aspect(child, sizes) / total, out);
            }
        }
        Column(children) => children.iter().for_each(|child| widths(child, sizes, width, out)),
    }
}

/// Places a node in a rectangle, splitting what's left after the gaps by aspect.
fn place(node: &Node, sizes: &[(u32, u32)], area: Rect, out: &mut [Rect]) {
    let (children, along_width) = match node {
        Photo(i) => {
            out[*i] = area;
            return;
        }
        Row(children) => (children, true),
        Column(children) => (children, false),
    };
    let length = if along_width { area.width } else { area.height };
    let gaps = GAP * u32::try_from(children.len() - 1).unwrap_or(0);
    let room = f64::from(length.saturating_sub(gaps));
    // A row splits by width (aspect), a column by height (1 / aspect).
    let shares: Vec<f64> =
        children.iter().map(|c| if along_width { aspect(c, sizes) } else { 1.0 / aspect(c, sizes) }).collect();
    let total: f64 = shares.iter().sum();
    let mut offset = 0;
    for (i, (child, share)) in children.iter().zip(&shares).enumerate() {
        let last = i == children.len() - 1;
        let size = if last { length.saturating_sub(offset) } else { to_px(room * share / total) };
        let rect = if along_width {
            Rect { x: area.x + offset, y: area.y, width: size, height: area.height }
        } else {
            Rect { x: area.x, y: area.y + offset, width: area.width, height: size }
        };
        place(child, sizes, rect, out);
        offset += size + GAP;
    }
}

#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a positive size in pixels")]
fn to_px(value: f64) -> u32 {
    value.round().max(1.0) as u32
}

/// The arrangement for photos of these sizes (2 to 4): among those showing every photo at
/// close to the same scale, the squarest. No photo is enlarged, and the longest side is at most
/// [`MAX_SIDE`].
pub fn layout(sizes: &[(u32, u32)]) -> Option<Layout> {
    if !(2..=MAX_PHOTOS).contains(&sizes.len()) || sizes.iter().any(|(w, h)| *w == 0 || *h == 0) {
        return None;
    }
    // How much bigger the most enlarged photo is drawn than the most shrunk one.
    let spread = |node: &Node| {
        let mut out = vec![0.0; sizes.len()];
        widths(node, sizes, 1.0, &mut out);
        let scales: Vec<f64> = out.iter().zip(sizes).map(|(w, (pw, _))| w / f64::from(*pw)).collect();
        let (low, high) = scales.iter().fold((f64::MAX, 0.0_f64), |(l, h), s| (l.min(*s), h.max(*s)));
        (high / low, high)
    };
    let candidates = arrangements(sizes.len());
    let best_spread = candidates.iter().map(|n| spread(n).0).fold(f64::MAX, f64::min);
    let squareness = |node: &Node| aspect(node, sizes).ln().abs();
    let node = candidates
        .iter()
        .filter(|n| spread(n).0 <= best_spread + 0.5)
        .min_by(|a, b| squareness(a).total_cmp(&squareness(b)))?;
    // At width 1 the largest scale is `high`, so `1 / high` draws that photo at its own size.
    let natural = 1.0 / spread(node).1;
    let ratio = aspect(node, sizes);
    let (width, height) = (natural, natural / ratio);
    let fit = (f64::from(MAX_SIDE) / width.max(height)).min(1.0);
    let (width, height) = (to_px(width * fit), to_px(height * fit));
    let mut tiles = vec![Rect { x: 0, y: 0, width: 0, height: 0 }; sizes.len()];
    place(node, sizes, Rect { x: 0, y: 0, width, height }, &mut tiles);
    // Photos far thinner than the others can leave one no room after the gaps: no mosaic then.
    let shown = |t: &Rect| t.width > 0 && t.height > 0 && t.x + t.width <= width && t.y + t.height <= height;
    tiles.iter().all(shown).then_some(Layout { width, height, tiles })
}

/// The photos' sizes as the post gives them, when it gives them all.
fn declared(post: &Post) -> Option<Vec<(u32, u32)>> {
    post.media.iter().take(MAX_PHOTOS).map(|m| m.width.zip(m.height)).collect()
}

/// The size a card's mosaic will have, when the post gives every photo's size.
pub fn size(post: &Post) -> Option<(u32, u32)> {
    layout(&declared(post)?).map(|l| (l.width, l.height))
}

/// The photo's lighter copy when it still covers `tile` without being enlarged, else the photo.
fn file_for<'a>(media: &'a Media, tile: Option<&Rect>) -> &'a str {
    let lighter = media.smaller.as_ref().zip(tile).zip(media.width.zip(media.height));
    let Some(((smaller, tile), (width, height))) = lighter else { return &media.url };
    let Some(copy_width) = smaller.width.filter(|_| width > 0) else { return &media.url };
    let copy_height = u64::from(copy_width) * u64::from(height) / u64::from(width);
    if tile.width <= copy_width && u64::from(tile.height) <= copy_height { &smaller.url } else { &media.url }
}

/// Draws the photos into their tiles on black, each filling its tile (cropped to the middle),
/// as a JPEG. They come one at a time, so only one is ever decoded at once.
pub fn draw(photos: impl IntoIterator<Item = Result<RgbImage>>, layout: &Layout) -> Result<Vec<u8>> {
    let mut canvas = RgbImage::new(layout.width, layout.height);
    for (photo, tile) in photos.into_iter().zip(&layout.tiles) {
        let filled = fill(&photo?, tile.width, tile.height);
        imageops::replace(&mut canvas, &filled, i64::from(tile.x), i64::from(tile.y));
    }
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, QUALITY).encode_image(&canvas)?;
    Ok(jpeg)
}

/// The photo scaled to cover `width` × `height`, cut to it around the middle.
fn fill(photo: &RgbImage, width: u32, height: u32) -> RgbImage {
    let (pw, ph) = (photo.width().max(1), photo.height().max(1));
    let scale = (f64::from(width) / f64::from(pw)).max(f64::from(height) / f64::from(ph));
    // The part that shows is cut first, then scaled: never the whole photo, which could be huge
    // when the file's shape is far from the one the page announced.
    let (cw, ch) = (to_px(f64::from(width) / scale).clamp(1, pw), to_px(f64::from(height) / scale).clamp(1, ph));
    // A view of the part, scaled straight from the photo: no copy of it.
    let part = imageops::crop_imm(photo, (pw - cw) / 2, (ph - ch) / 2, cw, ch);
    // Shrinking averages pixels (fast and sharp enough); the rare enlargement interpolates.
    if scale < 1.0 {
        imageops::thumbnail(&*part, width, height)
    } else {
        imageops::resize(&*part, width, height, FilterType::Triangle)
    }
}

fn reader(bytes: &[u8]) -> Result<ImageReader<std::io::Cursor<&[u8]>>> {
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    (limits.max_image_width, limits.max_image_height) = (Some(MAX_PHOTO_SIDE), Some(MAX_PHOTO_SIDE));
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    Ok(reader)
}

/// A photo's size from its header, without decoding it.
fn dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    reader(bytes)?.into_dimensions().context("a photo couldn't be read")
}

fn decode(bytes: &[u8]) -> Result<RgbImage> {
    // The decoder keeps to the limit, but the copy as RGB is made after: it's counted here.
    let header = reader(bytes)?.into_decoder().context("a photo couldn't be read")?;
    let (width, height) = header.dimensions();
    let copy = if header.color_type() == ColorType::Rgb8 { 0 } else { u64::from(width) * u64::from(height) * 3 };
    ensure!(header.total_bytes().saturating_add(copy) <= MAX_DECODED_BYTES, "a photo is too big to draw");
    Ok(reader(bytes)?.decode().context("a photo couldn't be read")?.into_rgb8())
}

/// Built mosaics, kept for a while, and the builds running.
#[derive(Clone)]
pub struct Mosaics {
    made: moka::future::Cache<String, Bytes>,
    /// Decoding and drawing take a core each; two at a time leaves room for everything else.
    slots: Arc<Semaphore>,
}

impl Mosaics {
    pub fn new() -> Self {
        // Least recently used out first, as the post cache: a mosaic just built must be there
        // for the request that comes back for it.
        let made = moka::future::Cache::builder()
            .max_capacity(16 * 1024 * 1024)
            .eviction_policy(moka::policy::EvictionPolicy::lru())
            .weigher(|_, jpeg: &Bytes| u32::try_from(jpeg.len()).unwrap_or(u32::MAX))
            .time_to_live(Duration::from_secs(3600))
            .build();
        Self { made, slots: Arc::new(Semaphore::new(2)) }
    }

    pub async fn get(&self, id: &str) -> Option<Bytes> {
        self.made.get(id).await
    }

    /// Drops a post's mosaic, for when its photos may have changed.
    pub async fn forget(&self, id: &str) {
        self.made.invalidate(id).await;
    }

    /// The mosaic for the post with media ID `id`, built once for all who ask at the same time. A
    /// build carries on when the one who asked stops waiting, so the next finds it made; `share`
    /// (Discord's, when it asked) is held until the build ends.
    pub async fn build(
        &self,
        id: &str,
        post: &Arc<Post>,
        client: &reqwest::Client,
        share: Option<OwnedSemaphorePermit>,
    ) -> Result<Bytes, Arc<anyhow::Error>> {
        // Discord's builds take a slot only when one is free: queued, they'd keep Telegram's
        // waiting behind them.
        let slot = match share {
            Some(_) => Some(
                Arc::clone(&self.slots)
                    .try_acquire_owned()
                    .map_err(|_| Arc::new(anyhow::anyhow!("both mosaic slots are busy")))?,
            ),
            None => None,
        };
        let (this, id, post, client) = (self.clone(), id.to_string(), Arc::clone(post), client.clone());
        let build = tokio::spawn(async move {
            let _share = share;
            this.made.try_get_with(id, this.make(&post, &client, slot)).await
        });
        build.await.unwrap_or_else(|e| Err(Arc::new(anyhow::anyhow!("the mosaic's build stopped: {e}"))))
    }

    async fn make(&self, post: &Post, client: &reqwest::Client, slot: Option<OwnedSemaphorePermit>) -> Result<Bytes> {
        let declared = declared(post);
        let planned = declared.as_deref().and_then(layout);
        let urls: Vec<&str> = post
            .media
            .iter()
            .take(MAX_PHOTOS)
            .enumerate()
            .map(|(i, m)| file_for(m, planned.as_ref().and_then(|l| l.tiles.get(i))))
            .collect();
        ensure!(urls.len() >= 2, "a mosaic needs two photos or more");
        // The slot covers the downloads too, and stays with the drawing even if the request
        // gives up, so no more than two builds ever hold photos at once. A build that can't get
        // one soon is given up: whoever asked has long stopped waiting.
        let slot = match slot {
            Some(slot) => slot,
            None => tokio::time::timeout(SLOT_WAIT, Arc::clone(&self.slots).acquire_owned())
                .await
                .context("no mosaic slot came free in time")??,
        };
        let photos = try_join_all(urls.iter().map(|url| crate::video::download(client, url, MAX_PHOTO_BYTES))).await?;
        let jpeg = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let _slot = slot;
            // Laid out as the page announced it, from the post's sizes; the files' own otherwise.
            let sizes = match declared {
                Some(sizes) => sizes,
                None => photos.iter().map(|bytes| dimensions(bytes)).collect::<Result<_>>()?,
            };
            let layout = layout(&sizes).context("the photos can't be laid out")?;
            draw(photos.iter().map(|bytes| decode(bytes)), &layout)
        })
        .await??;
        Ok(Bytes::from(jpeg))
    }
}

impl Default for Mosaics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_of(sizes: &[(u32, u32)]) -> Layout {
        layout(sizes).unwrap()
    }

    fn no_overlap_and_inside(layout: &Layout) {
        for (i, a) in layout.tiles.iter().enumerate() {
            assert!(a.width > 0 && a.height > 0, "{a:?}");
            assert!(a.x + a.width <= layout.width && a.y + a.height <= layout.height, "{a:?} in {layout:?}");
            for b in &layout.tiles[i + 1..] {
                let apart =
                    a.x + a.width <= b.x || b.x + b.width <= a.x || a.y + a.height <= b.y || b.y + b.height <= a.y;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn layouts_are_square_ish_and_never_enlarge() {
        // Two portrait photos sit side by side; two wide ones stack.
        let portraits = layout(&[(1080, 1350), (1080, 1350)]).unwrap();
        assert_eq!((portraits.tiles[0].y, portraits.tiles[1].y), (0, 0), "{portraits:?}");
        let wide = layout(&[(1920, 1080), (1920, 1080)]).unwrap();
        assert_eq!((wide.tiles[0].x, wide.tiles[1].x), (0, 0), "{wide:?}");
        // Four squares make a 2×2 grid: twice the photos' size plus a gap, under the cap.
        let grid = layout(&[(1000, 1000); 4]).unwrap();
        assert_eq!((grid.width, grid.height), (2000, 2000));
        assert_eq!(grid.tiles[3], Rect { x: 1005, y: 1005, width: 995, height: 995 });
        // Big photos are fitted to the cap; small ones aren't enlarged.
        let big = layout(&[(4000, 3000), (4000, 3000), (4000, 3000)]).unwrap();
        assert!(big.width.max(big.height) <= MAX_SIDE);
        let small = layout(&[(300, 300), (300, 300)]).unwrap();
        assert!(small.width <= 600 && small.height <= 600);
        for l in [&portraits, &wide, &grid, &big, &small, &layout(&[(1080, 1920), (1920, 1080), (1000, 1000)]).unwrap()]
        {
            no_overlap_and_inside(l);
        }
        assert!(layout(&[(100, 100)]).is_none() && layout(&[(100, 0), (100, 100)]).is_none());
        // Every photo shows, or there's no mosaic.
        for sizes in [&[(1, 10_000), (10_000, 1)][..], &[(10_000, 1), (10_000, 1), (1, 1), (5, 10_000)]] {
            if let Some(l) = layout(sizes) {
                no_overlap_and_inside(&l);
            }
        }
    }

    #[test]
    fn a_lighter_copy_only_where_it_covers_its_tile() {
        let mut photo = Media::photo("https://preview.redd.it/a.jpeg?width=3024&s=1".into());
        (photo.width, photo.height) = (Some(3024), Some(4032));
        photo.smaller = Some(crate::extract::SmallerFile {
            url: "https://preview.redd.it/a.jpeg?width=1080&s=2".into(),
            bytes: None,
            width: Some(1080),
        });
        let tile = |width, height| Rect { x: 0, y: 0, width, height };
        assert_eq!(file_for(&photo, Some(&tile(1000, 1000))), "https://preview.redd.it/a.jpeg?width=1080&s=2");
        assert_eq!(file_for(&photo, Some(&tile(1080, 1440))), "https://preview.redd.it/a.jpeg?width=1080&s=2");
        assert_eq!(file_for(&photo, Some(&tile(2048, 900))), photo.url, "too narrow for a wide tile");
        assert_eq!(file_for(&photo, Some(&tile(900, 1500))), photo.url, "too short for a tall one");
        assert_eq!(file_for(&photo, None), photo.url, "no planned layout");
    }

    #[test]
    fn draws_a_jpeg_of_the_layouts_size() {
        let photos = [RgbImage::new(64, 80), RgbImage::new(80, 64), RgbImage::new(50, 50)];
        let files: Vec<Vec<u8>> = photos
            .iter()
            .map(|p| {
                let mut jpeg = Vec::new();
                image::codecs::jpeg::JpegEncoder::new(&mut jpeg).encode_image(p).unwrap();
                jpeg
            })
            .collect();
        let sizes: Vec<_> = files.iter().map(|f| dimensions(f).unwrap()).collect();
        assert_eq!(sizes, [(64, 80), (80, 64), (50, 50)], "read from the headers");
        let layout = layout(&sizes).unwrap();
        let jpeg = draw(files.iter().map(|f| decode(f)), &layout).unwrap();
        let back = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((back.width(), back.height()), (layout.width, layout.height));
        // Files whose shape differs from what the post said still fill their tiles.
        let other = layout_of(&[(80, 64), (64, 80), (90, 30)]);
        draw(photos.iter().map(|p| Ok(p.clone())), &other).unwrap();
        // A photo past the limits is refused before it's decoded.
        let huge = RgbImage::new(MAX_PHOTO_SIDE + 1, 1);
        let mut file = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut file).encode_image(&huge).unwrap();
        assert!(decode(&file).is_err());
        // Reddit's galleries hold PNGs as well.
        let mut png = Vec::new();
        RgbImage::new(20, 10).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        assert_eq!(dimensions(&png).unwrap(), (20, 10));
        assert!(decode(&png).is_ok());
    }

    #[test]
    fn a_photo_and_its_copy_as_rgb_keep_to_the_limit() {
        // Only a PNG's header: its size and colours are all the check reads.
        fn header(color_type: u8) -> Vec<u8> {
            fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
                let crc = !kind.iter().chain(data).fold(!0_u32, |mut c, b| {
                    c ^= u32::from(*b);
                    (0..8).for_each(|_| c = if c & 1 == 1 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 });
                    c
                });
                out.extend((u32::try_from(data.len()).unwrap()).to_be_bytes().iter().chain(kind).chain(data));
                out.extend(crc.to_be_bytes());
            }
            let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
            let ihdr = [&16_384_u32.to_be_bytes()[..], &4_200_u32.to_be_bytes(), &[8, color_type, 0, 0, 0]].concat();
            chunk(&mut png, b"IHDR", &ihdr);
            chunk(&mut png, b"IDAT", &[]);
            png
        }
        // Grey: 69 MB decoded, then 206 MB more as RGB.
        let grey = decode(&header(0)).unwrap_err().to_string();
        assert!(grey.contains("too big"), "{grey}");
        // RGB: 206 MB, and no copy. Past the check, the missing pixels fail it.
        let rgb = decode(&header(2)).unwrap_err().to_string();
        assert!(!rgb.contains("too big"), "{rgb}");
    }
}
