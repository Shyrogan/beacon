//! Undoes ChunkEdge's full-bright workaround for having no light engine.

use chunkedge::nbt::Value;
use chunkedge::prelude::*;
use chunkedge::registry::RegistryCodec;

pub(crate) fn plugin(app: &mut App) {
    app.add_systems(Startup, restore_vanilla_ambient_light);
}

/// ChunkEdge brightens every dimension to full ambient light. Put vanilla's
/// values back before the registry is snapshotted for clients in
/// `PostStartup`.
fn restore_vanilla_ambient_light(
    mut dimensions: ResMut<DimensionTypeRegistry>,
    codec: Res<RegistryCodec>,
) {
    const COLOR: &str = "minecraft:visual/ambient_light_color";
    let vanilla = codec.registry(DimensionTypeRegistry::KEY);
    for (_, name, dimension) in dimensions.iter_mut() {
        // Dimensions vanilla doesn't know keep what they were given.
        let Some(element) = vanilla
            .iter()
            .find(|value| value.name.as_str() == name.as_str())
            .map(|value| &value.element)
        else {
            continue;
        };
        if let Some(Value::Float(ambient)) = element.get("ambient_light") {
            dimension.ambient_light = *ambient;
        }
        let color = match element.get("attributes") {
            Some(Value::Compound(attributes)) => attributes.get(COLOR).cloned(),
            _ => None,
        };
        if let Some(attributes) = dimension.attributes.as_mut() {
            match color {
                Some(color) => {
                    attributes.insert(COLOR, color);
                }
                None => {
                    attributes.remove(COLOR);
                }
            }
        }
    }
}
