//! A Starlight-style light engine for ChunkEdge.

mod ambient;
mod client;
mod engine;
mod layer;
mod properties;
mod storage;

use chunkedge::client::{FlushPacketsSet, UpdateClientsSet};
use chunkedge::layer::UpdateLayersPreClientSet;
use chunkedge::prelude::*;
pub use layer::{BlockChanged, ChunkLit, ChunkReplaced, LayerLight, LightStats};
pub use properties::{Face, FaceMask, LightProperties};

/// Computes sky and block light for every chunk of every `ChunkLayer` and
/// sends it to clients.
///
/// Chunks are lit when they load. ChunkEdge doesn't report block changes, so
/// write a [`BlockChanged`] for every block you set, and a [`ChunkReplaced`]
/// when `insert_chunk` overwrites a loaded chunk, before
/// [`LightSystems::Propagate`].
pub struct LightPlugin;

impl Plugin for LightPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            PostUpdate,
            (
                // After ChunkEdge has inserted this tick's chunks, including
                // the anvil loader's.
                LightSystems::Propagate
                    .after(UpdateLayersPreClientSet)
                    .before(UpdateClientsSet),
                // After ChunkEdge has written chunk packets, so light follows
                // them.
                LightSystems::Send
                    .after(UpdateClientsSet)
                    .before(FlushPacketsSet),
            ),
        )
        .add_plugins((ambient::plugin, layer::plugin, client::plugin));
    }
}

/// Where beacon's systems run, in `PostUpdate`.
#[derive(SystemSet, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LightSystems {
    /// Updates [`LayerLight`] for loaded, unloaded, replaced and changed
    /// chunks.
    Propagate,
    /// Sends light to clients.
    Send,
}
