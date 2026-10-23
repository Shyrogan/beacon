//! Light storage on `ChunkLayer` entities, and the systems that keep it in
//! step with the layer's chunks.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use bevy_tasks::{ComputeTaskPool, TaskPool};
use bevy_utils::Parallel;
use chunkedge::prelude::*;

use crate::LightSystems;
use crate::engine::{CENTRE, Engine, LocalPos, Window, window_chunks};
use crate::storage::{ChunkLight, LightKind};

pub(crate) fn plugin(app: &mut App) {
    app.add_message::<BlockChanged>()
        .add_message::<ChunkReplaced>()
        .add_message::<ChunkLit>()
        .init_resource::<LightStats>()
        .add_observer(attach_layer_light)
        .add_systems(
            PostUpdate,
            (
                forget_unloaded_chunks,
                light_loaded_chunks,
                relight_changed_blocks.run_if(on_message::<BlockChanged>),
            )
                .chain()
                .in_set(LightSystems::Propagate),
        );
}

/// A block of `layer` changed, so light around it must be recomputed.
#[derive(Message, Clone, Copy, Debug)]
pub struct BlockChanged {
    pub layer: Entity,
    pub pos: BlockPos,
}

/// The chunk at `pos` was overwritten in place and must be lit again.
#[derive(Message, Clone, Copy, Debug)]
pub struct ChunkReplaced {
    pub layer: Entity,
    pub pos: ChunkPos,
}

/// The chunk at `pos` was lit from scratch: its light is readable from
/// [`LayerLight`], and clients get it in full.
#[derive(Message, Clone, Copy, Debug)]
pub struct ChunkLit {
    pub layer: Entity,
    pub pos: ChunkPos,
}

/// Wall time the light systems spent, accumulated since startup.
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct LightStats {
    pub chunks_lit: u64,
    pub chunk_time: Duration,
    /// Chunks relit after blocks changed, once per chunk per tick.
    pub relights: u64,
    pub relight_time: Duration,
}

/// The light of a `ChunkLayer`'s lit chunks, added to every layer entity.
#[derive(Component, Debug)]
pub struct LayerLight {
    chunks: HashMap<ChunkPos, ChunkLight>,
    min_y: i32,
    height: i32,
    light_sections: usize,
    has_sky: bool,
    /// Every chunk ever lit. A chunk lit again may have left stale light in
    /// its neighbours.
    lit_before: HashSet<ChunkPos>,
}

impl LayerLight {
    /// The sky light at `pos`, or `None` if its chunk isn't lit.
    pub fn sky_light(&self, pos: BlockPos) -> Option<u8> {
        if !self.has_sky {
            return Some(0);
        }
        self.get(LightKind::Sky, pos)
    }

    /// The block light at `pos`, or `None` if its chunk isn't lit.
    pub fn block_light(&self, pos: BlockPos) -> Option<u8> {
        self.get(LightKind::Block, pos)
    }

    pub(crate) fn chunk(&self, pos: ChunkPos) -> Option<&ChunkLight> {
        self.chunks.get(&pos)
    }

    pub(crate) fn chunks_mut(&mut self) -> impl Iterator<Item = &mut ChunkLight> {
        self.chunks.values_mut()
    }

    fn new(layer: &ChunkLayer, dimensions: &DimensionTypeRegistry) -> Self {
        Self {
            chunks: HashMap::new(),
            min_y: layer.min_y(),
            height: layer.height() as i32,
            light_sections: layer.height() as usize / 16 + 2,
            has_sky: dimensions[*layer.dimension_type()].has_skylight,
            lit_before: HashSet::new(),
        }
    }

    fn get(&self, kind: LightKind, pos: BlockPos) -> Option<u8> {
        let chunk = self.chunks.get(&ChunkPos::from(pos))?;
        let y = pos.y - self.min_y + 16;
        // Below the light sections is dark, above them is open sky.
        if y < 0 {
            return Some(0);
        }
        if y >= self.light_sections as i32 * 16 {
            return Some(if kind == LightKind::Sky { 15 } else { 0 });
        }
        let index = (pos.x & 15) | (pos.z & 15) << 4 | (y & 15) << 8;
        Some(chunk.get(kind, y as usize / 16, index as usize))
    }

    /// Lights the chunks at `positions`, which have no light yet, from
    /// scratch.
    fn light_chunks(
        &mut self,
        layer: &ChunkLayer,
        positions: &[ChunkPos],
        engines: &Parallel<Engine>,
    ) {
        let jobs = positions
            .iter()
            .map(|&pos| (pos, !self.lit_before.insert(pos)))
            .collect();
        self.for_each_window(layer, jobs, engines, |engine, window, lit_before| {
            engine.light_chunk(window, lit_before);
        });
    }

    /// Relights each chunk after the blocks listed with it changed.
    fn relight_chunks(
        &mut self,
        layer: &ChunkLayer,
        changes: HashMap<ChunkPos, Vec<BlockPos>>,
        engines: &Parallel<Engine>,
    ) -> usize {
        let jobs: Vec<(ChunkPos, Vec<LocalPos>)> = changes
            .into_iter()
            .filter(|(chunk, _)| self.chunks.contains_key(chunk))
            .map(|(chunk, positions)| (chunk, self.local_positions(chunk, &positions)))
            .collect();
        let relit = jobs.len();
        self.for_each_window(layer, jobs, engines, |engine, window, positions| {
            engine.relight_blocks(window, &positions);
        });
        relit
    }

