//! Starlight's light propagation, run over a 3x3 window of chunks.
//!
//! Light travels at most 15 blocks, so whatever happens in the centre chunk
//! never reaches past its 8 neighbours. Each operation borrows those 9 chunks
//! and works in window-local coordinates: x and z in `0..48`, y in light
//! sections counted from the one below the world.
//!
//! The propagation itself is a breadth-first flood fill over two queues of
//! packed entries, with the same rules as vanilla: light loses
//! `max(1, dampening)` of the block it enters, conditionally opaque blocks
//! (slabs, stairs, ...) block it when their touching faces cover the square
//! between two blocks, and sky light passes straight down without loss through
//! blocks that don't dampen it.

use chunkedge::prelude::{BlockState, Chunk, ChunkLayer, ChunkPos, LoadedChunk};

use crate::properties::{Face, LightProperties, faces_occlude};
use crate::storage::{ChunkLight, LightKind, Nibbles};

/// The queues of the flood fill, kept between operations to reuse their
/// allocations.
#[derive(Default)]
pub struct Engine {
    increase: Vec<u64>,
    decrease: Vec<u64>,
}

impl Engine {
    /// Lights the window's centre chunk from scratch.
    ///
    /// A chunk lit for the first time can only add light to its neighbours,
    /// since none of theirs went through it, so their border light is just
    /// pulled in. A chunk lit before may have left light behind in them, so
    /// their borders are checked both ways.
    pub fn light_chunk(&mut self, window: &mut Window, lit_before: bool) {
        let scan = window.scan_centre();
        if window.has_sky {
            self.light_sky(window, scan.highest_non_empty, lit_before);
        }
        self.light_block(window, &scan.emitters, lit_before);
    }

    /// Updates light after the blocks at `positions`, all in the window's
    /// centre chunk, changed.
    pub fn relight_blocks(&mut self, window: &mut Window, positions: &[LocalPos]) {
        if window.has_sky {
            self.relight_sky_columns(window, positions);
            for &pos in positions {
                self.check_block(window, LightKind::Sky, pos);
            }
            self.decrease(window, LightKind::Sky);
        }
        for &pos in positions {
            self.check_block(window, LightKind::Block, pos);
        }
        self.decrease(window, LightKind::Block);
    }

    fn light_block(&mut self, window: &mut Window, emitters: &[LocalPos], lit_before: bool) {
        for &pos in emitters {
            let props = LightProperties::of(window.block(pos));
            let emission = props.emission();
            if emission > window.light(LightKind::Block, pos).unwrap_or(15) {
                window.set_light(LightKind::Block, pos, emission);
                let flags = sided_flag(props);
                self.increase
                    .push(encode(pos, emission, ALL_DIRECTIONS, flags));
            }
        }
        self.finish_chunk(
            window,
            LightKind::Block,
            0..window.light_sections,
            lit_before,
        );
    }

    /// Runs the queued increases and settles the centre chunk's borders.
    fn finish_chunk(
        &mut self,
        window: &mut Window,
        kind: LightKind,
        sections: std::ops::Range<usize>,
        lit_before: bool,
    ) {
        if lit_before {
            self.increase(window, kind);
            self.check_edges(window, kind, sections);
        } else {
            self.pull_neighbours(window, kind, sections);
            self.increase(window, kind);
        }
    }

    /// Queues the light on the neighbours' side of the centre chunk's
    /// borders, flowing into the centre.
    fn pull_neighbours(
        &mut self,
        window: &Window,
        kind: LightKind,
        sections: std::ops::Range<usize>,
    ) {
        for (direction, neighbour) in HORIZONTAL_NEIGHBOURS {
            if !window.is_lit(neighbour) {
                continue;
            }
            let (dx, _, dz) = OFFSETS[direction as usize];
            for section in sections.clone() {
                if window.section_is_dark(kind, neighbour, section) {
                    continue;
                }
                for y in (section * 16) as i32..(section * 16 + 16) as i32 {
                    for along in 0..16 {
                        let inside = centre_border(direction, along, y);
                        let outside = LocalPos {
                            x: inside.x + dx,
                            y,
                            z: inside.z + dz,
                        };
                        let level = window.light(kind, outside).unwrap_or(0);
                        if level > 1 {
                            self.increase
                                .push(encode(outside, level, 1 << (direction ^ 1), SIDED));
                        }
                    }
                }
            }
        }
    }

