# Cubelark

A small HD voxel renderer built on Bevy 0.20 ECS. Voxels are tiny (16 per meter) and lit
like a modern diorama render: baked per-vertex AO, per-voxel color jitter, PBR surfaces,
soft cascaded shadows, SSAO, TAA, bloom, emissive voxels, and a physical sky.

```sh
bevy run            # or: cargo run
```

In the browser:

```sh
bevy run web --open
```

Web builds use WebGPU, which the physical sky needs, so they need a WebGPU browser (recent
Chrome, Edge, Safari or Firefox). On the web, SSAO and model hot reload are off; everything
else matches native.

Controls: drag to orbit, scroll to zoom, `R` toggles auto-rotate, `G` toggles XYZ axes.

## How it fits together

| Piece | Where | Role |
| --- | --- | --- |
| `VoxelModelSource` | `src/voxel/mod.rs` | Component pointing an entity at a `.vox.ron` model |
| `VoxelModel` | `src/voxel/mod.rs` | Runtime data: `VoxelGrid` + `Palette` + scale. Mutate it and the mesh rebuilds |
| `VoxelGrid` | `src/voxel/grid.rs` | Dense `(x, y, z)` storage, `+Y` up, `0` = air |
| `Palette` / `Surface` | `src/voxel/palette.rs` | Voxel colors; each surface becomes one `StandardMaterial` |
| mesher | `src/voxel/mesher.rs` | 32³ chunks, face culling, baked AO, one mesh per chunk × surface |
| loader | `src/voxel/asset.rs` | Parses `.vox.ron` and rasterizes its ops into a grid |
| scene | `src/scene.rs` | Sky, sun, post-processing, orbit camera |

## Model format (`assets/models/*.vox.ron`)

A model declares `surfaces` (roughness, metallic, emissive, alpha), a `palette` of named
colors that reference a surface, and an ordered list of `ops`:

- `Shape(geom, fill, noise?, mask?)` with `Box`, `Ellipsoid`, `Capsule` or `Cylinder` geometry.
  `noise` distorts the boundary; `mask` only overwrites the listed materials (`"air"` too).
- `Island(...)` builds a floating island with a heightmap top and layered soil and rock.
- `Scatter(...)` decorates exposed top faces: tufts, flowers, petals.
- `Settle(...)` and `Spill(...)` make liquids rest on ground and pour over ledges.

A `fill` is either `Solid("name")` or `Noise(mats: [...], scale)`. Point `lights` can be
attached in voxel coordinates. Save the file while the app runs and the model reloads live.

To render a still: `CUBELARK_SHOT=shot.png CUBELARK_YAW=0.2 cargo run`.
