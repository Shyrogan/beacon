//! Measures beacon on the cases Starlight publishes numbers for, in
//! instructions retired, which unlike wall time doesn't depend on what else
//! the machine is doing.
//!
//! ```sh
//! cargo run --profile profiling --example bench_light -- <world dir> <chunk x> <chunk z> [threads]
//! ```
//!
//! - Lighting real chunks from scratch: a 32x32 area of the world.
//! - LightBench's flat world (bedrock at y = 0, grass at y = 254): breaking
//!   and placing a grass block, so sky light falls down to y = 0, and placing
//!   and breaking glowstone on the bedrock.
//!
//! Counts cover every thread Bevy runs systems on, so they include the cost
//! of a tick. Block updates subtract the instructions of an idle tick.

use std::path::PathBuf;
use std::sync::Mutex;

use beacon::{BlockChanged, LightPlugin, LightStats};
use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};
use chunkedge::anvil::parsing::DimensionFolder;
use chunkedge::prelude::*;
use chunkedge::testing::add_plugins;
use perf_event::Counter;
use perf_event::events::Hardware;

/// Half the side of the area of real chunks lit.
const RADIUS: i32 = 16;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [world, x, z, rest @ ..] = &args[..] else {
        eprintln!("usage: bench_light <world dir> <chunk x> <chunk z> [threads]");
        return;
    };
    // One thread by default: more only adds the executor's spinning to the
    // counts.
    let threads = rest
        .first()
        .map_or(1, |threads| threads.parse().expect("thread count"));
    count_this_thread();
    ComputeTaskPool::get_or_init(|| {
        TaskPoolBuilder::new()
            .num_threads(threads)
            .on_thread_spawn(count_this_thread)
            .build()
    });
    let centre = ChunkPos::new(x.parse().unwrap(), z.parse().unwrap());
    println!("{threads} light threads");
    // `BENCH=updates` skips chunk lighting, to profile block updates alone.
    if std::env::var("BENCH").as_deref() != Ok("updates") {
        chunk_lighting(&PathBuf::from(world), centre);
    }
    block_updates();
}

/// A counter of the instructions retired by each thread that runs systems.
static COUNTERS: Mutex<Vec<Counter>> = Mutex::new(Vec::new());

fn count_this_thread() {
    let mut counter = perf_event::Builder::new(Hardware::INSTRUCTIONS)
        .exclude_kernel(true)
        .exclude_hv(true)
        .build()
        .expect("instruction counter (is perf_event_paranoid above 2?)");
    counter.enable().unwrap();
    COUNTERS.lock().unwrap().push(counter);
}

/// The instructions retired so far by every counted thread.
fn instructions() -> u64 {
    COUNTERS
        .lock()
        .unwrap()
        .iter_mut()
        .map(|counter| counter.read().unwrap())
        .sum()
}

/// The instructions `app.update()` takes.
fn update(app: &mut App) -> u64 {
    let before = instructions();
    app.update();
    instructions() - before
}

fn chunk_lighting(world: &PathBuf, centre: ChunkPos) {
    let (mut app, layer) = new_app(None);
    let mut folder = DimensionFolder::new(world, app.world().resource::<BiomeRegistry>());
    let mut area = |centre: ChunkPos| -> Vec<(ChunkPos, UnloadedChunk)> {
        (-RADIUS..RADIUS)
            .flat_map(|dx| {
                (-RADIUS..RADIUS).map(move |dz| ChunkPos::new(centre.x + dx, centre.z + dz))
            })
            .filter_map(|pos| Some((pos, folder.get_chunk(pos).ok()??.chunk)))
            .collect()
    };
    // Warm up on another area, so the measured one is lit for the first time.
    let warmup = area(ChunkPos::new(centre.x + 3 * RADIUS, centre.z));
    let chunks = area(centre);

    for (pass, chunks) in [
        ("warmup", &warmup),
        ("first load", &chunks),
        ("reload", &chunks),
    ] {
        app.world_mut()
            .get_mut::<ChunkLayer>(layer)
            .unwrap()
            .clear_chunks();
        // A tick without the chunks, so they are lit again from scratch.
        app.update();
        let mut chunk_layer = app.world_mut().get_mut::<ChunkLayer>(layer).unwrap();
        for (pos, chunk) in chunks {
            chunk_layer.insert_chunk(*pos, chunk.clone());
        }
        let before = app.world().resource::<LightStats>().chunks_lit;
        let ops = update(&mut app);
        let lit = app.world().resource::<LightStats>().chunks_lit - before;
        println!(
            "chunk lighting ({pass}): {lit} chunks, {:.3} M instructions per chunk",
            ops as f64 / lit as f64 / 1e6
        );
    }
}