    /// Sections above the highest non-empty one are fully lit. Below that,
    /// every column is lit straight down until something blocks it, and the
    /// flood fill spreads from there.
    fn light_sky(
        &mut self,
        window: &mut Window,
        highest_non_empty: Option<usize>,
        lit_before: bool,
    ) {
        let first_open = highest_non_empty.map_or(0, |section| section + 1);
        for section in first_open..window.light_sections {
            window.fill_centre(LightKind::Sky, section, 15);
        }

        // Light the open sections' neighbours sideways, unless they are
        // open too.
        for (direction, neighbour) in HORIZONTAL_NEIGHBOURS {
            if !window.is_lit(neighbour) {
                continue;
            }
            for section in first_open..window.light_sections {
                if window.section_level(LightKind::Sky, neighbour, section) == Some(15) {
                    continue;
                }
                for y in (section * 16) as i32..(section * 16 + 16) as i32 {
                    for along in 0..16 {
                        let pos = centre_border(direction, along, y);
                        self.increase.push(encode(pos, 15, 1 << direction, 0));
                    }
                }
            }
        }

        if let Some(section) = highest_non_empty {
            let top = (section * 16 + 15) as i32;
            let mut stops = [0; 256];
            for (column, stop) in stops.iter_mut().enumerate() {
                let (x, z) = column_pos(column);
                *stop = Self::light_column(window, x, top, z);
            }
            self.queue_exposed_sources(window, top, &stops);
        }

        self.finish_chunk(window, LightKind::Sky, 0..first_open, lit_before);
    }

    /// Sets every block in the column, from `top` down, to full sky light
    /// until one blocks sky light, provided the block above `top` is itself
    /// lit by the sky. Returns the y of the first block that isn't a source,
    /// or -1 when the column is open to the bottom.
    fn light_column(window: &mut Window, x: i32, top: i32, z: i32) -> i32 {
        let above_top = LocalPos { x, y: top + 1, z };
        if window.light(LightKind::Sky, above_top).unwrap_or(15) != 15 {
            return top;
        }

        let mut above = LightProperties::of(window.block(above_top));
        let mut y = top;
        while y >= 0 {
            let pos = LocalPos { x, y, z };
            let from_face = above.occlusion(Face::Down);
            let current = LightProperties::of(window.block(pos));
            if faces_occlude(from_face, current.occlusion(Face::Up)) || current.dampening() > 0 {
                break;
            }
            window.set_light(LightKind::Sky, pos, 15);
            above = current;
            y -= 1;
        }
        y
    }

    /// Queues the sky sources of the centre's columns that have something
    /// darker beside or below them, only in those directions. A source
    /// surrounded by other sources can't light anything, and on open ground
    /// those are nearly all of them.
    fn queue_exposed_sources(&mut self, window: &Window, top: i32, stops: &[i32; 256]) {
        for (column, &stop) in stops.iter().enumerate() {
            let (x, z) = column_pos(column);
            let on_border = !(17..31).contains(&x) || !(17..31).contains(&z);
            // Inside the chunk, nothing beside the column is darker above its
            // neighbours' sources.
            let highest = if on_border {
                top
            } else {
                (0..4).fold(stop + 1, |highest, direction| {
                    let (dx, _, dz) = OFFSETS[direction];
                    highest.max(stops[column_index(x + dx, z + dz)])
                })
            };
            for y in stop + 1..=highest.min(top) {
                let pos = LocalPos { x, y, z };
                let mut directions = if y == stop + 1 { 1 << DOWN } else { 0 };
                for (direction, (dx, _, dz)) in OFFSETS[..4].iter().enumerate() {
                    let (nx, nz) = (x + dx, z + dz);
                    let darker = if (16..32).contains(&nx) && (16..32).contains(&nz) {
                        y <= stops[column_index(nx, nz)]
                    } else {
                        window
                            .light(LightKind::Sky, LocalPos { x: nx, y, z: nz })
                            .is_some_and(|level| level < 15)
                    };
                    if darker {
                        directions |= 1 << direction;
                    }
                }
                if directions != 0 {
                    let flags = sided_flag(LightProperties::of(window.block(pos)));
                    self.increase.push(encode(pos, 15, directions, flags));
                }
            }
        }
    }

