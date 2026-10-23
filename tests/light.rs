//! Checks the incremental engine against a from-scratch computation of
//! vanilla's light rules, on random worlds.

use std::collections::VecDeque;
use std::time::Duration;

use beacon::{BlockChanged, ChunkReplaced, Face, LayerLight, LightPlugin, LightProperties};
use chunkedge::ServerSettings;
use chunkedge::keepalive::KeepaliveSettings;
use chunkedge::prelude::*;
use chunkedge::protocol::packets::play::{LevelChunkWithLightS2c, LightUpdateS2c};
use chunkedge::testing::{MockClientHelper, add_plugins, create_mock_client};

/// World height of the test dimension, kept small so the reference is fast.
const HEIGHT: i32 = 48;
/// The world is `SIZE` x `SIZE` chunks starting at chunk (0, 0).
const SIZE: i32 = 5;

#[test]
fn matches_reference_after_loading_in_random_order() {
    let mut rng = Rng(1);
    let mut world = TestWorld::new();
    let mut order: Vec<ChunkPos> = all_chunks().collect();
    rng.shuffle(&mut order);
    // A few chunks per tick, so some get lit before their neighbours exist.
    for batch in order.chunks(3) {
        for &pos in batch {
            world.insert_chunk(pos, random_chunk(&mut rng));
        }
        world.app.update();
    }
    world.assert_matches_reference();
}

#[test]
fn matches_reference_after_block_changes() {
    let mut rng = Rng(2);
    let mut world = TestWorld::new();
    for pos in all_chunks() {
        world.insert_chunk(pos, random_chunk(&mut rng));
    }
    world.app.update();
    world.assert_matches_reference();

    for round in 0..20 {
        // Alternate single edits with batches spread over several chunks.
        let edits = if round % 2 == 0 { 1 } else { 40 };
        for _ in 0..edits {
            let pos = BlockPos::new(
                rng.below(SIZE * 16),
                rng.below(HEIGHT),
                rng.below(SIZE * 16),
            );
            world.set_block(pos, rng.block());
        }
        world.app.update();
        world.assert_matches_reference();
    }
}

#[test]
fn opening_and_closing_a_shaft() {
    let mut world = TestWorld::new();
    for pos in all_chunks() {
        let mut chunk = UnloadedChunk::with_height(HEIGHT as u32);
        for x in 0..16 {
            for z in 0..16 {
                for y in 0..32 {
                    chunk.set_block_state(x, y, z, BlockState::STONE);
                }
            }
        }
        world.insert_chunk(pos, chunk);
    }
    world.app.update();
    world.assert_matches_reference();

    // Dig a shaft into the centre chunk, then cap it with a bottom slab.
    for y in 10..32 {
        world.set_block(BlockPos::new(40, y, 40), BlockState::AIR);
    }
    world.app.update();
    world.assert_matches_reference();
    assert_eq!(world.light().sky_light(BlockPos::new(40, 10, 40)), Some(15));

    world.set_block(BlockPos::new(40, 31, 40), BlockState::OAK_SLAB);
    world.app.update();
    world.assert_matches_reference();
    assert_eq!(world.light().sky_light(BlockPos::new(40, 30, 40)), Some(0));
}

#[test]
fn replaced_chunk_heals_its_neighbours() {
    let mut rng = Rng(3);
    let mut world = TestWorld::new();
    for pos in all_chunks() {
        world.insert_chunk(pos, random_chunk(&mut rng));
    }
    world.app.update();

    let centre = ChunkPos::new(2, 2);
    world.insert_chunk(centre, random_chunk(&mut rng));
    world.app.world_mut().write_message(ChunkReplaced {
        layer: world.layer,
        pos: centre,
    });
    world.app.update();
    world.assert_matches_reference();
}

#[test]
fn light_follows_the_chunk_packet() {
    let mut rng = Rng(4);
    let mut world = TestWorld::new();
    let mut helper = world.spawn_client();
    world.insert_chunk(ChunkPos::new(0, 0), random_chunk(&mut rng));
    world.app.update();

    let frames = helper.collect_received();
    frames.assert_count::<LevelChunkWithLightS2c>(1);
    frames.assert_count::<LightUpdateS2c>(1);
    frames.assert_order::<(LevelChunkWithLightS2c, LightUpdateS2c)>();
}

struct TestWorld {
    app: App,
    layer: Entity,
}