fn block_updates() {
    let (mut app, layer) = new_app(Some(256));
    {
        let mut chunk_layer = app.world_mut().get_mut::<ChunkLayer>(layer).unwrap();
        for cx in -7..=7 {
            for cz in -7..=7 {
                let mut chunk = UnloadedChunk::with_height(256);
                for x in 0..16 {
                    for z in 0..16 {
                        chunk.set_block_state(x, 0, z, BlockState::BEDROCK);
                        chunk.set_block_state(x, 254, z, BlockState::GRASS_BLOCK);
                    }
                }
                chunk_layer.insert_chunk(ChunkPos::new(cx, cz), chunk);
            }
        }
    }
    app.update();
    let idle = (0..20).map(|_| update(&mut app)).min().unwrap();

    let grass = BlockPos::new(8, 254, 8);
    let glowstone = BlockPos::new(8, 1, 8);
    let cases = [
        (
            "remove block at y = 254",
            grass,
            BlockState::AIR,
            BlockState::GRASS_BLOCK,
        ),
        (
            "place block at y = 254",
            grass,
            BlockState::GRASS_BLOCK,
            BlockState::AIR,
        ),
        (
            "place glowstone",
            glowstone,
            BlockState::GLOWSTONE,
            BlockState::AIR,
        ),
        (
            "break glowstone",
            glowstone,
            BlockState::AIR,
            BlockState::GLOWSTONE,
        ),
    ];
    for (name, pos, state, undo) in cases {
        let mut counts: Vec<u64> = (0..10)
            .map(|_| {
                set_block(&mut app, layer, pos, undo);
                set_block(&mut app, layer, pos, state).saturating_sub(idle)
            })
            .collect();
        counts.sort_unstable();
        println!(
            "{name}: {:.1} k instructions",
            counts[counts.len() / 2] as f64 / 1e3
        );
    }
}

/// Sets the block and runs a tick, returning its instructions.
fn set_block(app: &mut App, layer: Entity, pos: BlockPos, state: BlockState) -> u64 {
    app.world_mut()
        .get_mut::<ChunkLayer>(layer)
        .unwrap()
        .set_block(pos, state);
    app.world_mut().write_message(BlockChanged { layer, pos });
    update(app)
}

/// An app with an empty layer: the overworld, or a dimension `height` blocks
/// tall starting at y = 0.
fn new_app(height: Option<i32>) -> (App, Entity) {
    let mut app = App::new();
    add_plugins(&mut app);
    app.add_plugins(LightPlugin);
    app.update();

    let dimension = match height {
        Some(height) => {
            let dimension = DimensionType {
                min_y: 0,
                height,
                logical_height: height,
                ..Default::default()
            };
            app.world_mut()
                .resource_mut::<DimensionTypeRegistry>()
                .insert(ident!("beacon:bench"), dimension);
            ident!("beacon:bench")
        }
        None => ident!("overworld"),
    };
    let world = app.world();
    let layer = ChunkLayer::new(
        dimension,
        world.resource::<DimensionTypeRegistry>(),
        world.resource::<BiomeRegistry>(),
        world.resource::<Server>(),
    );
    let layer = app.world_mut().spawn(layer).id();
    (app, layer)
}