    /// Before the usual per-block checks, extends sky sources down through
    /// newly opened columns and removes the ones under new obstructions.
    fn relight_sky_columns(&mut self, window: &mut Window, positions: &[LocalPos]) {
        let mut highest = [None::<i32>; 256];
        for pos in positions {
            let column = &mut highest[column_index(pos.x, pos.z)];
            *column = Some(column.map_or(pos.y, |y| y.max(pos.y)));
        }

        for (column, top) in highest.into_iter().enumerate() {
            let Some(top) = top else { continue };
            let (x, z) = column_pos(column);
            let stop = Self::light_column(window, x, top, z);
            for y in stop + 1..=top {
                let pos = LocalPos { x, y, z };
                let flags = sided_flag(LightProperties::of(window.block(pos)));
                self.increase.push(encode(pos, 15, ALL_BUT_UP, flags));
            }
            for y in (0..=stop).rev() {
                let pos = LocalPos { x, y, z };
                if window.light(LightKind::Sky, pos) != Some(15) {
                    break;
                }
                self.decrease.push(encode(pos, 15, ALL_BUT_UP, 0));
                window.set_light(LightKind::Sky, pos, 0);
            }
        }
    }

    /// Queues the work to bring one block's light up to date: re-adds it as a
    /// source if it is one, and removes the light it may have passed on.
    fn check_block(&mut self, window: &mut Window, kind: LightKind, pos: LocalPos) {
        let Some(current) = window.light(kind, pos) else {
            return;
        };
        match kind {
            LightKind::Block => {
                let props = LightProperties::of(window.block(pos));
                let emission = props.emission();
                window.set_light(kind, pos, emission);
                if emission != 0 {
                    let flags = sided_flag(props);
                    self.increase
                        .push(encode(pos, emission, ALL_DIRECTIONS, flags));
                }
            }
            // Sky sources are only removed by the column pass.
            LightKind::Sky if current == 15 => {
                self.increase.push(encode(pos, 15, ALL_DIRECTIONS, SIDED));
            }
            LightKind::Sky => window.set_light(kind, pos, 0),
        }
        // The decrease also re-propagates every lit neighbour, which covers
        // blocks that let more light through than before.
        self.decrease.push(encode(pos, current, ALL_DIRECTIONS, 0));
    }

    /// Finds blocks on either side of the centre chunk's borders whose light
    /// disagrees with their neighbours, and rechecks them.
    fn check_edges(
        &mut self,
        window: &mut Window,
        kind: LightKind,
        sections: std::ops::Range<usize>,
    ) {
        let mut wrong = Vec::new();
        for (direction, neighbour) in HORIZONTAL_NEIGHBOURS {
            if !window.is_lit(neighbour) {
                continue;
            }
            for section in sections.clone() {
                if window.section_pair_is_uniform(kind, neighbour, section) {
                    continue;
                }
                for y in (section * 16) as i32..(section * 16 + 16) as i32 {
                    for along in 0..16 {
                        let inside = centre_border(direction, along, y);
                        let (dx, _, dz) = OFFSETS[direction as usize];
                        let outside = LocalPos {
                            x: inside.x + dx,
                            y,
                            z: inside.z + dz,
                        };
                        for pos in [inside, outside] {
                            let stored = window.light(kind, pos).unwrap_or(0);
                            if self.calculate_light(window, kind, pos, stored) != stored {
                                wrong.push(pos);
                            }
                        }
                    }
                }
            }
        }
        for pos in wrong {
            self.check_block(window, kind, pos);
        }
        self.decrease(window, kind);
    }