impl TestWorld {
    fn new() -> Self {
        let mut app = App::new();
        app.insert_resource(KeepaliveSettings {
            period: Duration::MAX,
        })
        .insert_resource(ServerSettings {
            // The mock client reads uncompressed frames.
            compression_threshold: Default::default(),
            ..Default::default()
        });
        add_plugins(&mut app);
        app.add_plugins(LightPlugin);
        app.update();

        let dimension = DimensionType {
            min_y: 0,
            height: HEIGHT,
            logical_height: HEIGHT,
            has_skylight: true,
            ..Default::default()
        };
        app.world_mut()
            .resource_mut::<DimensionTypeRegistry>()
            .insert(ident!("beacon:test"), dimension);

        let world = app.world();
        let layer = (
            ChunkLayer::new(
                ident!("beacon:test"),
                world.resource::<DimensionTypeRegistry>(),
                world.resource::<BiomeRegistry>(),
                world.resource::<Server>(),
            ),
            EntityLayer::new(world.resource::<Server>()),
        );
        let layer = app.world_mut().spawn(layer).id();
        Self { app, layer }
    }

    fn spawn_client(&mut self) -> MockClientHelper {
        let (mut client, helper) = create_mock_client("test");
        client.layer.0 = self.layer;
        client.visible_chunk_layer.0 = self.layer;
        client.visible_entity_layers.0.insert(self.layer);
        let client = self.app.world_mut().spawn(client).id();
        let mut client = self.app.world_mut().entity_mut(client);
        client.get_mut::<Position>().unwrap().set([8.0, 24.0, 8.0]);
        client.get_mut::<ViewDistance>().unwrap().set(2);
        helper
    }

    fn chunk_layer(&mut self) -> Mut<'_, ChunkLayer> {
        self.app
            .world_mut()
            .get_mut::<ChunkLayer>(self.layer)
            .unwrap()
    }

    fn light(&self) -> &LayerLight {
        self.app.world().get::<LayerLight>(self.layer).unwrap()
    }

    fn insert_chunk(&mut self, pos: ChunkPos, chunk: UnloadedChunk) {
        self.chunk_layer().insert_chunk(pos, chunk);
    }

    fn set_block(&mut self, pos: BlockPos, state: BlockState) {
        self.chunk_layer().set_block(pos, state);
        let layer = self.layer;
        self.app
            .world_mut()
            .write_message(BlockChanged { layer, pos });
    }

    #[track_caller]
    fn assert_matches_reference(&self) {
        let layer = self.app.world().get::<ChunkLayer>(self.layer).unwrap();
        let expected = Reference::compute(layer);
        let light = self.light();
        let mut mismatches = Vec::new();
        for x in 0..SIZE * 16 {
            for z in 0..SIZE * 16 {
                for y in -16..HEIGHT + 16 {
                    let pos = BlockPos::new(x, y, z);
                    let (sky, block) = expected.at(pos);
                    let actual = (light.sky_light(pos), light.block_light(pos));
                    if actual != (Some(sky), Some(block)) {
                        mismatches.push(format!(
                            "{pos:?} {:?}: expected sky {sky} block {block}, got {actual:?}",
                            expected.state(pos),
                        ));
                    }
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} blocks differ from the reference, first ones:\n{}",
            mismatches.len(),
            mismatches[..mismatches.len().min(20)].join("\n")
        );
    }
}

/// Vanilla's light rules computed from scratch over the whole test world:
/// sky sources straight down each column until something stops sky light,
/// block sources at emitters, then a flood fill where entering a block costs
/// `max(1, dampening)` unless the touching faces cover the square between.
struct Reference {
    states: Vec<BlockState>,
    sky: Vec<u8>,
    block: Vec<u8>,
}

impl Reference {
    fn compute(layer: &ChunkLayer) -> Self {
        let mut states = vec![BlockState::AIR; Self::LEN];
        for x in 0..SIZE * 16 {
            for z in 0..SIZE * 16 {
                for y in 0..HEIGHT {
                    let pos = BlockPos::new(x, y, z);
                    states[Self::index(pos)] = layer.block(pos).unwrap().state;
                }
            }
        }
        let mut reference = Self {
            states,
            sky: vec![0; Self::LEN],
            block: vec![0; Self::LEN],
        };

        let mut queue = VecDeque::new();
        for x in 0..SIZE * 16 {
            for z in 0..SIZE * 16 {
                let mut above = reference.props(BlockPos::new(x, HEIGHT + 16, z));
                for y in (-16..HEIGHT + 16).rev() {
                    let pos = BlockPos::new(x, y, z);
                    let current = reference.props(pos);
                    if above.occludes(current, Face::Down) || current.dampening() > 0 {
                        break;
                    }
                    reference.sky[Self::index(pos)] = 15;
                    queue.push_back(pos);
                    above = current;
                }
            }
        }
        reference.flood(&mut queue, |reference| &mut reference.sky);

        for (i, state) in reference.states.iter().enumerate() {
            if state.luminance() > 0 {
                reference.block[i] = state.luminance();
                queue.push_back(Self::pos(i));
            }
        }
        reference.flood(&mut queue, |reference| &mut reference.block);
        reference
    }

