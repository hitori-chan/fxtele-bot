//! Joins Facebook's DASH files into one ordinary MP4.
//!
//! Each input is a fragmented MP4 holding one track: `ftyp`, `moov` (an empty sample table plus
//! `mvex`), `sidx`, then `moof`/`mdat` pairs. The output has the index first ("faststart"):
//! `ftyp`, `moov` with full sample tables, then one `mdat`, with the tracks interleaved in chunks
//! of about a second. Samples are copied byte for byte; nothing is decoded or re-encoded, so the
//! codec (AV1, VP9, H.264, AAC...) doesn't matter.

use std::cmp::Ordering;

use anyhow::{Context, Result, anyhow, bail, ensure};

/// Each chunk holds about this many seconds of one track.
const CHUNK_SECONDS: u64 = 1;
/// More samples than this in one fragment means a broken file (an hour of 48 kHz AAC is 170k).
const MAX_SAMPLES_PER_RUN: u32 = 1 << 22;

/// Joins `inputs`, one track each, into one MP4 of at most `limit` bytes, refused before it's
/// written when it would be bigger. Tracks keep their order: video first.
pub fn join(inputs: &[&[u8]], limit: u64) -> Result<Vec<u8>> {
    ensure!(!inputs.is_empty(), "nothing to join");
    let tracks = inputs
        .iter()
        .enumerate()
        .map(|(i, data)| Track::parse(data).with_context(|| format!("file {}", i + 1)))
        .collect::<Result<Vec<_>>>()?;
    write(&tracks, limit)
}

/// What the start of a DASH file holds: its track's handler (`vide`, `soun`) and codec
/// (`av01`, `mp4a`...). Needs only the first few kilobytes, through the `moov` box.
pub fn probe(data: &[u8]) -> Result<(String, String)> {
    let whole = Atom::whole(data);
    for atom in children(data, whole) {
        let atom = atom?;
        if &atom.kind == b"moov" {
            let init = Init::parse(data, atom)?;
            return Ok((fourcc(init.handler), fourcc(codec(data, init.stsd)?)));
        }
    }
    bail!("no moov box at the start: not an MP4 file, or a broken one")
}

// ---- Reading ----

/// A box: its type and where its header and body sit in the file.
#[derive(Debug, Clone, Copy)]
struct Atom {
    kind: [u8; 4],
    start: usize,
    body: usize,
    end: usize,
}

impl Atom {
    const fn whole(data: &[u8]) -> Self {
        Self { kind: *b"file", start: 0, body: 0, end: data.len() }
    }

    fn bytes<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.start..self.end]
    }
}

struct Children<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl Iterator for Children<'_> {
    type Item = Result<Atom>;

    fn next(&mut self) -> Option<Result<Atom>> {
        if self.pos >= self.end {
            return None;
        }
        let atom = read_atom(self.data, self.pos, self.end);
        // After a broken box nothing further can be found.
        self.pos = atom.as_ref().map_or(self.end, |a| a.end);
        Some(atom)
    }
}

fn read_atom(data: &[u8], pos: usize, end: usize) -> Result<Atom> {
    let mut r = Reader { data, pos, end };
    let size = r.u32().context("a truncated box header")?;
    let kind = r.take::<4>().context("a truncated box header")?;
    let size = match size {
        0 => end - pos,
        1 => usize::try_from(r.u64().context("a truncated box header")?).context("a box too large")?,
        n => usize::try_from(n).context("a box too large")?,
    };
    let body = r.pos;
    ensure!(size >= body - pos && size <= end - pos, "the {} box overruns its parent", fourcc(kind));
    Ok(Atom { kind, start: pos, body, end: pos + size })
}

const fn children(data: &[u8], parent: Atom) -> Children<'_> {
    Children { data, pos: parent.body, end: parent.end }
}

fn find(data: &[u8], parent: Atom, kind: &[u8; 4]) -> Result<Option<Atom>> {
    for atom in children(data, parent) {
        let atom = atom?;
        if &atom.kind == kind {
            return Ok(Some(atom));
        }
    }
    Ok(None)
}

fn need(data: &[u8], parent: Atom, kind: &[u8; 4]) -> Result<Atom> {
    find(data, parent, kind)?.with_context(|| format!("no {} box in {}", fourcc(*kind), fourcc(parent.kind)))
}

fn all(data: &[u8], parent: Atom, kind: &[u8; 4]) -> Result<Vec<Atom>> {
    children(data, parent).filter(|a| a.as_ref().map_or(true, |a| &a.kind == kind)).collect()
}

fn fourcc(kind: [u8; 4]) -> String {
    kind.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { char::from(b) } else { '?' }).collect()
}

/// Big-endian fields inside one box, bounds-checked.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Reader<'a> {
    const fn body(data: &'a [u8], atom: Atom) -> Self {
        Self { data, pos: atom.body, end: atom.end }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        ensure!(self.end - self.pos >= N, "a box is shorter than its fields");
        let mut bytes = [0; N];
        bytes.copy_from_slice(&self.data[self.pos..self.pos + N]);
        self.pos += N;
        Ok(bytes)
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        ensure!(self.end - self.pos >= n, "a box is shorter than its fields");
        self.pos += n;
        Ok(())
    }

    fn u32(&mut self) -> Result<u32> {
        self.take().map(u32::from_be_bytes)
    }

    fn u64(&mut self) -> Result<u64> {
        self.take().map(u64::from_be_bytes)
    }

    /// A full box's version and flags.
    fn full(&mut self) -> Result<(u8, u32)> {
        let word = self.u32()?;
        Ok((word.to_be_bytes()[0], word & 0x00ff_ffff))
    }
}

/// The parts of a track's init segment that are kept, plus what's needed to read fragments.
struct Init {
    movie_timescale: u32,
    mvhd: Atom,
    tkhd: Atom,
    track_id: u32,
    edits: Vec<Edit>,
    mdhd: Atom,
    timescale: u32,
    hdlr: Atom,
    handler: [u8; 4],
    /// `vmhd`, `smhd`...
    header: Atom,
    dinf: Option<Atom>,
    stsd: Atom,
    defaults: Defaults,
}

