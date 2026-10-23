//! Sends each client the light of the chunks it sees.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use chunkedge::prelude::*;
use chunkedge::protocol::packets::play::LightUpdateS2c;
use chunkedge::protocol::{FixedArray, VarInt, VariableBitSet, WritePacket};

use crate::LightSystems;
use crate::layer::{ChunkLit, LayerLight};
use crate::storage::{ChunkLight, LightKind};

pub(crate) fn plugin(app: &mut App) {
    app.add_observer(attach_light_sent).add_systems(
        PostUpdate,
        (send_light, clear_dirty_sections)
            .chain()
            .in_set(LightSystems::Send),
    );
}

/// Light each client may be sent per tick, in uncompressed bytes, so joining
/// a dense world doesn't overflow ChunkEdge's outgoing buffer.
const LIGHT_BYTES_PER_TICK: usize = 1 << 20;

/// The chunks a client has been sent light for, in the layer it sees.
#[derive(Component, Debug)]
struct LightSent {
    layer: Entity,
    chunks: HashSet<ChunkPos>,
}

fn attach_light_sent(add: On<Add, Client>, mut commands: Commands) {
    commands.entity(add.entity).insert(LightSent {
        layer: Entity::PLACEHOLDER,
        chunks: HashSet::new(),
    });
}

/// Mirrors how ChunkEdge sends chunks: a chunk is sent when it enters the
/// view and forgotten once it leaves the view's outer radius. Light follows
/// in full whenever the chunk itself was (re)sent, and as changed sections
/// otherwise.
///
/// Each client gets [`LIGHT_BYTES_PER_TICK`], nearest chunks first. Chunks
/// that don't fit wait for a later tick; a chunk whose changes don't fit is
/// sent in full later instead.
fn send_light(
    mut clients: Query<(&mut Client, View, &VisibleChunkLayer, &mut LightSent)>,
    layers: Query<&LayerLight>,
    mut lit: MessageReader<ChunkLit>,
) {
    let mut fresh: HashMap<Entity, Vec<ChunkPos>> = HashMap::new();
    for lit in lit.read() {
        fresh.entry(lit.layer).or_default().push(lit.pos);
    }
    // Clients seeing the same chunk share its packet.
    let mut packets: HashMap<(Entity, ChunkPos, bool), LightUpdateS2c<'static>> = HashMap::new();

    for (mut client, view, visible, mut sent) in &mut clients {
        let layer = visible.0;
        let Ok(light) = layers.get(layer) else {
            continue;
        };
        let sent = &mut *sent;
        if sent.layer != layer {
            sent.layer = layer;
            sent.chunks.clear();
        }
        let view = view.get();
        let fresh = fresh.get(&layer).map_or(&[][..], Vec::as_slice);
        sent.chunks
            .retain(|&pos| view.contains(pos) && light.chunk(pos).is_some());
        // ChunkEdge resent these chunks without light.
        for pos in fresh {
            sent.chunks.remove(pos);
        }

        let distance = |pos: &ChunkPos| view.pos.distance_squared(*pos);
        let mut complete: Vec<ChunkPos> = view
            .iter()
            .chain(fresh.iter().copied().filter(|&pos| view.contains(pos)))
            .filter(|&pos| light.chunk(pos).is_some() && !sent.chunks.contains(&pos))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        complete.sort_unstable_by_key(distance);
        let mut changed: Vec<ChunkPos> = sent
            .chunks
            .iter()
            .copied()
            .filter(|&pos| light.chunk(pos).is_some_and(ChunkLight::is_dirty))
            .collect();
        changed.sort_unstable_by_key(distance);

        let mut budget = LIGHT_BYTES_PER_TICK;
        let queue = changed
            .into_iter()
            .map(|pos| (pos, false))
            .chain(complete.into_iter().map(|pos| (pos, true)));
        for (pos, complete) in queue {
            let chunk = light.chunk(pos).expect("only lit chunks are queued");
            let cost = packet_cost(chunk, complete);
            if cost > budget {
                if !complete {
                    sent.chunks.remove(&pos);
                }
                continue;
            }
            budget -= cost;
            let packet = packets
                .entry((layer, pos, complete))
                .or_insert_with(|| light_packet(pos, chunk, complete));
            client.write_packet(packet);
            sent.chunks.insert(pos);
        }
    }
}

/// Every client has been sent this tick's changes.
fn clear_dirty_sections(mut layers: Query<&mut LayerLight>) {
    layers.par_iter_mut().for_each(|mut light| {
        for chunk in light.chunks_mut().filter(|chunk| chunk.is_dirty()) {
            chunk.clear_dirty();
        }
    });
}

/// Roughly what [`light_packet`] weighs on the wire: uniform sections
/// compress to almost nothing.
fn packet_cost(light: &ChunkLight, complete: bool) -> usize {
    [LightKind::Sky, LightKind::Block]
        .into_iter()
        .flat_map(|kind| {
            light
                .sections(kind)
                .iter()
                .enumerate()
                .filter(move |(section, _)| complete || light.dirty(kind).contains(*section))
                .map(|(_, nibbles)| match nibbles.uniform_level() {
                    Some(_) => 32,
                    None => 2048,
                })
        })
        .sum()
}

/// Every section when `complete`, else only the ones that changed. All-dark
/// sections go in the empty masks.
fn light_packet(pos: ChunkPos, light: &ChunkLight, complete: bool) -> LightUpdateS2c<'static> {
    let mut masks = [VariableBitSet::new(), VariableBitSet::new()];
    let mut empty_masks = [VariableBitSet::new(), VariableBitSet::new()];
    let mut arrays: [Vec<FixedArray<u8, 2048>>; 2] = [Vec::new(), Vec::new()];

    for kind in [LightKind::Sky, LightKind::Block] {
        let k = kind as usize;
        for (section, nibbles) in light.sections(kind).iter().enumerate() {
            if !complete && !light.dirty(kind).contains(section) {
                continue;
            }
            if nibbles.uniform_level() == Some(0) {
                empty_masks[k].set(section);
            } else {
                masks[k].set(section);
                arrays[k].push(FixedArray(*nibbles.bytes()));
            }
        }
    }

    let [sky_mask, block_mask] = masks;
    let [empty_sky_mask, empty_block_mask] = empty_masks;
    let [sky_arrays, block_arrays] = arrays;
    LightUpdateS2c {
        chunk_x: VarInt(pos.x),
        chunk_z: VarInt(pos.z),
        sky_light_mask: Cow::Owned(sky_mask),
        block_light_mask: Cow::Owned(block_mask),
        empty_sky_light_mask: Cow::Owned(empty_sky_mask),
        empty_block_light_mask: Cow::Owned(empty_block_mask),
        sky_light_arrays: Cow::Owned(sky_arrays),
        block_light_arrays: Cow::Owned(block_arrays),
    }
}