    fn flood(&mut self, queue: &mut VecDeque<BlockPos>, light: fn(&mut Self) -> &mut Vec<u8>) {
        while let Some(pos) = queue.pop_front() {
            let level = light(self)[Self::index(pos)];
            for face in Face::ALL {
                let (dx, dy, dz) = face.offset();
                let next = BlockPos::new(pos.x + dx, pos.y + dy, pos.z + dz);
                if !Self::contains(next) || self.props(pos).occludes(self.props(next), face) {
                    continue;
                }
                let next_level = level.saturating_sub(self.props(next).dampening().max(1));
                let slot = &mut light(self)[Self::index(next)];
                if next_level > *slot {
                    *slot = next_level;
                    queue.push_back(next);
                }
            }
        }
    }

    fn at(&self, pos: BlockPos) -> (u8, u8) {
        let i = Self::index(pos);
        (self.sky[i], self.block[i])
    }

    fn state(&self, pos: BlockPos) -> BlockState {
        if Self::contains(pos) {
            self.states[Self::index(pos)]
        } else {
            BlockState::AIR
        }
    }

    fn props(&self, pos: BlockPos) -> LightProperties {
        LightProperties::of(self.state(pos))
    }

    const SIDE: i32 = SIZE * 16;
    const LIGHT_HEIGHT: i32 = HEIGHT + 32;
    const LEN: usize = (Self::SIDE * Self::SIDE * Self::LIGHT_HEIGHT) as usize;

    fn contains(pos: BlockPos) -> bool {
        (0..Self::SIDE).contains(&pos.x)
            && (0..Self::SIDE).contains(&pos.z)
            && (-16..HEIGHT + 16).contains(&pos.y)
    }

    fn index(pos: BlockPos) -> usize {
        (pos.x + Self::SIDE * (pos.z + Self::SIDE * (pos.y + 16))) as usize
    }

    fn pos(index: usize) -> BlockPos {
        let index = index as i32;
        BlockPos::new(
            index % Self::SIDE,
            index / (Self::SIDE * Self::SIDE) - 16,
            index / Self::SIDE % Self::SIDE,
        )
    }
}

fn all_chunks() -> impl Iterator<Item = ChunkPos> {
    (0..SIZE).flat_map(|x| (0..SIZE).map(move |z| ChunkPos::new(x, z)))
}

/// Hilly stone with caves, sprinkled with every kind of light-relevant block.
fn random_chunk(rng: &mut Rng) -> UnloadedChunk {
    let mut chunk = UnloadedChunk::with_height(HEIGHT as u32);
    for x in 0..16 {
        for z in 0..16 {
            let ground = 12 + rng.below(16);
            for y in 0..HEIGHT {
                let state = if y < ground && rng.below(5) != 0 {
                    BlockState::STONE
                } else if rng.below(8) == 0 {
                    rng.block()
                } else {
                    BlockState::AIR
                };
                chunk.set_block_state(x as u32, y as u32, z as u32, state);
            }
        }
    }
    chunk
}

/// A small deterministic xorshift generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: i32) -> i32 {
        (self.next() % n as u64) as i32
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i as i32 + 1) as usize);
        }
    }

    fn block(&mut self) -> BlockState {
        const BLOCKS: &[BlockState] = &[
            BlockState::AIR,
            BlockState::STONE,
            BlockState::GLASS,
            BlockState::WATER,
            BlockState::OAK_LEAVES,
            BlockState::COBWEB,
            BlockState::TINTED_GLASS,
            BlockState::GLOWSTONE,
            BlockState::TORCH,
            BlockState::SEA_LANTERN,
            BlockState::OAK_SLAB,
            BlockState::OAK_STAIRS,
            BlockState::SNOW,
            BlockState::DIRT_PATH,
        ];
        let state = BLOCKS[self.below(BLOCKS.len() as i32) as usize];
        // Vary slab halves and stair facings, whose faces differ.
        match self.below(4) {
            0 => state.set(PropName::Type, PropValue::Top),
            1 => state.set(PropName::Facing, PropValue::East),
            2 => state.set(PropName::Half, PropValue::Top),
            _ => state,
        }
    }
}
