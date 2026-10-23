//! Joins an Anvil world with a mock client and reports, per tick, the time
//! spent and the bytes sent as chunks and as light.
//!
//! ```sh
//! cargo run --example bench_world -- <world dir> <x> <z> [--no-light]
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use beacon::LightPlugin;
use chunkedge::ServerSettings;
use chunkedge::anvil::{AnvilLevel, ChunkLoadMessage, ChunkLoadStatus};
use chunkedge::keepalive::KeepaliveSettings;
use chunkedge::prelude::*;
use chunkedge::protocol::Packet;
use chunkedge::protocol::packets::play::{LevelChunkWithLightS2c, LightUpdateS2c};
use chunkedge::testing::{add_plugins, create_mock_client};
use flate2::Compression;
use flate2::write::ZlibEncoder;

fn view_distance() -> u8 {
    std::env::var("VIEW_DISTANCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [world, x, z, rest @ ..] = &args[..] else {
        eprintln!("usage: bench_world <world dir> <x> <z> [--no-light]");
        return;
    };
    let light = !rest.iter().any(|arg| arg == "--no-light");
    let spawn = DVec3::new(x.parse().unwrap(), 120.0, z.parse().unwrap());

    let mut app = App::new();
    app.insert_resource(KeepaliveSettings {
        period: Duration::MAX,
    })
    .insert_resource(ServerSettings {
        // The mock client reads uncompressed frames; compression is
        // measured below instead.
        compression_threshold: Default::default(),
        ..Default::default()
    });
    add_plugins(&mut app);
    if light {
        app.add_plugins(LightPlugin);
    }
    app.add_systems(Update, insert_missing_chunks);
    app.update();

    let world_ref = app.world();
    let layer = LayerBundle::new(
        ident!("overworld"),
        world_ref.resource::<DimensionTypeRegistry>(),
        world_ref.resource::<BiomeRegistry>(),
        world_ref.resource::<Server>(),
    );
    let level = AnvilLevel::new(PathBuf::from(world), world_ref.resource::<BiomeRegistry>());
    let layer = app.world_mut().spawn((layer, level)).id();

    let (mut client, mut helper) = create_mock_client("bench");
    client.layer.0 = layer;
    client.visible_chunk_layer.0 = layer;
    client.visible_entity_layers.0.insert(layer);
    let client = app.world_mut().spawn(client).id();
    let mut entity = app.world_mut().entity_mut(client);
    entity.get_mut::<Position>().unwrap().set(spawn);
    entity
        .get_mut::<ViewDistance>()
        .unwrap()
        .set(view_distance());

    println!("tick  ms    chunks  chunk KiB (zlib)   light KiB (zlib)   other KiB");
    let mut quiet = 0;
    let mut totals = [0usize; 6];
    for tick in 0.. {
        let start = Instant::now();
        app.update();
        let elapsed = start.elapsed();

        let frames = helper.collect_received().0;
        let mut bytes = [0usize; 6];
        for frame in &frames {
            let kind = match frame.id {
                LevelChunkWithLightS2c::ID => 0,
                LightUpdateS2c::ID => 2,
                _ => 4,
            };
            bytes[kind] += frame.body.len();
            bytes[kind + 1] += zlib_len(&frame.body);
        }
        for (total, b) in totals.iter_mut().zip(bytes) {
            *total += b;
        }
        let chunks = frames
            .iter()
            .filter(|f| f.id == LevelChunkWithLightS2c::ID)
            .count();

        if bytes.iter().sum::<usize>() > 0 {
            quiet = 0;
            println!(
                "{tick:>4}  {:>4}  {chunks:>6}  {:>7} ({:>6})   {:>7} ({:>6})   {:>6}",
                elapsed.as_millis(),
                bytes[0] / 1024,
                bytes[1] / 1024,
                bytes[2] / 1024,
                bytes[3] / 1024,
                bytes[4] / 1024,
            );
        } else {
            quiet += 1;
            if quiet == 40 {
                break;
            }
        }
    }
    println!(
        "total: chunks {} KiB ({} zlib), light {} KiB ({} zlib)",
        totals[0] / 1024,
        totals[1] / 1024,
        totals[2] / 1024,
        totals[3] / 1024
    );
}

fn zlib_len(bytes: &[u8]) -> usize {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(4));
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap().len()
}

fn insert_missing_chunks(
    mut messages: MessageReader<ChunkLoadMessage>,
    mut layers: Query<&mut ChunkLayer>,
) {
    let Ok(mut layer) = layers.single_mut() else {
        return;
    };
    for message in messages.read() {
        if !matches!(message.status, ChunkLoadStatus::Success { .. }) {
            layer.insert_chunk(message.pos, UnloadedChunk::new());
        }
    }
}