    /// The light `pos` should have given its neighbours, with Starlight's
    /// early exits: a result above `expect` is only a lower bound.
    fn calculate_light(&self, window: &Window, kind: LightKind, pos: LocalPos, expect: u8) -> u8 {
        if kind == LightKind::Sky && expect == 15 {
            return 15;
        }
        let slot = Slot::from(pos);
        let props = LightProperties::of(window.block_at(slot));
        let mut level = match kind {
            LightKind::Block => props.emission(),
            LightKind::Sky => 0,
        };
        if level >= 14 || level > expect {
            return level;
        }
        let opacity = props.dampening().max(1);
        if opacity >= 15 {
            return level;
        }

        for direction in 0..6 {
            let neighbour = slot.step(direction);
            let neighbour_level = window.light_at(kind, neighbour).unwrap_or(0);
            if neighbour_level <= level + 1 {
                continue;
            }
            let face = FACES[direction as usize];
            let neighbour_props = LightProperties::of(window.block_at(neighbour));
            if props.occludes(neighbour_props, face) {
                continue;
            }
            level = level.max(neighbour_level.saturating_sub(opacity));
            if level > expect {
                return level;
            }
        }
        level
    }

    /// Floods light outwards from the queued entries. Each entry carries the
    /// level at its position and the directions still worth trying.
    fn increase(&mut self, window: &mut Window, kind: LightKind) {
        let mut read = 0;
        while read < self.increase.len() {
            let entry = self.increase[read];
            read += 1;
            let slot = decode_slot(entry);
            let level = decode_level(entry);

            if entry & RECHECK_LEVEL != 0 {
                // Something lowered this block since it was queued.
                if window.light_at(kind, slot) != Some(level) {
                    continue;
                }
            } else if entry & WRITE_LEVEL != 0 {
                window.set_at(kind, slot, level);
            }

            let from = (entry & SIDED != 0).then(|| LightProperties::of(window.block_at(slot)));
            for direction in directions(entry) {
                let face = FACES[direction as usize];
                let from_face = from.and_then(|props| props.occlusion(face));
                if faces_occlude(from_face, None) {
                    continue;
                }
                let target = slot.step(direction);
                let Some(current) = window.light_at(kind, target) else {
                    continue;
                };
                if current + 1 >= level {
                    continue;
                }

                let props = LightProperties::of(window.block_at(target));
                if faces_occlude(from_face, props.occlusion(face.opposite())) {
                    continue;
                }
                let new_level = level.saturating_sub(props.dampening().max(1));
                if new_level <= current {
                    continue;
                }
                window.set_at(kind, target, new_level);
                if new_level > 1 {
                    let directions = ALL_DIRECTIONS ^ (1 << (direction ^ 1));
                    self.increase
                        .push(encode(target, new_level, directions, sided_flag(props)));
                }
            }
        }
        self.increase.clear();
    }

