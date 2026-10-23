//! Rewrites a vanilla world in place so ChunkEdge's anvil loader can read it:
//! renames blocks that changed name since the world was saved (vanilla does
//! this with data fixers, ChunkEdge has none), and drops the light-only
//! sections vanilla saves above and below the world, which the loader rejects
//! and beacon recomputes anyway.
//!
//! ```sh
//! cargo run --example upgrade_world -- <world dir>
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use chunkedge::anvil::RegionFolder;
use chunkedge::nbt::{Compound, List, Value};
use chunkedge::prelude::*;

/// Old name to 26.2 name.
const RENAMES: &[(&str, &str)] = &[("minecraft:chain", "minecraft:iron_chain")];

fn main() {
    let Some(world) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: upgrade_world <world dir>");
        return;
    };
    let mut region = RegionFolder::new(world.join("region"));
    let positions: Vec<(i32, i32)> = region
        .all_chunk_positions()
        .expect("list region files")
        .filter_map(Result::ok)
        .collect();

    let mut rewritten = 0;
    let mut unknown = BTreeMap::<String, usize>::new();
    for &(x, z) in &positions {
        let Ok(Some(mut chunk)) = region.get_chunk::<String>(x, z) else {
            continue;
        };
        if upgrade_chunk(&mut chunk.data, &mut unknown) {
            region.set_chunk(x, z, &chunk.data).expect("write chunk");
            rewritten += 1;
        }
    }

    println!("{rewritten} of {} chunks rewritten", positions.len());
    for (name, count) in unknown {
        println!("still unknown: {name} in {count} palettes");
    }
}

/// Drops light-only sections, renames old blocks in every section palette,
/// and records names that are still unknown. Returns whether anything
/// changed.
fn upgrade_chunk(chunk: &mut Compound, unknown: &mut BTreeMap<String, usize>) -> bool {
    let Some(Value::List(List::Compound(sections))) = chunk.get_mut("sections") else {
        return false;
    };
    let before = sections.len();
    sections.retain(|section| section.get("block_states").is_some());
    let mut changed = sections.len() != before;
    for section in sections {
        let Some(Value::Compound(block_states)) = section.get_mut("block_states") else {
            continue;
        };
        let Some(Value::List(List::Compound(palette))) = block_states.get_mut("palette") else {
            continue;
        };
        for entry in palette {
            let Some(Value::String(name)) = entry.get_mut("Name") else {
                continue;
            };
            if let Some((_, new)) = RENAMES.iter().find(|(old, _)| old == name) {
                *name = new.to_string();
                changed = true;
            }
            let path = name.strip_prefix("minecraft:").unwrap_or(name);
            if BlockKind::from_str(path).is_none() {
                *unknown.entry(name.clone()).or_default() += 1;
            }
        }
    }
    changed
}
