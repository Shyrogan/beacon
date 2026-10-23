package dev.beacon.extractor;

import com.google.gson.GsonBuilder;
import com.google.gson.JsonArray;
import com.google.gson.JsonObject;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.util.HashMap;
import java.util.Map;
import net.fabricmc.api.ModInitializer;
import net.minecraft.core.Direction;
import net.minecraft.world.level.block.Block;
import net.minecraft.world.level.block.state.BlockState;
import net.minecraft.world.phys.shapes.VoxelShape;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * Dumps the per-state block properties the light engine reads, then exits.
 *
 * <p>Block state caches are built during bootstrap, so everything is available
 * in {@code onInitialize} without starting a world.
 */
public class Main implements ModInitializer {

    public static final Logger LOGGER = LoggerFactory.getLogger("beacon_extractor");

    /** Bit set in a state's flags when skylight passes down through it undiminished. */
    private static final int PROPAGATES_SKYLIGHT_DOWN = 1;
    /** Bit set when light occlusion depends on the face shapes (vanilla's !isEmptyShape). */
    private static final int USES_SHAPE_FOR_OCCLUSION = 2;

    @Override
    public void onInitialize() {
        try {
            var out = Files.createDirectories(Paths.get("beacon_extractor_output"))
                .resolve("light.json");
            try (var writer = Files.newBufferedWriter(out, StandardCharsets.UTF_8)) {
                new GsonBuilder().create().toJson(extract(), writer);
            }
            LOGGER.info("Wrote " + out.toAbsolutePath());
        } catch (IOException e) {
            LOGGER.error("Light extraction failed", e);
            System.exit(1);
        }
        System.exit(0);
    }

    /**
     * {@code states[id] = [dampening, flags, emission, face DOWN, UP, NORTH, SOUTH, WEST, EAST]},
     * where faces index into {@code faces}: 16x16 coverage masks as 64 hex
     * digits. Face 0 is empty and face 1 is full.
     */
    private static JsonObject extract() {
        var faceIds = new HashMap<String, Integer>();
        var faces = new JsonArray();
        internFace(faceIds, faces, "0".repeat(64));
        internFace(faceIds, faces, "f".repeat(64));

        var states = new JsonArray();
        for (int id = 0; id < Block.BLOCK_STATE_REGISTRY.size(); id++) {
            BlockState state = Block.BLOCK_STATE_REGISTRY.byId(id);

            boolean usesShape = state.canOcclude() && state.useShapeForLightOcclusion();
            int flags = (state.propagatesSkylightDown() ? PROPAGATES_SKYLIGHT_DOWN : 0)
                | (usesShape ? USES_SHAPE_FOR_OCCLUSION : 0);

            var entry = new JsonArray();
            entry.add(state.getLightDampening());
            entry.add(flags);
            entry.add(state.getLightEmission());
            for (var dir : Direction.values()) {
                entry.add(usesShape
                    ? internFace(faceIds, faces, rasterize(state.getFaceOcclusionShape(dir), dir.getAxis()))
                    : 0);
            }
            states.add(entry);
        }

        var root = new JsonObject();
        root.add("faces", faces);
        root.add("states", states);
        return root;
    }

    private static int internFace(Map<String, Integer> ids, JsonArray faces, String mask) {
        return ids.computeIfAbsent(mask, m -> {
            faces.add(m);
            return faces.size() - 1;
        });
    }

    /**
     * Projects a face shape onto its face plane and samples the 16x16 pixel
     * centers. Bit {@code u + 16 * v} is set when the pixel is covered, with
     * (u, v) = (x, z) for Y faces, (x, y) for Z faces and (z, y) for X faces,
     * so opposite faces of neighbours line up.
     */
    private static String rasterize(VoxelShape shape, Direction.Axis axis) {
        var bits = new long[4];
        for (var box : shape.toAabbs()) {
            double minU, maxU, minV, maxV;
            switch (axis) {
                case Y -> { minU = box.minX; maxU = box.maxX; minV = box.minZ; maxV = box.maxZ; }
                case Z -> { minU = box.minX; maxU = box.maxX; minV = box.minY; maxV = box.maxY; }
                default -> { minU = box.minZ; maxU = box.maxZ; minV = box.minY; maxV = box.maxY; }
            }
            for (int v = 0; v < 16; v++) {
                double cv = (v + 0.5) / 16.0;
                if (cv < minV || cv > maxV) continue;
                for (int u = 0; u < 16; u++) {
                    double cu = (u + 0.5) / 16.0;
                    if (cu < minU || cu > maxU) continue;
                    int bit = u + 16 * v;
                    bits[bit >>> 6] |= 1L << (bit & 63);
                }
            }
        }
        var hex = new StringBuilder(64);
        for (var word : bits) {
            hex.append(String.format("%016x", word));
        }
        return hex.toString();
    }
}
