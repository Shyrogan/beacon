//! Serves an Anvil world with beacon's lighting, to check it in game.
//!
//! ```sh
//! cargo run --example world -- <world dir> [x z]
//! ```
//!
//! Join `localhost` with a 26.2 client. You spawn flying in creative above
//! `x z` (default 0 0). Break and place blocks, torches and glowstone to see
//! light update. Chunks load as you fly and unload once no one can see them.

use std::path::PathBuf;

use beacon::{BlockChanged, LightPlugin};
use chunkedge::abilities::PlayerAbilitiesFlags;
use chunkedge::anvil::{AnvilLevel, ChunkLoadMessage, ChunkLoadStatus};
use chunkedge::interact_block::InteractBlockMessage;
use chunkedge::inventory::HeldItem;
use chunkedge::prelude::*;

#[derive(Resource)]
struct Spawn(DVec3);

pub fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: world <world dir> [x z]");
        return;
    };
    if !path.join("region").is_dir() {
        eprintln!("`{}` has no `region` directory", path.display());
        return;
    }
    let coords: Vec<f64> = args.filter_map(|arg| arg.parse().ok()).collect();
    let spawn = match coords[..] {
        [x, z] => DVec3::new(x, 120.0, z),
        _ => DVec3::new(0.0, 120.0, 0.0),
    };

    App::new()
        // ChunkEdge sends every chunk in view at once. At high render
        // distances on a dense world that exceeds the default 8 MiB buffer
        // and kicks the player.
        .insert_resource(NetworkSettings {
            outgoing_byte_limit: 64 << 20,
            ..Default::default()
        })
        .add_plugins((DefaultPlugins, LightPlugin))
        .insert_resource(Spawn(spawn))
        .add_systems(
            Startup,
            move |mut commands: Commands,
                  dimensions: Res<DimensionTypeRegistry>,
                  biomes: Res<BiomeRegistry>,
                  server: Res<Server>| {
                let layer = LayerBundle::new(ident!("overworld"), &dimensions, &biomes, &server);
                commands.spawn((layer, AnvilLevel::new(&path, &biomes)));
            },
        )
        .add_systems(
            Update,
            (
                despawn_disconnected_clients,
                init_clients,
                handle_chunk_loads,
                break_blocks,
                place_blocks,
            ),
        )
        .run();
}

/// What `init_clients` sets up on a joining client.
type NewClient = (
    &'static mut EntityLayerId,
    &'static mut VisibleChunkLayer,
    &'static mut VisibleEntityLayers,
    &'static mut Position,
    &'static mut GameMode,
    &'static mut PlayerAbilitiesFlags,
);

fn init_clients(
    mut clients: Query<NewClient, Added<Client>>,
    layers: Query<Entity, With<ChunkLayer>>,
    spawn: Res<Spawn>,
) {
    let Ok(layer) = layers.single() else {
        return;
    };
    for (
        mut layer_id,
        mut visible_chunks,
        mut visible_entities,
        mut pos,
        mut game_mode,
        mut abilities,
    ) in &mut clients
    {
        layer_id.0 = layer;
        visible_chunks.0 = layer;
        visible_entities.0.insert(layer);
        pos.set(spawn.0);
        *game_mode = GameMode::Creative;
        abilities.set_allow_flying(true);
        abilities.set_flying(true);
    }
}

/// Missing or unreadable chunks become empty ones, so the world has no holes.
fn handle_chunk_loads(
    mut messages: MessageReader<ChunkLoadMessage>,
    mut layers: Query<&mut ChunkLayer>,
) {
    let Ok(mut layer) = layers.single_mut() else {
        return;
    };
    for message in messages.read() {
        match &message.status {
            ChunkLoadStatus::Success { .. } => {}
            ChunkLoadStatus::Empty => {
                layer.insert_chunk(message.pos, UnloadedChunk::new());
            }
            ChunkLoadStatus::Failed(error) => {
                eprintln!("failed to load chunk {:?}: {error:#}", message.pos);
                layer.insert_chunk(message.pos, UnloadedChunk::new());
            }
        }
    }
}

fn break_blocks(
    clients: Query<&GameMode>,
    mut layers: Query<(Entity, &mut ChunkLayer)>,
    mut digging: MessageReader<DiggingMessage>,
    mut changed: MessageWriter<BlockChanged>,
) {
    let Ok((entity, mut layer)) = layers.single_mut() else {
        return;
    };
    for message in digging.read() {
        let Ok(game_mode) = clients.get(message.client) else {
            continue;
        };
        if *game_mode == GameMode::Creative && message.state == DiggingState::Start {
            layer.set_block(message.position, BlockState::AIR);
            changed.write(BlockChanged {
                layer: entity,
                pos: message.position,
            });
        }
    }
}

fn place_blocks(
    clients: Query<(&Inventory, &HeldItem)>,
    mut layers: Query<(Entity, &mut ChunkLayer)>,
    mut interactions: MessageReader<InteractBlockMessage>,
    mut changed: MessageWriter<BlockChanged>,
) {
    let Ok((entity, mut layer)) = layers.single_mut() else {
        return;
    };
    for message in interactions.read() {
        let Ok((inventory, held)) = clients.get(message.client) else {
            continue;
        };
        if message.hand != Hand::Main {
            continue;
        }
        let Some(kind) = BlockKind::from_item_kind(inventory.slot(held.slot()).item) else {
            continue;
        };
        let pos = message.position.get_in_direction(message.face);
        layer.set_block(pos, kind.to_state());
        changed.write(BlockChanged { layer: entity, pos });
    }
}