    /// Removes the light that queued entries passed on, then floods back in
    /// from whatever light remains around the hole.
    fn decrease(&mut self, window: &mut Window, kind: LightKind) {
        let mut read = 0;
        while read < self.decrease.len() {
            let entry = self.decrease[read];
            read += 1;
            let slot = decode_slot(entry);
            let level = decode_level(entry);

            let from = (entry & SIDED != 0).then(|| LightProperties::of(window.block_at(slot)));
            for direction in directions(entry) {
                let face = FACES[direction as usize];
                let from_face = from.and_then(|props| props.occlusion(face));
                if faces_occlude(from_face, None) {
                    continue;
                }
                let target = slot.step(direction);
                let current = match window.light_at(kind, target) {
                    None | Some(0) => continue,
                    Some(current) => current,
                };

                let props = LightProperties::of(window.block_at(target));
                if faces_occlude(from_face, props.occlusion(face.opposite())) {
                    continue;
                }
                let flags = sided_flag(props);
                let passed_on = level.saturating_sub(props.dampening().max(1));
                if current > passed_on {
                    // Lit by something else: let it flood back afterwards.
                    self.increase.push(encode(
                        target,
                        current,
                        ALL_DIRECTIONS,
                        RECHECK_LEVEL | flags,
                    ));
                    continue;
                }
                if kind == LightKind::Block && props.emission() != 0 {
                    self.increase.push(encode(
                        target,
                        props.emission(),
                        ALL_DIRECTIONS,
                        WRITE_LEVEL | flags,
                    ));
                }
                window.set_at(kind, target, 0);
                if passed_on > 0 {
                    let directions = ALL_DIRECTIONS ^ (1 << (direction ^ 1));
                    self.decrease
                        .push(encode(target, passed_on, directions, flags));
                }
            }
        }
        self.decrease.clear();
        self.increase(window, kind);
    }
}

/// A block position in window coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// The chunks around one centre chunk, borrowed for one operation.
///
/// Sections are laid out like Starlight's caches: a 5x5 grid of section
/// columns with the 3x3 window in the middle, and one more section below and
/// above the light sections. The ring around the window holds nothing, so
/// stepping out of the window lands on a section without light instead of
/// needing a bounds check.
pub struct Window<'a> {
    /// The light of each section by kind, indexed like [`Slot::section`],
    /// moved out of the chunks for the operation so that reading a level is
    /// two loads. `None` around the window and for chunks that aren't loaded
    /// or not lit yet: light neither enters nor leaves them.
    light: [Vec<Option<Nibbles>>; 2],
    /// The chunk holding each section's blocks, `None` outside the world.
    blocks: Vec<Option<&'a LoadedChunk>>,
    centre_blocks: Option<&'a LoadedChunk>,
    /// Bitsets of the sections whose light changed, by kind, marked dirty
    /// when the window drops.
    changed: [Vec<u64>; 2],
    /// The lit chunks, which get their light back when the window drops.
    chunks: [Option<&'a mut ChunkLight>; 9],
    light_sections: usize,
    has_sky: bool,
}

/// A window position as stored: the section, as `column + LAYER * (light
/// section + 1)` with `column` in the 5x5 grid, above the index
/// `x | z << 4 | y << 8` in the section. Queue entries hold slots, so the flood
/// fill steps between neighbours without going back to coordinates.
#[derive(Clone, Copy)]
struct Slot(u32);

impl Slot {
    #[inline]
    fn section(self) -> usize {
        (self.0 >> 12) as usize
    }

    #[inline]
    fn index(self) -> usize {
        (self.0 & 0xFFF) as usize
    }

    /// The neighbour in `direction`. Within a section that's one add; on its
    /// edge the add also moves to the next section.
    #[inline]
    fn step(self, direction: u8) -> Self {
        let direction = direction as usize;
        let delta = if self.0 & EDGE_MASK[direction] == EDGE[direction] {
            CROSS[direction]
        } else {
            STEP[direction]
        };
        Self(self.0.wrapping_add(delta))
    }
}

impl From<LocalPos> for Slot {
    #[inline]
    fn from(pos: LocalPos) -> Self {
        let column = (pos.x >> 4) + 1 + 5 * ((pos.z >> 4) + 1);
        let section = column + LAYER as i32 * ((pos.y >> 4) + 1);
        let index = (pos.x & 15) | ((pos.z & 15) << 4) | ((pos.y & 15) << 8);
        Self(index as u32 | (section as u32) << 12)
    }
}

