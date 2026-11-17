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
use image::{DynamicImage, RgbImage};
use tokio::sync::Semaphore;

use crate::extract::{MediaKind, Post, Target};
use crate::settings::Settings;

/// Photos in one mosaic, as FxEmbed: more would be too small to make out on a card.
const MAX_PHOTOS: usize = 4;
const GAP: u32 = 10;
/// The longest side: plenty for a card, and small enough to stay well under a megabyte.
const MAX_SIDE: u32 = 2048;
/// A photo over this is refused (the CDN's are a few hundred kilobytes).
const MAX_PHOTO_BYTES: u64 = 15 * 1024 * 1024;
const QUALITY: u8 = 85;

/// Whether a card shows a mosaic: on, two photos or more, no video (which the card plays), no
/// `img_index` asking for one item, and media the card may show.
pub fn wanted(post: &Post, target: &Target, settings: &Settings) -> bool {
    settings.photo_mosaic
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

/// Draws the photos into their tiles on black, each filling its tile (cropped to the middle),
/// as a JPEG.
pub fn draw(photos: &[DynamicImage], layout: &Layout) -> Result<Vec<u8>> {
    let mut canvas = RgbImage::new(layout.width, layout.height);
    for (photo, tile) in photos.iter().zip(&layout.tiles) {
        let filled = fill(photo, tile.width, tile.height);
        imageops::replace(&mut canvas, &filled, i64::from(tile.x), i64::from(tile.y));
    }
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, QUALITY).encode_image(&canvas)?;
    Ok(jpeg)
}

/// The photo scaled to cover `width` × `height`, cut to it around the middle.
fn fill(photo: &DynamicImage, width: u32, height: u32) -> RgbImage {
    let (pw, ph) = (photo.width().max(1), photo.height().max(1));
    let scale = (f64::from(width) / f64::from(pw)).max(f64::from(height) / f64::from(ph));
    // The part that shows is cut first, then scaled: never the whole photo, which could be huge
    // when the file's shape is far from the one the page announced.
    let (cw, ch) = (to_px(f64::from(width) / scale).clamp(1, pw), to_px(f64::from(height) / scale).clamp(1, ph));
    let part = photo.crop_imm((pw - cw) / 2, (ph - ch) / 2, cw, ch).to_rgb8();
    // Shrinking averages pixels (fast and sharp enough); the rare enlargement interpolates.
    if scale < 1.0 {
        imageops::thumbnail(&part, width, height)
    } else {
        imageops::resize(&part, width, height, FilterType::Triangle)
    }
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
        let made = moka::future::Cache::builder()
            .max_capacity(16 * 1024 * 1024)
            .weigher(|_, jpeg: &Bytes| u32::try_from(jpeg.len()).unwrap_or(u32::MAX))
            .time_to_live(Duration::from_secs(3600))
            .build();
        Self { made, slots: Arc::new(Semaphore::new(2)) }
    }

    pub async fn get(&self, id: &str) -> Option<Bytes> {
        self.made.get(id).await
    }

    /// The mosaic for the post with media ID `id`, built once for all who ask at the same time. A
    /// build carries on when the one who asked stops waiting, so the next finds it made.
    pub async fn build(
        &self,
        id: &str,
        post: &Arc<Post>,
        client: &reqwest::Client,
    ) -> Result<Bytes, Arc<anyhow::Error>> {
        let (this, id, post, client) = (self.clone(), id.to_string(), Arc::clone(post), client.clone());
        let build = tokio::spawn(async move { this.made.try_get_with(id, this.make(&post, &client)).await });
        build.await.unwrap_or_else(|e| Err(Arc::new(anyhow::anyhow!("the mosaic's build stopped: {e}"))))
    }

    async fn make(&self, post: &Post, client: &reqwest::Client) -> Result<Bytes> {
        let urls: Vec<&str> = post.media.iter().take(MAX_PHOTOS).map(|m| m.url.as_str()).collect();
        ensure!(urls.len() >= 2, "a mosaic needs two photos or more");
        // The slot covers the downloads too, and stays with the drawing even if the request
        // gives up, so no more than two builds ever hold photos at once.
        let slot = Arc::clone(&self.slots).acquire_owned().await?;
        let photos = try_join_all(urls.iter().map(|url| fetch(client, url))).await?;
        let declared = declared(post);
        let jpeg = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let _slot = slot;
            let photos = photos
                .iter()
                .map(|bytes| image::load_from_memory(bytes).context("a photo couldn't be read"))
                .collect::<Result<Vec<_>>>()?;
            // Laid out as the page announced it, from the post's sizes; the files' own otherwise.
            let sizes = declared.unwrap_or_else(|| photos.iter().map(|p| (p.width(), p.height())).collect());
            draw(&photos, &layout(&sizes).context("the photos can't be laid out")?)
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

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Bytes> {
    let response = client.get(url).send().await.map_err(|e| anyhow::anyhow!("{}", crate::net::describe(e)))?;
    ensure!(response.status().is_success(), "the CDN answered {}", response.status());
    ensure!(response.content_length().is_none_or(|n| n <= MAX_PHOTO_BYTES), "a photo is too big");
    let bytes = response.bytes().await?;
    ensure!(bytes.len() as u64 <= MAX_PHOTO_BYTES, "a photo is too big");
    Ok(bytes)
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
    fn draws_a_jpeg_of_the_layouts_size() {
        let photos = [DynamicImage::new_rgb8(64, 80), DynamicImage::new_rgb8(80, 64), DynamicImage::new_rgb8(50, 50)];
        let sizes: Vec<_> = photos.iter().map(|p| (p.width(), p.height())).collect();
        let layout = layout(&sizes).unwrap();
        let jpeg = draw(&photos, &layout).unwrap();
        let back = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((back.width(), back.height()), (layout.width, layout.height));
        // Files whose shape differs from what the post said still fill their tiles.
        let other = layout_of(&[(80, 64), (64, 80), (90, 30)]);
        draw(&photos, &other).unwrap();
    }
}