#[derive(Debug, Clone, Copy, Default)]
struct Defaults {
    duration: u32,
    size: u32,
    flags: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Edit {
    /// In the movie's timescale; 0 in fragmented files means "the rest of the media".
    duration: u64,
    /// In the track's timescale; -1 is an empty edit (a pause).
    media_time: i64,
    rate: u32,
}

impl Init {
    fn parse(data: &[u8], moov: Atom) -> Result<Self> {
        let mvhd = need(data, moov, b"mvhd")?;
        let movie_timescale = timescale(data, mvhd)?;
        let traks = all(data, moov, b"trak")?;
        ensure!(traks.len() == 1, "expected one track in the file, found {}", traks.len());
        let trak = traks[0];
        let tkhd = need(data, trak, b"tkhd")?;
        let track_id = {
            let mut r = Reader::body(data, tkhd);
            let (version, _) = r.full()?;
            r.skip(if version == 1 { 16 } else { 8 })?;
            r.u32()?
        };
        let edits = match find(data, trak, b"edts")? {
            Some(edts) => match find(data, edts, b"elst")? {
                Some(elst) => read_edits(data, elst)?,
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        let mdia = need(data, trak, b"mdia")?;
        let mdhd = need(data, mdia, b"mdhd")?;
        let track_timescale = timescale(data, mdhd)?;
        let hdlr = need(data, mdia, b"hdlr")?;
        let handler = {
            let mut r = Reader::body(data, hdlr);
            r.skip(8)?;
            r.take::<4>()?
        };
        let minf = need(data, mdia, b"minf")?;
        let header = children(data, minf)
            .filter_map(Result::ok)
            .find(|a| matches!(&a.kind, b"vmhd" | b"smhd" | b"sthd" | b"nmhd" | b"hmhd"))
            .context("no media header box (vmhd, smhd...)")?;
        let dinf = find(data, minf, b"dinf")?;
        let stbl = need(data, minf, b"stbl")?;
        let stsd = need(data, stbl, b"stsd")?;
        let mut defaults = Defaults::default();
        if let Some(mvex) = find(data, moov, b"mvex")? {
            for trex in all(data, mvex, b"trex")? {
                let mut r = Reader::body(data, trex);
                r.full()?;
                if r.u32()? == track_id {
                    r.skip(4)?; // sample description index
                    defaults = Defaults { duration: r.u32()?, size: r.u32()?, flags: r.u32()? };
                }
            }
        }
        Ok(Self {
            movie_timescale,
            mvhd,
            tkhd,
            track_id,
            edits,
            mdhd,
            timescale: track_timescale,
            hdlr,
            handler,
            header,
            dinf,
            stsd,
            defaults,
        })
    }
}

/// `mvhd` and `mdhd` share the layout up to the timescale.
fn timescale(data: &[u8], atom: Atom) -> Result<u32> {
    let mut r = Reader::body(data, atom);
    let (version, _) = r.full()?;
    r.skip(if version == 1 { 16 } else { 8 })?;
    let timescale = r.u32()?;
    ensure!(timescale > 0, "a zero timescale in {}", fourcc(atom.kind));
    Ok(timescale)
}

fn read_edits(data: &[u8], elst: Atom) -> Result<Vec<Edit>> {
    let mut r = Reader::body(data, elst);
    let (version, _) = r.full()?;
    let count = r.u32()?;
    ensure!(count <= 1024, "an edit list with {count} entries");
    (0..count)
        .map(|_| {
            let (duration, media_time) = if version == 1 {
                (r.u64()?, i64::from_be_bytes(r.take()?))
            } else {
                (u64::from(r.u32()?), i64::from(i32::from_be_bytes(r.take()?)))
            };
            Ok(Edit { duration, media_time, rate: r.u32()? })
        })
        .collect()
}

/// The first sample entry's type, e.g. `av01`.
fn codec(data: &[u8], stsd: Atom) -> Result<[u8; 4]> {
    let mut r = Reader::body(data, stsd);
    r.full()?;
    ensure!(r.u32()? > 0, "no sample description");
    r.skip(4)?;
    r.take()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
    /// Where its bytes start in the input file.
    offset: usize,
    size: u32,
    duration: u32,
    /// Composition offset: presentation time minus decode time.
    cto: i32,
    sync: bool,
}

struct Track<'a> {
    data: &'a [u8],
    init: Init,
    samples: Vec<Sample>,
    /// Decode time of the first sample (the first fragment's `tfdt`).
    lead: u64,
}

impl<'a> Track<'a> {
    fn parse(data: &'a [u8]) -> Result<Self> {
        let mut moov = None;
        let mut moofs = Vec::new();
        for atom in children(data, Atom::whole(data)) {
            let atom = atom?;
            match &atom.kind {
                b"moov" => moov = Some(atom),
                b"moof" => moofs.push(atom),
                _ => {}
            }
        }
        let init = Init::parse(data, moov.context("no moov box: not an MP4 file")?)?;
        ensure!(!moofs.is_empty(), "no fragments (moof boxes), so not a DASH file");
        let mut track = Self { data, init, samples: Vec::new(), lead: 0 };
        let mut decoded: Option<u64> = None;
        for moof in moofs {
            track.read_fragment(moof, &mut decoded)?;
        }
        ensure!(!track.samples.is_empty(), "the fragments hold no samples");
        Ok(track)
    }

    /// Adds one fragment's samples. `decoded` is the decode time reached so far.
    fn read_fragment(&mut self, moof: Atom, decoded: &mut Option<u64>) -> Result<()> {
        let data = self.data;
        for traf in all(data, moof, b"traf")? {
            let tfhd = need(data, traf, b"tfhd")?;
            let mut r = Reader::body(data, tfhd);
            let (_, flags) = r.full()?;
            if r.u32()? != self.init.track_id {
                continue;
            }
            // Without an explicit base, offsets count from the moof (default-base-is-moof, or
            // the first traf of a moof in older files).
            let base = if flags & 0x01 != 0 {
                usize::try_from(r.u64()?).context("a base data offset too large")?
            } else {
                moof.start
            };
            if flags & 0x02 != 0 {
                r.skip(4)?; // sample description index
            }
            let d = self.init.defaults;
            let duration = if flags & 0x08 != 0 { r.u32()? } else { d.duration };
            let size = if flags & 0x10 != 0 { r.u32()? } else { d.size };
            let sample_flags = if flags & 0x20 != 0 { r.u32()? } else { d.flags };

            if let Some(tfdt) = find(data, traf, b"tfdt")? {
                let mut r = Reader::body(data, tfdt);
                let (version, _) = r.full()?;
                let start = if version == 1 { r.u64()? } else { u64::from(r.u32()?) };
                match *decoded {
                    None => self.lead = start,
                    // A gap before this fragment lengthens the last sample, so the samples after
                    // it keep their times.
                    Some(reached) if start > reached => {
                        if let Some(last) = self.samples.last_mut() {
                            let gap = u32::try_from(start - reached).unwrap_or(u32::MAX);
                            last.duration = last.duration.saturating_add(gap);
                        }
                    }
                    Some(_) => {}
                }
                *decoded = Some(decoded.map_or(start, |reached| reached.max(start)));
            }

            let mut next = base;
            for trun in all(data, traf, b"trun")? {
                let mut r = Reader::body(data, trun);
                let (version, run_flags) = r.full()?;
                let count = r.u32()?;
                ensure!(count <= MAX_SAMPLES_PER_RUN, "a fragment claims {count} samples");
                let mut pos = if run_flags & 0x01 != 0 {
                    let offset = i32::from_be_bytes(r.take()?);
                    base.checked_add_signed(isize::try_from(offset)?).context("a data offset before the file")?
                } else {
                    next
                };
                let first_flags = if run_flags & 0x04 != 0 { Some(r.u32()?) } else { None };
                for i in 0..count {
                    let duration = if run_flags & 0x100 != 0 { r.u32()? } else { duration };
                    let size = if run_flags & 0x200 != 0 { r.u32()? } else { size };
                    let own_flags = if run_flags & 0x400 != 0 { Some(r.u32()?) } else { None };
                    let flags = match (i, first_flags) {
                        (0, Some(first)) => first,
                        _ => own_flags.unwrap_or(sample_flags),
                    };
                    let cto = if run_flags & 0x800 != 0 {
                        let raw = r.u32()?;
                        if version == 0 {
                            i32::try_from(raw).context("a composition offset too large")?
                        } else {
                            raw.cast_signed()
                        }
                    } else {
                        0
                    };
                    let end = pos
                        .checked_add(usize::try_from(size)?)
                        .filter(|&end| end <= data.len())
                        .context("a sample lies outside the file (a cut-off download?)")?;
                    // Real samples take some bytes each; zero-sized ones claimed by the million are
                    // a broken (or hostile) file, and each costs memory to note.
                    ensure!(self.samples.len() < data.len() / 16 + 64, "more samples than the file could hold");
                    // Bit 16 is sample_is_non_sync_sample.
                    self.samples.push(Sample { offset: pos, size, duration, cto, sync: flags & 0x0001_0000 == 0 });
                    pos = end;
                    *decoded = Some(decoded.unwrap_or(0).saturating_add(u64::from(duration)));
                }
                next = pos;
            }
        }
        Ok(())
    }