// Per direction: the bits of a section index that sit on its edge in that
// direction, the step inside a section, and the step out of it.
const EDGE_MASK: [u32; 6] = [0x00F, 0x00F, 0x0F0, 0x0F0, 0xF00, 0xF00];
const EDGE: [u32; 6] = [0x00F, 0, 0x0F0, 0, 0xF00, 0];
const STEP: [u32; 6] = [
    1,
    1u32.wrapping_neg(),
    16,
    16u32.wrapping_neg(),
    256,
    256u32.wrapping_neg(),
];
const CROSS: [u32; 6] = [
    (1 << 12) - 15,
    15u32.wrapping_sub(1 << 12),
    (5 << 12) - 240,
    240u32.wrapping_sub(5 << 12),
    ((LAYER as u32) << 12) - 3840,
    3840u32.wrapping_sub((LAYER as u32) << 12),
];

/// Section indices between one light section and the next: the 25 columns,
/// padded so that finding the light section is a shift.
const LAYER: usize = 32;

/// The column of window chunk `chunk` in the 5x5 grid.
fn grid_column(chunk: usize) -> usize {
    chunk % 3 + 1 + 5 * (chunk / 3 + 1)
}

/// The section index of light section `section` of window chunk `chunk`.
fn grid_section(chunk: usize, section: usize) -> usize {
    grid_column(chunk) + LAYER * (section + 1)
}