    /// `positions` inside the world, in the window coordinates of `chunk`,
    /// without duplicates.
    fn local_positions(&self, chunk: ChunkPos, positions: &[BlockPos]) -> Vec<LocalPos> {
        let mut local: Vec<LocalPos> = positions
            .iter()
            .filter(|pos| (self.min_y..self.min_y + self.height).contains(&pos.y))
            .map(|pos| LocalPos {
                x: pos.x - chunk.x * 16 + 16,
                y: pos.y - self.min_y + 16,
                z: pos.z - chunk.z * 16 + 16,
            })
            .collect();
        local.sort_unstable_by_key(|pos| (pos.y, pos.z, pos.x));
        local.dedup();
        local
    }

    /// Runs `op` on the window around each job's chunk, on every core.
    ///
    /// Chunks whose coordinates are equal modulo 3 have disjoint windows, so
    /// each of those 9 classes runs at once, one class after another. That
    /// gives the same light as running them in any sequential order. Each
    /// window's light is taken out of the map while it runs, so tasks own
    /// what they write. A centre chunk without light starts dark.
    fn for_each_window<T: Send>(
        &mut self,
        layer: &ChunkLayer,
        jobs: Vec<(ChunkPos, T)>,
        engines: &Parallel<Engine>,
        op: impl Fn(&mut Engine, &mut Window, T) + Sync,
    ) {
        let (light_sections, has_sky) = (self.light_sections, self.has_sky);
        let mut classes: [Vec<(ChunkPos, T)>; 9] = Default::default();
        for (centre, job) in jobs {
            classes[(centre.x.rem_euclid(3) + 3 * centre.z.rem_euclid(3)) as usize]
                .push((centre, job));
        }

        let run = |(centre, light): &mut (ChunkPos, [Option<ChunkLight>; 9]), job: T| {
            let light = light.each_mut().map(Option::as_mut);
            let mut window = Window::new(layer, light, *centre, light_sections, has_sky);
            engines.scope(|engine| op(engine, &mut window, job));
        };

        for class in classes.into_iter().filter(|class| !class.is_empty()) {
            let mut windows: Vec<(ChunkPos, [Option<ChunkLight>; 9])> = class
                .iter()
                .map(|&(centre, _)| {
                    let mut light = window_chunks(centre).map(|pos| self.chunks.remove(&pos));
                    light[CENTRE].get_or_insert_with(|| ChunkLight::new(light_sections, has_sky));
                    (centre, light)
                })
                .collect();

            let jobs = class.into_iter().map(|(_, job)| job);
            if let [window] = &mut windows[..] {
                // A single small update isn't worth a task.
                jobs.for_each(|job| run(window, job));
            } else {
                let run = &run;
                ComputeTaskPool::get_or_init(TaskPool::default).scope(|scope| {
                    for (window, job) in windows.iter_mut().zip(jobs) {
                        scope.spawn(async move { run(window, job) });
                    }
                });
            }

            for (centre, light) in windows {
                for (pos, light) in window_chunks(centre).into_iter().zip(light) {
                    if let Some(light) = light {
                        self.chunks.insert(pos, light);
                    }
                }
            }
        }
    }
}

fn attach_layer_light(
    add: On<Add, ChunkLayer>,
    layers: Query<&ChunkLayer>,
    dimensions: Res<DimensionTypeRegistry>,
    mut commands: Commands,
) {
    if let Ok(layer) = layers.get(add.entity) {
        commands
            .entity(add.entity)
            .insert(LayerLight::new(layer, &dimensions));
    }
}

/// Drops the light of chunks that unloaded, and of replaced chunks so they
/// are lit again.
fn forget_unloaded_chunks(
    mut layers: Query<(&ChunkLayer, &mut LayerLight)>,
    mut replaced: MessageReader<ChunkReplaced>,
) {
    for message in replaced.read() {
        if let Ok((_, mut light)) = layers.get_mut(message.layer) {
            light.chunks.remove(&message.pos);
        }
    }
    for (layer, mut light) in &mut layers {
        light.chunks.retain(|&pos, _| layer.chunk(pos).is_some());
    }
}

fn light_loaded_chunks(
    mut layers: Query<(Entity, &ChunkLayer, &mut LayerLight)>,
    mut lit: MessageWriter<ChunkLit>,
    engines: Local<Parallel<Engine>>,
    mut stats: ResMut<LightStats>,
) {
    for (entity, layer, mut light) in &mut layers {
        let loaded: Vec<ChunkPos> = layer
            .chunks()
            .map(|(pos, _)| pos)
            .filter(|&pos| light.chunk(pos).is_none())
            .collect();
        if loaded.is_empty() {
            continue;
        }
        let start = Instant::now();
        light.light_chunks(layer, &loaded, &engines);
        stats.chunk_time += start.elapsed();
        stats.chunks_lit += loaded.len() as u64;
        lit.write_batch(
            loaded
                .into_iter()
                .map(|pos| ChunkLit { layer: entity, pos }),
        );
    }
}

/// Relights each chunk with changed blocks once, for all of them.
fn relight_changed_blocks(
    mut layers: Query<(&ChunkLayer, &mut LayerLight)>,
    mut changed: MessageReader<BlockChanged>,
    engines: Local<Parallel<Engine>>,
    mut stats: ResMut<LightStats>,
) {
    let mut changes: HashMap<Entity, HashMap<ChunkPos, Vec<BlockPos>>> = HashMap::new();
    for message in changed.read() {
        changes
            .entry(message.layer)
            .or_default()
            .entry(ChunkPos::from(message.pos))
            .or_default()
            .push(message.pos);
    }

    for (entity, changes) in changes {
        let Ok((layer, mut light)) = layers.get_mut(entity) else {
            continue;
        };
        let start = Instant::now();
        let relit = light.relight_chunks(layer, changes, &engines);
        stats.relight_time += start.elapsed();
        stats.relights += relit as u64;
    }
}