    fn duration(&self) -> u64 {
        self.samples.iter().map(|s| u64::from(s.duration)).sum()
    }

    /// How long, in `movie_timescale`, the track waits before its first sample shows: its first
    /// fragment's time, less what its edit list skips of it.
    fn delay(&self, movie_timescale: u32) -> u64 {
        let to_movie = |value: u64| rescale(value, self.init.timescale, movie_timescale);
        if self.init.edits.is_empty() {
            return to_movie(self.lead);
        }
        let first = self.init.edits.iter().find(|e| e.media_time >= 0);
        first.map_or(0, |e| to_movie(self.lead.saturating_sub(u64::try_from(e.media_time).unwrap_or(0))))
    }

    /// The edit list for the output, in `movie_timescale`, the tracks' common delay (`shared`)
    /// taken off: only how far one starts after the other is kept. Fragmented files leave the
    /// length of an edit at 0 ("all of it"), which an ordinary file must spell out.
    fn edits(&self, movie_timescale: u32, shared: u64) -> Vec<Edit> {
        let media = self.duration();
        let to_movie = |value: u64| rescale(value, self.init.timescale, movie_timescale);
        let mut edits = Vec::new();
        if self.init.edits.is_empty() {
            let pause = to_movie(self.lead).saturating_sub(shared);
            if pause > 0 {
                edits.push(Edit { duration: pause, media_time: -1, rate: 1 << 16 });
                edits.push(Edit { duration: to_movie(media), media_time: 0, rate: 1 << 16 });
            }
            return edits;
        }
        let mut shared = shared;
        for edit in &self.init.edits {
            let shown = rescale(edit.duration, self.init.movie_timescale, movie_timescale);
            if edit.media_time < 0 {
                if shown > 0 {
                    edits.push(Edit { duration: shown, ..*edit });
                }
                continue;
            }
            // The samples now start at 0 rather than at the first fragment's time, so the edit's
            // media time moves back by the lead, and what it showed of the lead, before the first
            // sample, becomes a pause: the track keeps its place against the other one.
            let start = u64::try_from(edit.media_time).unwrap_or(0);
            let pause = to_movie(self.lead.saturating_sub(start));
            let start = start.saturating_sub(self.lead);
            let waits = pause.saturating_sub(shared);
            shared = shared.saturating_sub(pause);
            if waits > 0 {
                edits.push(Edit { duration: waits, media_time: -1, rate: 1 << 16 });
            }
            let duration =
                if edit.duration == 0 { to_movie(media.saturating_sub(start)) } else { shown.saturating_sub(pause) };
            edits.push(Edit { duration, media_time: i64::try_from(start).unwrap_or(i64::MAX), rate: edit.rate });
        }
        edits
    }
}

fn rescale(value: u64, from: u32, to: u32) -> u64 {
    u64::try_from(u128::from(value) * u128::from(to) / u128::from(from.max(1))).unwrap_or(u64::MAX)
}

/// Compares two times given in different timescales.
fn earlier(a: (u64, u32), b: (u64, u32)) -> Ordering {
    (u128::from(a.0) * u128::from(b.1)).cmp(&(u128::from(b.0) * u128::from(a.1)))
}

// ---- Writing ----

/// A run of consecutive samples of one track, stored together.
#[derive(Debug, Clone, Copy)]
struct Chunk {
    track: usize,
    first: usize,
    count: usize,
}

/// Chunks in file order: always the track whose next sample is due first, about a second each.
fn interleave(tracks: &[Track<'_>]) -> Vec<Chunk> {
    let mut next = vec![0usize; tracks.len()];
    let mut time = vec![0u64; tracks.len()];
    let mut chunks = Vec::new();
    loop {
        let due = (0..tracks.len())
            .filter(|&t| next[t] < tracks[t].samples.len())
            .min_by(|&a, &b| earlier((time[a], tracks[a].init.timescale), (time[b], tracks[b].init.timescale)));
        let Some(t) = due else { break };
        let samples = &tracks[t].samples;
        let until = time[t] + CHUNK_SECONDS * u64::from(tracks[t].init.timescale);
        let first = next[t];
        while next[t] < samples.len() && (next[t] == first || time[t] < until) {
            time[t] += u64::from(samples[next[t]].duration);
            next[t] += 1;
        }
        chunks.push(Chunk { track: t, first, count: next[t] - first });
    }
    chunks
}

fn write(tracks: &[Track<'_>], limit: u64) -> Result<Vec<u8>> {
    let chunks = interleave(tracks);
    let chunk_bytes = |c: &Chunk| -> u64 {
        tracks[c.track].samples[c.first..c.first + c.count].iter().map(|s| u64::from(s.size)).sum()
    };
    let payload: u64 = chunks.iter().map(chunk_bytes).sum();
    let ftyp = ftyp(tracks)?;
    let mdat_header: u64 = if payload + 8 > u64::from(u32::MAX) { 16 } else { 8 };
    // The moov's size doesn't depend on the offsets in it (they're all 64-bit), so measure it
    // once with a placeholder base, then write it for real.
    let measured = moov(tracks, &chunks, &chunk_bytes, 0)?.len() as u64;
    let base = ftyp.len() as u64 + measured + mdat_header;
    let moov = moov(tracks, &chunks, &chunk_bytes, base)?;
    debug_assert_eq!(moov.len() as u64, measured);

    // Checked before anything is written: samples can point at the same bytes again and again,
    // so a small broken file could otherwise ask for gigabytes.
    ensure!(
        base + payload <= limit,
        "the joined video would be {} MB, over the {} MB limit",
        (base + payload).div_ceil(1024 * 1024),
        limit / (1024 * 1024)
    );
    let total = usize::try_from(base + payload).context("the video is too large")?;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    if mdat_header == 16 {
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(b"mdat");
        out.extend_from_slice(&(payload + 16).to_be_bytes());
    } else {
        out.extend_from_slice(&u32::try_from(payload + 8)?.to_be_bytes());
        out.extend_from_slice(b"mdat");
    }
    for chunk in &chunks {
        let track = &tracks[chunk.track];
        for sample in &track.samples[chunk.first..chunk.first + chunk.count] {
            out.extend_from_slice(&track.data[sample.offset..sample.offset + sample.size as usize]);
        }
    }
    debug_assert_eq!(out.len(), total);
    Ok(out)
}

/// A box from its type and body parts.
fn atom(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let size = 8 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&u32::try_from(size).expect("boxes here stay far below 4 GB").to_be_bytes());
    out.extend_from_slice(kind);
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

/// A full box: version and flags, then the fields.
fn full_atom(kind: &[u8; 4], version: u8, flags: u32, fields: &[u8]) -> Vec<u8> {
    let head = (u32::from(version) << 24 | flags).to_be_bytes();
    atom(kind, &[&head, fields])
}

fn ftyp(tracks: &[Track<'_>]) -> Result<Vec<u8>> {
    let mut brands: Vec<&[u8; 4]> = vec![b"isom", b"iso2", b"mp41"];
    for track in tracks {
        match &codec(track.data, track.init.stsd)? {
            b"av01" => brands.push(b"av01"),
            b"avc1" | b"avc3" => brands.push(b"avc1"),
            _ => {}
        }
    }
    let mut body = Vec::new();
    body.extend_from_slice(b"isom");
    body.extend_from_slice(&0x200u32.to_be_bytes());
    for brand in brands {
        body.extend_from_slice(brand);
    }
    Ok(atom(b"ftyp", &[&body]))
}

/// Writes `value` into a copied box at `at` bytes past its header, as 32 or 64 bits. Boxes come
/// from the CDN's file, so a short one is an error, never a panic.
fn patch(bytes: &mut [u8], header: usize, at: usize, value: u64, wide: bool) -> Result<()> {
    let at = header + at;
    let short = || anyhow!("a header box is too short to hold its fields");
    if wide {
        bytes.get_mut(at..at + 8).ok_or_else(short)?.copy_from_slice(&value.to_be_bytes());
    } else {
        let value = u32::try_from(value).context("a duration too long for the file's 32-bit fields")?;
        bytes.get_mut(at..at + 4).ok_or_else(short)?.copy_from_slice(&value.to_be_bytes());
    }
    Ok(())
}

/// Whether a copied full box (version, flags, then fields) uses 64-bit fields: version 1.
fn is_wide(bytes: &[u8], header: usize) -> Result<bool> {
    Ok(*bytes.get(header).context("a header box is empty")? == 1)
}

fn moov(tracks: &[Track<'_>], chunks: &[Chunk], chunk_bytes: &dyn Fn(&Chunk) -> u64, base: u64) -> Result<Vec<u8>> {
    let movie_timescale = tracks[0].init.movie_timescale;
    // Chunk offsets per track, in file order.
    let mut offsets = vec![Vec::new(); tracks.len()];
    let mut counts = vec![Vec::new(); tracks.len()];
    let mut at = base;
    for chunk in chunks {
        offsets[chunk.track].push(at);
        counts[chunk.track].push(chunk.count);
        at += chunk_bytes(chunk);
    }

    let mut traks = Vec::new();
    let mut movie_duration = 0;
    // Tracks that all start late (a piece cut from the middle of a stream) start at once.
    let shared = tracks.iter().map(|t| t.delay(movie_timescale)).min().unwrap_or(0);
    for (i, track) in tracks.iter().enumerate() {
        let edits = track.edits(movie_timescale, shared);
        let duration = if edits.is_empty() {
            rescale(track.duration(), track.init.timescale, movie_timescale)
        } else {
            edits.iter().fold(0, |sum: u64, e| sum.saturating_add(e.duration))
        };
        movie_duration = movie_duration.max(duration);
        let id = u32::try_from(i + 1)?;
        traks.push(trak(track, id, duration, &edits, &offsets[i], &counts[i])?);
    }

    let data = tracks[0].data;
    let mvhd = tracks[0].init.mvhd;
    let mut mvhd_bytes = mvhd.bytes(data).to_vec();
    let header = mvhd.body - mvhd.start;
    let wide = is_wide(&mvhd_bytes, header)?;
    patch(&mut mvhd_bytes, header, if wide { 24 } else { 16 }, movie_duration, wide)?;
    let next_id = u32::try_from(tracks.len() + 1)?;
    let last = mvhd_bytes.len().checked_sub(4).filter(|&last| last >= header).context("the mvhd box is too short")?;
    patch(&mut mvhd_bytes, 0, last, u64::from(next_id), false)?;

    let mut parts: Vec<&[u8]> = vec![&mvhd_bytes];
    parts.extend(traks.iter().map(Vec::as_slice));
    Ok(atom(b"moov", &parts))
}

fn trak(
    track: &Track<'_>,
    id: u32,
    duration: u64,
    edits: &[Edit],
    offsets: &[u64],
    counts: &[usize],
) -> Result<Vec<u8>> {
    let data = track.data;
    let init = &track.init;

    let mut tkhd = init.tkhd.bytes(data).to_vec();
    let header = init.tkhd.body - init.tkhd.start;
    let wide = is_wide(&tkhd, header)?;
    *tkhd.get_mut(header + 3).context("the tkhd box is too short")? |= 0x03; // enabled, in the movie
    patch(&mut tkhd, header, if wide { 20 } else { 12 }, u64::from(id), false)?;
    patch(&mut tkhd, header, if wide { 28 } else { 20 }, duration, wide)?;

    let mut mdhd = init.mdhd.bytes(data).to_vec();
    let header = init.mdhd.body - init.mdhd.start;
    let wide = is_wide(&mdhd, header)?;
    patch(&mut mdhd, header, if wide { 24 } else { 16 }, track.duration(), wide)?;

    let dinf = match init.dinf {
        Some(dinf) => dinf.bytes(data).to_vec(),
        // A data reference to this same file.
        None => atom(
            b"dinf",
            &[&full_atom(b"dref", 0, 0, &[&1u32.to_be_bytes()[..], &full_atom(b"url ", 0, 1, &[])].concat())],
        ),
    };
    let stbl = stbl(track, offsets, counts)?;
    let minf = atom(b"minf", &[init.header.bytes(data), &dinf, &stbl]);
    let mdia = atom(b"mdia", &[&mdhd, init.hdlr.bytes(data), &minf]);
    let edts = (!edits.is_empty()).then(|| atom(b"edts", &[&elst(edits)]));
    Ok(match edts {
        Some(edts) => atom(b"trak", &[&tkhd, &edts, &mdia]),
        None => atom(b"trak", &[&tkhd, &mdia]),
    })
}

fn elst(edits: &[Edit]) -> Vec<u8> {
    let wide = edits.iter().any(|e| e.duration > u64::from(u32::MAX) || i32::try_from(e.media_time).is_err());
    let mut fields = u32::try_from(edits.len()).unwrap_or(0).to_be_bytes().to_vec();
    for edit in edits {
        if wide {
            fields.extend_from_slice(&edit.duration.to_be_bytes());
            fields.extend_from_slice(&edit.media_time.to_be_bytes());
        } else {
            fields.extend_from_slice(&u32::try_from(edit.duration).unwrap_or(u32::MAX).to_be_bytes());
            fields.extend_from_slice(&i32::try_from(edit.media_time).unwrap_or(-1).to_be_bytes());
        }
        fields.extend_from_slice(&edit.rate.to_be_bytes());
    }
    full_atom(b"elst", u8::from(wide), 0, &fields)
}

/// Run-length pairs `(count, value)` of consecutive equal values.
fn runs(values: impl Iterator<Item = u32>) -> Vec<(u32, u32)> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for value in values {
        match runs.last_mut() {
            Some((count, last)) if *last == value => *count += 1,
            _ => runs.push((1, value)),
        }
    }
    runs
}

fn table(entries: &[(u32, u32)]) -> Vec<u8> {
    let mut fields = u32::try_from(entries.len()).unwrap_or(0).to_be_bytes().to_vec();
    for (a, b) in entries {
        fields.extend_from_slice(&a.to_be_bytes());
        fields.extend_from_slice(&b.to_be_bytes());
    }
    fields
}

fn stbl(track: &Track<'_>, offsets: &[u64], counts: &[usize]) -> Result<Vec<u8>> {
    let samples = &track.samples;
    let count = u32::try_from(samples.len())?;

    let stts = full_atom(b"stts", 0, 0, &table(&runs(samples.iter().map(|s| s.duration))));

    let ctts = samples.iter().any(|s| s.cto != 0).then(|| {
        let negative = samples.iter().any(|s| s.cto < 0);
        let entries = runs(samples.iter().map(|s| s.cto.cast_unsigned()));
        full_atom(b"ctts", u8::from(negative), 0, &table(&entries))
    });

    // Without stss every sample counts as a sync sample, as it should for audio.
    let sync: Vec<u32> = (1..=count).zip(samples).filter(|(_, s)| s.sync).map(|(n, _)| n).collect();
    let stss = (!sync.is_empty() && sync.len() < samples.len()).then(|| {
        let mut fields = u32::try_from(sync.len()).unwrap_or(0).to_be_bytes().to_vec();
        for n in &sync {
            fields.extend_from_slice(&n.to_be_bytes());
        }
        full_atom(b"stss", 0, 0, &fields)
    });

    let mut stsc_entries: Vec<(u32, u32)> = Vec::new();
    let mut last_count = None;
    for (n, &per_chunk) in (1u32..).zip(counts) {
        let per_chunk = u32::try_from(per_chunk)?;
        if last_count != Some(per_chunk) {
            stsc_entries.push((n, per_chunk));
            last_count = Some(per_chunk);
        }
    }
    let mut stsc_fields = u32::try_from(stsc_entries.len())?.to_be_bytes().to_vec();
    for (first_chunk, per_chunk) in stsc_entries {
        stsc_fields.extend_from_slice(&first_chunk.to_be_bytes());
        stsc_fields.extend_from_slice(&per_chunk.to_be_bytes());
        stsc_fields.extend_from_slice(&1u32.to_be_bytes()); // sample description index
    }
    let stsc = full_atom(b"stsc", 0, 0, &stsc_fields);

    let first_size = samples[0].size;
    let mut stsz_fields = Vec::with_capacity(8 + 4 * samples.len());
    if samples.iter().all(|s| s.size == first_size) {
        stsz_fields.extend_from_slice(&first_size.to_be_bytes());
        stsz_fields.extend_from_slice(&count.to_be_bytes());
    } else {
        stsz_fields.extend_from_slice(&0u32.to_be_bytes());
        stsz_fields.extend_from_slice(&count.to_be_bytes());
        for s in samples {
            stsz_fields.extend_from_slice(&s.size.to_be_bytes());
        }
    }
    let stsz = full_atom(b"stsz", 0, 0, &stsz_fields);

    let mut co64_fields = u32::try_from(offsets.len())?.to_be_bytes().to_vec();
    for offset in offsets {
        co64_fields.extend_from_slice(&offset.to_be_bytes());
    }
    let co64 = full_atom(b"co64", 0, 0, &co64_fields);

    let stsd = track.init.stsd.bytes(track.data);
    let mut parts: Vec<&[u8]> = vec![stsd, &stts];
    if let Some(ctts) = &ctts {
        parts.push(ctts);
    }
    if let Some(stss) = &stss {
        parts.push(stss);
    }
    parts.extend([stsc.as_slice(), &stsz, &co64]);
    Ok(atom(b"stbl", &parts))
}

#[cfg(test)]
pub(crate) mod tests {
    //! A small fragmented-MP4 writer for the inputs, and an ordinary-MP4 reader for the output.

    use super::*;

    /// One sample of a test input: size, duration, sync, composition offset.
    type TestSample = (u32, u32, bool, i32);

    struct TestTrack {
        pub handler: [u8; 4],
        pub codec: [u8; 4],
        pub timescale: u32,
        /// `(tfdt, samples)` per fragment.
        pub fragments: Vec<(u64, Vec<TestSample>)>,
        pub edits: Vec<(u32, i32)>,
        /// Put durations and sizes in `trex` instead of each sample (needs equal values).
        pub use_defaults: bool,
    }

    /// The byte at position `i` of sample `n`, so copies can be checked.
    fn sample_byte(track: u8, n: usize, i: usize) -> u8 {
        track.wrapping_mul(97).wrapping_add((n as u8).wrapping_mul(31)).wrapping_add(i as u8)
    }

    fn fragmented(t: &TestTrack, tag: u8) -> Vec<u8> {
        let mvhd = {
            let mut f = vec![0; 8];
            f.extend_from_slice(&1000u32.to_be_bytes());
            f.extend_from_slice(&[0; 4]); // duration
            f.extend_from_slice(&[0, 1, 0, 0, 1, 0]); // rate, volume
            f.extend_from_slice(&[0; 10 + 36 + 24]);
            f.extend_from_slice(&2u32.to_be_bytes());
            full_atom(b"mvhd", 0, 0, &f)
        };
        let tkhd = {
            let mut f = vec![0; 8];
            f.extend_from_slice(&1u32.to_be_bytes());
            f.extend_from_slice(&[0; 4 + 4 + 8 + 4 + 4 + 36 + 8]);
            full_atom(b"tkhd", 0, 7, &f)
        };
        let mdhd = {
            let mut f = vec![0; 8];
            f.extend_from_slice(&t.timescale.to_be_bytes());
            f.extend_from_slice(&[0; 4 + 4]);
            full_atom(b"mdhd", 0, 0, &f)
        };
        let hdlr = full_atom(b"hdlr", 0, 0, &[&[0; 4][..], &t.handler, &[0; 12], b"test\0"].concat());
        let header =
            if &t.handler == b"vide" { full_atom(b"vmhd", 0, 1, &[0; 8]) } else { full_atom(b"smhd", 0, 0, &[0; 4]) };
        let entry = atom(&t.codec, &[&[0; 8]]);
        let stsd = full_atom(b"stsd", 0, 0, &[&1u32.to_be_bytes()[..], &entry].concat());
        let empty = |kind| full_atom(kind, 0, 0, &[0; 4]);
        let stbl = atom(
            b"stbl",
            &[&stsd, &empty(b"stts"), &empty(b"stsc"), &full_atom(b"stsz", 0, 0, &[0; 8]), &empty(b"stco")],
        );
        let minf = atom(b"minf", &[&header, &stbl]);
        let mdia = atom(b"mdia", &[&mdhd, &hdlr, &minf]);
        let edts = (!t.edits.is_empty()).then(|| {
            let mut f = u32::try_from(t.edits.len()).unwrap().to_be_bytes().to_vec();
            for (duration, media_time) in &t.edits {
                f.extend_from_slice(&duration.to_be_bytes());
                f.extend_from_slice(&media_time.to_be_bytes());
                f.extend_from_slice(&0x0001_0000u32.to_be_bytes());
            }
            atom(b"edts", &[&full_atom(b"elst", 0, 0, &f)])
        });
        let trak = match &edts {
            Some(edts) => atom(b"trak", &[&tkhd, edts, &mdia]),
            None => atom(b"trak", &[&tkhd, &mdia]),
        };
        let all_samples: Vec<&TestSample> = t.fragments.iter().flat_map(|f| &f.1).collect();
        let (default_duration, default_size) =
            if t.use_defaults { (all_samples[0].1, all_samples[0].0) } else { (0, 0) };
        let trex = full_atom(
            b"trex",
            0,
            0,
            &[1u32, 1, default_duration, default_size, 0x0001_0000]
                .iter()
                .flat_map(|v| v.to_be_bytes())
                .collect::<Vec<_>>(),
        );
        let moov = atom(b"moov", &[&mvhd, &trak, &atom(b"mvex", &[&trex])]);

        let mut out = atom(b"ftyp", &[b"iso6", &[0; 4], b"iso6dash"]);
        out.extend_from_slice(&moov);
        let mut n = 0;
        for (tfdt, samples) in &t.fragments {
            let tfhd = full_atom(b"tfhd", 0, 0x02_0000, &1u32.to_be_bytes());
            let tfdt = full_atom(b"tfdt", 1, 0, &tfdt.to_be_bytes());
            let mut run_flags = 0x01 | 0x400 | 0x800;
            if !t.use_defaults {
                run_flags |= 0x100 | 0x200;
            }
            let negative = samples.iter().any(|s| s.3 < 0);
            let mut fields = u32::try_from(samples.len()).unwrap().to_be_bytes().to_vec();
            fields.extend_from_slice(&0i32.to_be_bytes()); // data offset, patched below
            for &(size, duration, sync, cto) in samples {
                if !t.use_defaults {
                    fields.extend_from_slice(&duration.to_be_bytes());
                    fields.extend_from_slice(&size.to_be_bytes());
                }
                fields.extend_from_slice(&(if sync { 0u32 } else { 0x0001_0000 }).to_be_bytes());
                fields.extend_from_slice(&cto.to_be_bytes());
            }
            let trun = full_atom(b"trun", u8::from(negative), run_flags, &fields);
            let traf = atom(b"traf", &[&tfhd, &tfdt, &trun]);
            let mut moof = atom(b"moof", &[&full_atom(b"mfhd", 0, 0, &1u32.to_be_bytes()), &traf]);
            // The data offset counts from the moof to the mdat's body.
            let data_offset = i32::try_from(moof.len() + 8).unwrap();
            let at = moof.len() - trun.len() + 12 + 4;
            moof[at..at + 4].copy_from_slice(&data_offset.to_be_bytes());
            out.extend_from_slice(&moof);
            let mut body = Vec::new();
            for &(size, ..) in samples {
                body.extend((0..size as usize).map(|i| sample_byte(tag, n, i)));
                n += 1;
            }
            out.extend_from_slice(&atom(b"mdat", &[&body]));
        }
        out
    }

    /// A track read back from an ordinary MP4.
    #[derive(Debug)]
    pub struct ReadTrack {
        pub id: u32,
        pub timescale: u32,
        pub duration: u64,
        pub tkhd_duration: u64,
        pub edits: Vec<Edit>,
        /// `(offset, size, duration, cto, sync)`
        pub samples: Vec<(u64, u32, u32, i32, bool)>,
        pub codec: [u8; 4],
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn entries(data: &[u8], atom: Atom, width: usize) -> Vec<Vec<u32>> {
        let body = &data[atom.body..atom.end];
        let count = u32_at(body, 4) as usize;
        (0..count).map(|i| (0..width).map(|j| u32_at(body, 8 + (i * width + j) * 4)).collect()).collect()
    }

    pub fn read(data: &[u8]) -> (Vec<String>, u64, Vec<ReadTrack>) {
        let top: Vec<Atom> = children(data, Atom::whole(data)).map(Result::unwrap).collect();
        let order = top.iter().map(|a| fourcc(a.kind)).collect();
        let moov = *top.iter().find(|a| &a.kind == b"moov").unwrap();
        let mvhd = need(data, moov, b"mvhd").unwrap();
        let movie_duration = u64::from(u32_at(data, mvhd.body + 16));
        assert_eq!(u32_at(data, mvhd.end - 4) as usize, all(data, moov, b"trak").unwrap().len() + 1, "next track ID");
        let mut tracks = Vec::new();
        for trak in all(data, moov, b"trak").unwrap() {
            let tkhd = need(data, trak, b"tkhd").unwrap();
            let edits = find(data, trak, b"edts")
                .unwrap()
                .map(|e| read_edits(data, need(data, e, b"elst").unwrap()).unwrap())
                .unwrap_or_default();
            let mdia = need(data, trak, b"mdia").unwrap();
            let mdhd = need(data, mdia, b"mdhd").unwrap();
            let stbl = need(data, need(data, mdia, b"minf").unwrap(), b"stbl").unwrap();
            let stts = entries(data, need(data, stbl, b"stts").unwrap(), 2);
            let ctts = find(data, stbl, b"ctts").unwrap().map(|a| entries(data, a, 2)).unwrap_or_default();
            let stss = find(data, stbl, b"stss").unwrap().map(|a| entries(data, a, 1));
            let stsc = entries(data, need(data, stbl, b"stsc").unwrap(), 3);
            let stsz = need(data, stbl, b"stsz").unwrap();
            let co64 = need(data, stbl, b"co64").unwrap();
            let co64_body = &data[co64.body..co64.end];
            let offsets: Vec<u64> = (0..u32_at(co64_body, 4) as usize)
                .map(|i| u64::from_be_bytes(co64_body[8 + i * 8..16 + i * 8].try_into().unwrap()))
                .collect();
            let stsz_body = &data[stsz.body..stsz.end];
            let (fixed, count) = (u32_at(stsz_body, 4), u32_at(stsz_body, 8) as usize);
            let sizes: Vec<u32> =
                (0..count).map(|i| if fixed != 0 { fixed } else { u32_at(stsz_body, 12 + i * 4) }).collect();
            let durations: Vec<u32> = stts.iter().flat_map(|e| std::iter::repeat_n(e[1], e[0] as usize)).collect();
            let ctos: Vec<i32> =
                ctts.iter().flat_map(|e| std::iter::repeat_n(e[1].cast_signed(), e[0] as usize)).collect();
            // Samples per chunk, from stsc's runs.
            let mut per_chunk = Vec::new();
            for (i, entry) in stsc.iter().enumerate() {
                let next_first = stsc.get(i + 1).map_or(offsets.len() as u32 + 1, |e| e[0]);
                per_chunk.extend(std::iter::repeat_n(entry[1] as usize, (next_first - entry[0]) as usize));
            }
            let mut samples = Vec::new();
            let mut n = 0;
            for (chunk, &count) in per_chunk.iter().enumerate() {
                let mut at = offsets[chunk];
                for _ in 0..count {
                    let sync = stss.as_ref().is_none_or(|s| s.iter().any(|e| e[0] as usize == n + 1));
                    samples.push((at, sizes[n], durations[n], ctos.get(n).copied().unwrap_or(0), sync));
                    at += u64::from(sizes[n]);
                    n += 1;
                }
            }
            assert_eq!(n, count, "stsc covers every sample");
            let stsd = need(data, stbl, b"stsd").unwrap();
            tracks.push(ReadTrack {
                id: u32_at(data, tkhd.body + 12),
                timescale: u32_at(data, mdhd.body + 12),
                duration: u64::from(u32_at(data, mdhd.body + 16)),
                tkhd_duration: u64::from(u32_at(data, tkhd.body + 20)),
                edits,
                samples,
                codec: codec(data, stsd).unwrap(),
            });
        }
        (order, movie_duration, tracks)
    }

    fn video() -> TestTrack {
        TestTrack {
            handler: *b"vide",
            codec: *b"av01",
            timescale: 15360,
            fragments: vec![
                (0, (0..30).map(|i| (100 + i, 512, i % 15 == 0, 0)).collect()),
                (30 * 512, (0..30).map(|i| (200 + i, 512, i % 15 == 0, 0)).collect()),
            ],
            edits: Vec::new(),
            use_defaults: false,
        }
    }

    fn audio() -> TestTrack {
        TestTrack {
            handler: *b"soun",
            codec: *b"mp4a",
            timescale: 44100,
            fragments: vec![(0, vec![(50, 1024, true, 0); 40]), (40 * 1024, vec![(50, 1024, true, 0); 40])],
            edits: vec![(0, 2048)],
            use_defaults: true,
        }
    }

    /// Every output sample's bytes are the matching input sample's.
    fn assert_copied(out: &[u8], track: &ReadTrack, tag: u8) {
        for (n, &(offset, size, ..)) in track.samples.iter().enumerate() {
            let at = usize::try_from(offset).unwrap();
            let bytes = &out[at..at + size as usize];
            assert!(bytes.iter().enumerate().all(|(i, &b)| b == sample_byte(tag, n, i)), "sample {n} of track {tag}");
        }
    }

    #[test]
    fn joins_video_and_audio() {
        let (v, a) = (fragmented(&video(), 1), fragmented(&audio(), 2));
        let out = join(&[&v, &a], u64::MAX).unwrap();
        let over = join(&[&v, &a], out.len() as u64 - 1).unwrap_err().to_string();
        assert!(over.contains("over the"), "refused before it's written: {over}");
        let (order, movie_duration, tracks) = read(&out);
        assert_eq!(order, ["ftyp", "moov", "mdat"], "the index comes first");
        assert!(out[8..32].windows(4).any(|w| w == b"av01"), "the AV1 brand");

        let (video, audio) = (&tracks[0], &tracks[1]);
        assert_eq!((video.id, audio.id), (1, 2));
        assert_eq!((video.codec, audio.codec), (*b"av01", *b"mp4a"));
        assert_eq!((video.timescale, audio.timescale), (15360, 44100));
        assert_eq!((video.samples.len(), audio.samples.len()), (60, 80));
        assert_eq!(video.duration, 60 * 512);
        assert_eq!(audio.duration, 80 * 1024);
        assert_copied(&out, video, 1);
        assert_copied(&out, audio, 2);
        assert_eq!(video.samples.iter().filter(|s| s.4).count(), 4, "keyframes kept");
        assert!(audio.samples.iter().all(|s| s.4), "audio needs no stss");

        // The audio's 2048-sample delay stays, and its length is spelled out in movie time.
        assert_eq!(audio.edits.len(), 1);
        assert_eq!(audio.edits[0].media_time, 2048);
        assert_eq!(audio.edits[0].duration, rescale(80 * 1024 - 2048, 44100, 1000));
        assert_eq!(audio.tkhd_duration, audio.edits[0].duration);
        assert_eq!(video.tkhd_duration, 2000);
        assert_eq!(movie_duration, 2000);

        // Interleaved: a second of video, then a second of audio, and so on.
        let mut chunks: Vec<(u64, u32)> =
            tracks.iter().flat_map(|t| t.samples.iter().map(move |s| (s.0, t.id))).collect();
        chunks.sort_unstable();
        chunks.dedup_by_key(|c| c.1);
        assert!(chunks.len() >= 4, "{chunks:?}");
        let mdat_end = out.len() as u64;
        assert!(tracks.iter().flat_map(|t| &t.samples).all(|s| s.0 + u64::from(s.1) <= mdat_end));
    }

    #[test]
    fn joins_files_made_by_ffmpeg() {
        // A second producer's layout: other default fields, one audio fragment, an mfra box.
        let video = include_bytes!("../../tests/fixtures/video/video.mp4");
        let audio = include_bytes!("../../tests/fixtures/video/audio.mp4");
        let out = join(&[video, audio], u64::MAX).unwrap();
        let (order, _, tracks) = read(&out);
        assert_eq!(order, ["ftyp", "moov", "mdat"]);
        for (track, input) in tracks.iter().zip([&video[..], &audio[..]]) {
            let source = Track::parse(input).unwrap();
            assert_eq!(track.samples.len(), source.samples.len());
            for (ours, theirs) in track.samples.iter().zip(&source.samples) {
                let at = usize::try_from(ours.0).unwrap();
                let end = theirs.offset + theirs.size as usize;
                assert_eq!(&out[at..at + ours.1 as usize], &input[theirs.offset..end]);
                assert_eq!((ours.2, ours.3, ours.4), (theirs.duration, theirs.cto, theirs.sync));
            }
        }
        assert_eq!((tracks[0].codec, tracks[1].codec), (*b"avc1", *b"mp4a"));
        assert!(tracks[0].samples.iter().any(|s| s.3 != 0), "B-frames keep their composition offsets");
        assert_eq!(tracks[0].samples.iter().filter(|s| s.4).count(), 4, "a keyframe every 15 frames");
    }

    #[test]
    fn keeps_composition_offsets_and_gaps() {
        let mut v = video();
        v.fragments = vec![
            (0, vec![(10, 100, true, 0), (11, 100, false, 200), (12, 100, false, -100)]),
            // 50 ticks missing before this fragment.
            (350, vec![(13, 100, true, 0)]),
        ];
        let out = join(&[&fragmented(&v, 3)], u64::MAX).unwrap();
        let (_, _, tracks) = read(&out);
        let samples = &tracks[0].samples;
        assert_eq!(samples.iter().map(|s| s.3).collect::<Vec<_>>(), [0, 200, -100, 0]);
        assert_eq!(
            samples.iter().map(|s| s.2).collect::<Vec<_>>(),
            [100, 100, 150, 100],
            "the gap joins the last sample"
        );
        assert_copied(&out, &tracks[0], 3);
    }

    #[test]
    fn a_late_start_becomes_an_empty_edit() {
        let mut a = audio();
        a.edits.clear();
        a.fragments[0].0 = 4410;
        a.fragments[1].0 = 4410 + 40 * 1024;
        let (_, _, tracks) = read(&join(&[&fragmented(&video(), 3), &fragmented(&a, 4)], u64::MAX).unwrap());
        assert!(tracks[0].edits.is_empty(), "the video starts at once");
        assert_eq!(tracks[1].edits[0], Edit { duration: 100, media_time: -1, rate: 1 << 16 });
        assert_eq!(tracks[1].edits[1].media_time, 0);
        // Both 0.1 s late, as a piece from the middle of a stream: neither waits.
        let mut v = video();
        v.fragments[0].0 = 1536;
        v.fragments[1].0 = 1536 + 30 * 512;
        let (_, duration, tracks) = read(&join(&[&fragmented(&v, 3), &fragmented(&a, 4)], u64::MAX).unwrap());
        assert!(tracks.iter().all(|t| t.edits.is_empty()), "{:?}", tracks.iter().map(|t| &t.edits).collect::<Vec<_>>());
        assert_eq!(duration, 2000, "no black before it");
        let (_, _, alone) = read(&join(&[&fragmented(&a, 4)], u64::MAX).unwrap());
        assert!(alone[0].edits.is_empty(), "nor for a lone track");
    }

    #[test]
    fn a_late_start_keeps_its_place_under_an_edit_list() {
        // Starting 4410 (0.1 s) late, with an edit skipping 2048 of priming: 0.1 s less the
        // priming is a pause, and the priming is still skipped, now counted from the first sample.
        let mut a = audio();
        a.fragments[0].0 = 4410 + 2048;
        a.fragments[1].0 = 4410 + 2048 + 40 * 1024;
        a.edits = vec![(0, 2048)];
        let (_, _, tracks) = read(&join(&[&fragmented(&video(), 3), &fragmented(&a, 4)], u64::MAX).unwrap());
        assert_eq!(tracks[1].edits[0], Edit { duration: 100, media_time: -1, rate: 1 << 16 });
        assert_eq!(tracks[1].edits[1].media_time, 0);
        // Starting before the edit's media time: the edit still skips what's left of it.
        let mut b = audio();
        b.fragments[0].0 = 1000;
        b.fragments[1].0 = 1000 + 40 * 1024;
        let (_, _, tracks) = read(&join(&[&fragmented(&video(), 3), &fragmented(&b, 5)], u64::MAX).unwrap());
        assert_eq!(tracks[1].edits.len(), 1);
        assert_eq!(tracks[1].edits[0].media_time, 1048);
    }

    #[test]
    fn probe_reads_only_the_start() {
        let v = fragmented(&video(), 1);
        let moov_end = children(&v, Atom::whole(&v)).map(Result::unwrap).find(|a| &a.kind == b"moov").unwrap().end;
        assert_eq!(probe(&v[..moov_end + 20]).unwrap(), ("vide".into(), "av01".into()), "the moof after it is cut");
        assert!(probe(&v[..moov_end - 1]).is_err(), "the moov itself is cut");
        assert!(probe(b"<html>").is_err());
    }

    #[test]
    fn refuses_broken_files() {
        let v = fragmented(&video(), 1);
        assert!(join(&[&v[..v.len() - 10]], u64::MAX).is_err(), "cut off");
        assert!(join(&[b"not an mp4 at all"], u64::MAX).is_err());
        let moov_end = {
            let whole = Atom::whole(&v);
            children(&v, whole).map(Result::unwrap).find(|a| &a.kind == b"moov").unwrap().end
        };
        let error = join(&[&v[..moov_end]], u64::MAX).unwrap_err().to_string();
        assert!(error.contains("file 1"), "{error}");
        assert!(join(&[], u64::MAX).is_err());
    }
}