impl<'a> Window<'a> {
    /// The window around `centre`, given the light of
    /// [`window_chunks`]`(centre)`. The centre chunk must be lit.
    pub fn new(
        layer: &'a ChunkLayer,
        light: [Option<&'a mut ChunkLight>; 9],
        centre: ChunkPos,
        light_sections: usize,
        has_sky: bool,
    ) -> Self {
        let len = LAYER * (light_sections + 2);
        let mut tables: [Vec<Option<Nibbles>>; 2] = [vec![None; len], vec![None; len]];
        let mut blocks = vec![None; len];
        let mut light = light;
        let chunks = window_chunks(centre).map(|pos| layer.chunk(pos));
        for (chunk, light) in light.iter_mut().enumerate() {
            if let Some(light) = light {
                for (kind, table) in [LightKind::Sky, LightKind::Block]
                    .into_iter()
                    .zip(&mut tables)
                {
                    for (section, nibbles) in light.sections_mut(kind).iter_mut().enumerate() {
                        let nibbles = std::mem::replace(nibbles, Nibbles::uniform(0));
                        table[grid_section(chunk, section)] = Some(nibbles);
                    }
                }
            }
            if let Some(blocks_chunk) = chunks[chunk] {
                // World sections start at light section 1.
                for section in 1..light_sections - 1 {
                    blocks[grid_section(chunk, section)] = Some(blocks_chunk);
                }
            }
        }
        let words = len.div_ceil(64);
        let window = Self {
            light: tables,
            blocks,
            centre_blocks: chunks[CENTRE],
            changed: [vec![0; words], vec![0; words]],
            chunks: light,
            light_sections,
            has_sky,
        };
        debug_assert!(window.is_lit(CENTRE), "the centre chunk must be lit");
        window
    }

    fn is_lit(&self, chunk: usize) -> bool {
        self.chunks[chunk].is_some()
    }

    /// The block in `slot`. The light sections below and above the world
    /// hold air.
    #[inline]
    fn block_at(&self, slot: Slot) -> BlockState {
        let section = slot.section();
        match self.blocks[section] {
            Some(chunk) => {
                let index = slot.index() as u32;
                chunk.block_state(
                    index & 15,
                    (section / LAYER - 2) as u32 * 16 + (index >> 8),
                    (index >> 4) & 15,
                )
            }
            None => BlockState::AIR,
        }
    }

    #[inline]
    fn block(&self, pos: LocalPos) -> BlockState {
        self.block_at(pos.into())
    }

    /// The light in `slot`, or `None` outside lit chunks.
    #[inline]
    fn light_at(&self, kind: LightKind, slot: Slot) -> Option<u8> {
        let nibbles = self.light[kind as usize][slot.section()].as_ref()?;
        Some(nibbles.get(slot.index()))
    }

    #[inline]
    fn set_at(&mut self, kind: LightKind, slot: Slot, level: u8) {
        let section = slot.section();
        let Some(nibbles) = &mut self.light[kind as usize][section] else {
            return;
        };
        let changed = nibbles.set(slot.index(), level);
        self.changed[kind as usize][section / 64] |= (changed as u64) << (section % 64);
    }

    /// The light at `pos`, or `None` outside lit chunks and the light
    /// sections.
    #[inline]
    fn light(&self, kind: LightKind, pos: LocalPos) -> Option<u8> {
        self.light_at(kind, pos.into())
    }

    #[inline]
    fn set_light(&mut self, kind: LightKind, pos: LocalPos, level: u8) {
        self.set_at(kind, pos.into(), level);
    }

    fn fill_centre(&mut self, kind: LightKind, section: usize, level: u8) {
        let section = grid_section(CENTRE, section);
        let Some(nibbles) = &mut self.light[kind as usize][section] else {
            return;
        };
        if nibbles.uniform_level() != Some(level) {
            *nibbles = Nibbles::uniform(level);
            self.changed[kind as usize][section / 64] |= 1 << (section % 64);
        }
    }

    fn section(&self, kind: LightKind, chunk: usize, section: usize) -> Option<&Nibbles> {
        self.light[kind as usize][grid_section(chunk, section)].as_ref()
    }

    /// Whether `section` of `chunk` is uniformly too dark to light anything.
    fn section_is_dark(&self, kind: LightKind, chunk: usize, section: usize) -> bool {
        self.section(kind, chunk, section)
            .is_none_or(|nibbles| nibbles.uniform_level().is_some_and(|level| level <= 1))
    }

    /// The level of every block in `section` of `chunk`, if they all have
    /// the same.
    fn section_level(&self, kind: LightKind, chunk: usize, section: usize) -> Option<u8> {
        self.section(kind, chunk, section)?.uniform_level()
    }

    /// Whether `section` holds one same level in the centre chunk and in
    /// `neighbour`, in which case no block on their border can be wrong.
    fn section_pair_is_uniform(&self, kind: LightKind, neighbour: usize, section: usize) -> bool {
        let centre = self.section_level(kind, CENTRE, section);
        centre.is_some() && self.section_level(kind, neighbour, section) == centre
    }

    /// Finds the centre chunk's light emitters and its highest light section
    /// holding anything but air, reading blocks in storage order.
    fn scan_centre(&self) -> CentreScan {
        let chunk = self.centre_blocks.expect("the centre chunk is loaded");
        let mut scan = CentreScan {
            emitters: Vec::new(),
            highest_non_empty: None,
        };
        for section in 0..chunk.height() / 16 {
            let mut empty = true;
            for y in section * 16..section * 16 + 16 {
                for z in 0..16 {
                    for x in 0..16 {
                        let state = chunk.block_state(x, y, z);
                        if state.is_air() {
                            continue;
                        }
                        empty = false;
                        if LightProperties::of(state).emission() != 0 {
                            scan.emitters.push(LocalPos {
                                x: x as i32 + 16,
                                y: y as i32 + 16,
                                z: z as i32 + 16,
                            });
                        }
                    }
                }
            }
            if !empty {
                scan.highest_non_empty = Some(section as usize + 1);
            }
        }
        scan
    }
}

impl Drop for Window<'_> {
    /// Gives the chunks their light back, and marks the sections that
    /// changed dirty.
    fn drop(&mut self) {
        for (chunk, light) in self.chunks.iter_mut().enumerate() {
            let Some(light) = light else { continue };
            for (kind, table) in [LightKind::Sky, LightKind::Block]
                .into_iter()
                .zip(&mut self.light)
            {
                for (section, nibbles) in light.sections_mut(kind).iter_mut().enumerate() {
                    if let Some(moved) = table[grid_section(chunk, section)].take() {
                        *nibbles = moved;
                    }
                }
            }
        }
        for (kind, changed) in [LightKind::Sky, LightKind::Block]
            .into_iter()
            .zip(&self.changed)
        {
            for (word, &bits) in changed.iter().enumerate() {
                let mut bits = bits;
                while bits != 0 {
                    let section = word * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let (column, light_section) = (section % LAYER, section / LAYER - 1);
                    let chunk = column % 5 - 1 + 3 * (column / 5 - 1);
                    if let Some(light) = self.chunks[chunk].as_deref_mut() {
                        light.mark_dirty(kind, light_section);
                    }
                }
            }
        }
    }
}

struct CentreScan {
    emitters: Vec<LocalPos>,
    highest_non_empty: Option<usize>,
}

/// The chunks of the window around `centre`, north-west first, rows of x.
pub fn window_chunks(centre: ChunkPos) -> [ChunkPos; 9] {
    std::array::from_fn(|i| {
        ChunkPos::new(centre.x + (i % 3) as i32 - 1, centre.z + (i / 3) as i32 - 1)
    })
}

/// The centre chunk's index in [`window_chunks`].
pub const CENTRE: usize = 4;

/// Each horizontal direction with the window index of the chunk it leads to.
const HORIZONTAL_NEIGHBOURS: [(u8, usize); 4] = [(0, 5), (1, 3), (2, 7), (3, 1)];

/// The centre chunk's border block facing `direction`, `along` its 16 blocks.
fn centre_border(direction: u8, along: i32, y: i32) -> LocalPos {
    let (x, z) = match direction {
        0 => (31, 16 + along),
        1 => (16, 16 + along),
        2 => (16 + along, 31),
        _ => (16 + along, 16),
    };
    LocalPos { x, y, z }
}

/// The window position of a centre column, `0..256` as `x | z << 4`.
fn column_pos(column: usize) -> (i32, i32) {
    (16 + (column as i32 & 15), 16 + (column as i32 >> 4))
}

fn column_index(x: i32, z: i32) -> usize {
    ((x - 16) | ((z - 16) << 4)) as usize
}

// Directions in Starlight's order: +X, -X, +Z, -Z, +Y, -Y. The opposite of
// direction `d` is `d ^ 1`.
const OFFSETS: [(i32, i32, i32); 6] = [
    (1, 0, 0),
    (-1, 0, 0),
    (0, 0, 1),
    (0, 0, -1),
    (0, 1, 0),
    (0, -1, 0),
];
const FACES: [Face; 6] = [
    Face::East,
    Face::West,
    Face::South,
    Face::North,
    Face::Up,
    Face::Down,
];
const ALL_DIRECTIONS: u8 = 0b11_1111;
const DOWN: u8 = 5;
const ALL_BUT_UP: u8 = ALL_DIRECTIONS ^ (1 << 4);

// A queue entry packs its slot in the low 28 bits, then the level (4) from bit
// 28, the directions to try (6) from bit 32, and flags in the top bits.
/// Set the entry's level at its position before propagating.
const WRITE_LEVEL: u64 = 1 << 61;
/// Only propagate if the position still holds the entry's level.
const RECHECK_LEVEL: u64 = 1 << 62;
/// The block at the entry's position is conditionally opaque, so its faces
/// must be checked when leaving it.
const SIDED: u64 = 1 << 63;

fn sided_flag(props: LightProperties) -> u64 {
    if props.is_conditionally_opaque() {
        SIDED
    } else {
        0
    }
}

#[inline]
fn encode(slot: impl Into<Slot>, level: u8, directions: u8, flags: u64) -> u64 {
    slot.into().0 as u64 | (level as u64) << 28 | (directions as u64) << 32 | flags
}

#[inline]
fn decode_slot(entry: u64) -> Slot {
    Slot(entry as u32 & 0xFFF_FFFF)
}

#[inline]
fn decode_level(entry: u64) -> u8 {
    ((entry >> 28) & 15) as u8
}

#[inline]
fn directions(entry: u64) -> impl Iterator<Item = u8> {
    let mut mask = (entry >> 32) & 63;
    std::iter::from_fn(move || {
        (mask != 0).then(|| {
            let direction = mask.trailing_zeros() as u8;
            mask &= mask - 1;
            direction
        })
    })
}
