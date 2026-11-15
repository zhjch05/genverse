//! What a world is made of, as data: the materials a color can have, and the blocks of
//! the ground.
//!
//! A material is how a surface takes the light ([`MATERIALS`]), and any color of a
//! build or a block may tune it ([`Tune`]): rougher, more metal, glowing, see-through.
//! A world's blocks ([`Content`]) are the ground's own, which it may recolor, and
//! blocks of its own, which land changes paint with by name. They're part of the
//! world's rules, so they're recorded, replayed and undone like any rule.
//!
//! A block's place in the list is the voxel it is, for good: a block that's taken out
//! keeps its place, so the voxels made of it stay what they were.

use std::collections::BTreeMap;

use bevy_color::prelude::*;
use serde::{Deserialize, Serialize};

use crate::recipe::{self, FAMILIES};
use crate::terrain::{Terrain, block};
use crate::voxel::{Palette, PaletteEntry, Surface, VoxelId};

/// How a surface takes the light.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Finish {
    /// 0 is a mirror, 1 is chalk.
    pub roughness: f32,
    /// 0 is not metal, 1 is.
    pub metallic: f32,
    pub reflectance: f32,
    /// How much light it gives off in its own color: 0 is none, 1 is a lantern. The
    /// steps between are even to the eye ([`Finish::light`]).
    pub glow: f32,
    /// 1 is solid; less is see-through.
    pub alpha: f32,
}

/// The materials every world has, and what each is for.
pub const MATERIALS: [(&str, Finish, &str); 5] = [
    (
        "matte",
        Finish {
            roughness: 0.9,
            metallic: 0.0,
            reflectance: 0.35,
            glow: 0.0,
            alpha: 1.0,
        },
        "flat and soft: stone, wood, cloth, plaster",
    ),
    (
        "glossy",
        Finish {
            roughness: 0.25,
            metallic: 0.0,
            reflectance: 0.5,
            glow: 0.0,
            alpha: 1.0,
        },
        "smooth and shiny: ceramic, paint, wet things",
    ),
    (
        "metal",
        Finish {
            roughness: 0.35,
            metallic: 0.9,
            reflectance: 0.5,
            glow: 0.0,
            alpha: 1.0,
        },
        "iron, gold, steel",
    ),
    (
        "glass",
        Finish {
            roughness: 0.05,
            metallic: 0.0,
            reflectance: 0.6,
            glow: 0.0,
            alpha: 0.45,
        },
        "see-through: windows, ice, crystal",
    ),
    (
        "glow",
        Finish {
            roughness: 0.5,
            metallic: 0.0,
            reflectance: 0.4,
            glow: 1.0,
            alpha: 1.0,
        },
        "gives off light in its color: lanterns, lava, magic",
    ),
];

/// The light a glowing voxel gives off at glow 1, as a multiple of its color: far past
/// white, so it blooms.
pub const GLOW: f32 = 6_000.0;
pub const MAX_GLOW: f32 = 1.0;
pub const MIN_ALPHA: f32 = 0.1;

/// The first voxel that's one of a world's own blocks: the ground's come before it.
pub const FIRST: VoxelId = block::OWN;
/// How many blocks of its own a world may have.
pub const MAX_BLOCKS: usize = 64;

/// What a color changes about its material. Whatever is left out is the material's.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tune {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roughness: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metallic: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glow: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alpha: Option<f64>,
}

impl Tune {
    /// Nothing changed: the material as it is.
    pub fn is_plain(&self) -> bool {
        *self == Self::default()
    }

    fn fields(&self) -> [(&'static str, Option<f64>, f32, f32); 4] {
        [
            ("roughness", self.roughness, 0.0, 1.0),
            ("metallic", self.metallic, 0.0, 1.0),
            ("glow", self.glow, 0.0, MAX_GLOW),
            ("alpha", self.alpha, MIN_ALPHA, 1.0),
        ]
    }

    /// The changes in words: "glow 0.3, roughness 0.8".
    pub fn describe(&self) -> String {
        self.fields()
            .iter()
            .filter_map(|(name, v, ..)| v.map(|v| format!("{name} {v}")))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn material(name: &str) -> Option<Finish> {
    MATERIALS
        .iter()
        .find(|(n, ..)| *n == name)
        .map(|(_, finish, _)| *finish)
}

/// The names of the materials, for saying what there is.
pub fn material_names() -> String {
    MATERIALS.map(|(name, ..)| name).join(", ")
}

/// What's wrong with a material and its changes, phrased for whoever wrote them.
pub fn check(name: &str, tune: &Tune) -> Result<Finish, String> {
    if material(name).is_none() {
        return Err(format!(
            "unknown material {name:?}: use one of {}",
            material_names()
        ));
    }
    for (field, value, lo, hi) in tune.fields() {
        if value.is_some_and(|v| !v.is_finite() || v < lo as f64 || v > hi as f64) {
            return Err(format!("{field} is from {lo} to {hi}"));
        }
    }
    Ok(Finish::of(name, tune))
}

impl Finish {
    /// The finish of a material with its changes, whatever they say: an unknown
    /// material is matte, and every number is kept in its range.
    pub fn of(name: &str, tune: &Tune) -> Self {
        let base = material(name).unwrap_or(MATERIALS[0].1);
        let [roughness, metallic, glow, alpha] = tune.fields().map(|(_, v, lo, hi)| {
            v.filter(|v| v.is_finite())
                .map(|v| (v as f32).clamp(lo, hi))
        });
        Self {
            // A perfect mirror shows the renderer's seams.
            roughness: roughness.unwrap_or(base.roughness).max(0.02),
            metallic: metallic.unwrap_or(base.metallic),
            reflectance: base.reflectance,
            glow: glow.unwrap_or(base.glow),
            alpha: alpha.unwrap_or(base.alpha),
        }
    }

    /// The light it gives off, as a multiple of its color. Light adds straight onto
    /// the picture, where a few times a color is already bright, so glow counts in
    /// powers: 0.1 is about 1.4 times the color, 0.3 about 13, 0.5 about 76, 1 is
    /// [`GLOW`].
    pub fn light(&self) -> f32 {
        if self.glow > 0.0 {
            (GLOW.powf(self.glow.min(MAX_GLOW)) - 1.0) * GLOW / (GLOW - 1.0)
        } else {
            0.0
        }
    }

    /// The surface a color with this finish is drawn as.
    pub fn surface(&self, name: &str, color: LinearRgba) -> Surface {
        Surface {
            name: name.into(),
            roughness: self.roughness,
            metallic: self.metallic,
            reflectance: self.reflectance,
            emissive: if self.glow > 0.0 {
                color * self.light()
            } else {
                LinearRgba::BLACK
            },
            alpha: self.alpha,
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn matte() -> String {
    "matte".into()
}

/// One of a world's own blocks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlockDef {
    pub name: String,
    /// `#rrggbb`.
    pub hex: String,
    #[serde(default = "matte")]
    pub material: String,
    #[serde(flatten)]
    pub tune: Tune,
    /// How much each voxel's brightness differs from the next, 0 to 0.3: 0 is one flat
    /// color. Left out, a little.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variation: Option<f64>,
    /// Taken out: what's made of it stays as it looks, and nothing new is.
    #[serde(default, skip_serializing_if = "is_false")]
    pub gone: bool,
}

const VARIATION: f32 = 0.05;
const MAX_VARIATION: f32 = 0.3;

impl BlockDef {
    fn color(&self) -> LinearRgba {
        Srgba::hex(&self.hex)
            .map(Into::into)
            .unwrap_or(LinearRgba::rgb(0.5, 0.5, 0.5))
    }

    fn variation(&self) -> f32 {
        self.variation
            .filter(|v| v.is_finite())
            .map_or(VARIATION, |v| (v as f32).clamp(0.0, MAX_VARIATION))
    }

    /// "lava #ff5a00, glow (glow 0.3)".
    pub fn describe(&self) -> String {
        let tune = self.tune.describe();
        format!(
            "{} {}, {}{}",
            self.name,
            self.hex,
            self.material,
            if tune.is_empty() {
                String::new()
            } else {
                format!(" ({tune})")
            }
        )
    }
}

/// A world's blocks: colors over the ground's own, and blocks of its own.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Content {
    /// The world's own blocks, in the order they were made: a block's place is the
    /// voxel it is ([`FIRST`] and up).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<BlockDef>,
    /// Colors over the ground's own blocks, by family ([`FAMILIES`]), as `#rrggbb`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub colors: BTreeMap<String, String>,
}

fn rgb(hex: &str) -> Option<[u8; 3]> {
    let hex = hex.trim();
    (hex.len() == 7 && hex.starts_with('#')).then_some(())?;
    let [r, g, b, _] = Srgba::hex(hex).ok()?.to_u8_array();
    Some([r, g, b])
}

fn family(name: &str) -> Option<&'static str> {
    FAMILIES
        .iter()
        .map(|(family, ..)| *family)
        .find(|f| *f == name)
}

impl Content {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The world's own blocks that can be used, with the voxel each is.
    pub fn own(&self) -> impl Iterator<Item = (VoxelId, &BlockDef)> {
        self.blocks
            .iter()
            .take(MAX_BLOCKS)
            .enumerate()
            .filter(|(_, b)| !b.gone)
            .map(|(i, b)| (FIRST + i as VoxelId, b))
    }

    /// The voxel of the world's own block called `name`.
    pub fn id(&self, name: &str) -> Option<VoxelId> {
        self.own().find(|(_, b)| b.name == name).map(|(id, _)| id)
    }

    /// The name of the world's own block a voxel is, taken out or not.
    pub fn name_of(&self, id: VoxelId) -> Option<&str> {
        let i = (id as usize).checked_sub(FIRST as usize)?;
        self.blocks
            .iter()
            .take(MAX_BLOCKS)
            .nth(i)
            .map(|b| b.name.as_str())
    }

    /// Makes a block, or changes the one of that name. A family of the ground's own
    /// blocks (grass, water, ...) takes the color, and nothing else.
    pub fn define(&mut self, mut def: BlockDef) -> Result<String, String> {
        def.name = def.name.trim().to_lowercase();
        def.hex = def.hex.trim().to_lowercase();
        def.gone = false;
        let name = def.name.clone();
        let named = !name.is_empty()
            && name.len() <= 24
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !named {
            return Err("A block's name is lowercase letters, digits and _, starting with a letter, up to 24 long: lava, blue_ice.".into());
        }
        if rgb(&def.hex).is_none() {
            return Err(format!("{name}'s color must be #rrggbb, like #ff5a00."));
        }
        if let Some(family) = family(&name) {
            if def.material != "matte" || !def.tune.is_plain() || def.variation.is_some() {
                return Err(format!(
                    "{family} is one of the ground's own blocks: only its color changes. Make a block of your own for another material."
                ));
            }
            let before = self.colors.insert(family.into(), def.hex.clone());
            return Ok(match before {
                Some(before) if before == def.hex => format!("{family} is already {}.", def.hex),
                _ => format!("{family} is now {} everywhere in this world.", def.hex),
            });
        }
        if name == "air"
            || crate::terrain::palette().find(&name).is_some()
            || crate::sculpt::ITEMS.contains(&name.as_str())
        {
            return Err(format!("{name} is taken by the ground: pick another name."));
        }
        check(&def.material, &def.tune).map_err(|e| format!("{name}: {e}."))?;
        if def
            .variation
            .is_some_and(|v| !v.is_finite() || !(0.0..=MAX_VARIATION as f64).contains(&v))
        {
            return Err(format!("{name}: variation is from 0 to {MAX_VARIATION}."));
        }
        let said = def.describe();
        match self
            .blocks
            .iter()
            .take(MAX_BLOCKS)
            .position(|b| b.name == name)
        {
            Some(i) if self.blocks[i] == def => Ok(format!("{said} is already like that.")),
            Some(i) => {
                let back = self.blocks[i].gone;
                self.blocks[i] = def;
                Ok(if back {
                    format!("{said} is back, and so is everything made of it.")
                } else {
                    format!("{said}: everything made of it changed with it.")
                })
            }
            None if self.blocks.len() >= MAX_BLOCKS => Err(format!(
                "This world has its {MAX_BLOCKS} blocks. Change one you have, or bring back one you took out, by its name: {}.",
                self.blocks
                    .iter()
                    .map(|b| b.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            None => {
                self.blocks.push(def);
                Ok(format!(
                    "{said} is a block of this world now: land changes paint with \"{name}\"."
                ))
            }
        }
    }

    /// Takes a block out, or puts a family of the ground's back to its own color.
    pub fn remove(&mut self, name: &str) -> Result<String, String> {
        let name = name.trim().to_lowercase();
        if let Some(family) = family(&name) {
            return match self.colors.remove(family) {
                Some(_) => Ok(format!("{family} is its own color again.")),
                None => Err(format!("{family} already has its own color.")),
            };
        }
        match self
            .blocks
            .iter_mut()
            .take(MAX_BLOCKS)
            .find(|b| b.name == name && !b.gone)
        {
            Some(block) => {
                block.gone = true;
                Ok(format!(
                    "{name} is out: nothing new is made of it, and what's there stays as it looks. Making a block called {name} again brings it back."
                ))
            }
            None => Err(format!(
                "There's no block called {name} here. {}",
                self.list()
            )),
        }
    }

    /// The blocks in words, for the chat and the agent.
    pub fn list(&self) -> String {
        let own: Vec<String> = self.own().map(|(_, b)| b.describe()).collect();
        let colors: Vec<String> = self
            .colors
            .iter()
            .map(|(family, hex)| format!("{family} {hex}"))
            .collect();
        let mut said = match own.is_empty() {
            true => "This world has no blocks of its own yet.".to_string(),
            false => format!("This world's own blocks: {}.", own.join("; ")),
        };
        if !colors.is_empty() {
            said += &format!(" The ground recolored: {}.", colors.join(", "));
        }
        said + &format!(
            " The ground's own: {}. Materials: {}.",
            FAMILIES.map(|(family, ..)| family).join(", "),
            material_names()
        )
    }

    /// The world's blocks as a page, for the agent's folder.
    pub fn describe(&self, terrain: Terrain) -> String {
        let palette = self.palette(terrain);
        let own: Vec<String> = self
            .own()
            .map(|(_, b)| format!("- {}", b.describe()))
            .collect();
        let ground: Vec<String> = FAMILIES
            .iter()
            .map(|(family, middle, _)| {
                let hex = palette
                    .find(middle)
                    .map(|id| Srgba::from(palette.entry(id).color).to_hex())
                    .unwrap_or_default()
                    .to_lowercase();
                format!(
                    "- {family} {}{}",
                    &hex[..hex.len().min(7)],
                    if self.colors.contains_key(*family) {
                        " (recolored; removeblock gives it its own color back)"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        format!(
            "# This world's blocks\n\nIts own, by the name a land change's paint and path take as \"material\":\n{}\n\n\
             The ground's own, by family (`block <family> <#rrggbb>` recolors one everywhere):\n{}\n\n\
             Materials: {}.\n\ndocs/blocks.md says how blocks and materials work.\n",
            if own.is_empty() {
                "(none yet: the `block` tool makes one, like `block lava #ff5a00 glow`)".to_string()
            } else {
                own.join("\n")
            },
            ground.join("\n"),
            material_names(),
        )
    }

    /// Every block of `terrain`'s ground as this world draws it: the ground's own with
    /// the world's colors, then a place for each block the world could have, so a voxel
    /// always has a look, whatever the rules say by now.
    pub fn palette(&self, terrain: Terrain) -> Palette {
        let mut palette = terrain.palette();
        for (family, hex) in &self.colors {
            if let Some(color) = rgb(hex) {
                recipe::tint(&mut palette, family, color);
            }
        }
        debug_assert_eq!(palette.entries.len(), FIRST as usize);
        let spare = palette.entry(block::STONE[1]).clone();
        for i in 0..MAX_BLOCKS {
            let surface = palette.surfaces.len();
            match self.blocks.get(i) {
                Some(block) => {
                    let color = block.color();
                    palette
                        .surfaces
                        .push(Finish::of(&block.material, &block.tune).surface(&block.name, color));
                    palette.entries.push(PaletteEntry {
                        name: block.name.clone(),
                        color,
                        surface,
                        variation: block.variation(),
                    });
                }
                None => {
                    palette
                        .surfaces
                        .push(MATERIALS[0].1.surface("spare", spare.color));
                    palette.entries.push(PaletteEntry {
                        name: String::new(),
                        surface,
                        ..spare.clone()
                    });
                }
            }
        }
        // Voxels no block is: nothing makes them, and a log may still say them.
        palette.entries.resize(
            256,
            PaletteEntry {
                name: String::new(),
                ..spare
            },
        );
        palette
    }
}

/// How blocks and materials work, for the agent.
pub fn doc() -> String {
    let materials: Vec<String> = MATERIALS
        .iter()
        .map(|(name, f, about)| {
            format!(
                "- {name}: {about} (roughness {}, metallic {}, glow {}, alpha {})",
                f.roughness, f.metallic, f.glow, f.alpha
            )
        })
        .collect();
    format!(
        r##"# Blocks and materials

## Materials

A material is how a color takes the light. Every color of a build's palette, and every
block, has one:

{materials}

A color may change its material's numbers, each on its own. Leave out (or null) what
should stay the material's:

- roughness 0-1: 0 a mirror, 1 chalk.
- metallic 0-1.
- glow 0-{MAX_GLOW}: light it gives off in its own color. 0.1 glows and keeps its
  color (lava, embers), 0.3 is pale and hot, 1 a lantern: white-hot. A wide field of it
  above 0.2 washes out; small things can take more.
- alpha {MIN_ALPHA}-1: 1 solid, less see-through.

In a build: {{"name":"ember","hex":"#ff7a2a","material":"matte","glow":0.1,"roughness":null,"metallic":null,"alpha":null}}
is rough stone that glows faintly; {{"name":"ice","hex":"#bfe6ff","material":"glass","alpha":0.8,...}}
is cloudy ice; "metal" with roughness 0.05 is chrome.

## Blocks

The ground is made of blocks. Each world has the ground's own, in families
({families}), and up to {MAX_BLOCKS} of its own.

- The `block` tool makes a block of this world's own, or changes one: a name, a color,
  a material and the same numbers. `block lava #ff5a00 glow` then
  {{"kind":"paint","material":"lava",...}} in a land change covers the ground with it,
  and a path can be made of it. Changing a block changes everything made of it.
- `block grass #7a4fb0` (a family's name) recolors that family everywhere in this
  world: the grass, the water, the leaves. Only the color; its shades follow.
- `removeblock` takes a block out (what's made of it stays), or gives a family its own
  color back. `blocks` lists them. `BLOCKS.md` is this world's, now.
- Only a world's admins change its blocks, it's for everyone there, and undo takes it
  back.

What blocks can't do yet: be walked through, hurt, or be told apart by a game's logic.
For those, make a build and give it logic.
"##,
        materials = materials.join("\n"),
        families = FAMILIES.map(|(family, ..)| family).join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(name: &str, hex: &str, material: &str) -> BlockDef {
        BlockDef {
            name: name.into(),
            hex: hex.into(),
            material: material.into(),
            tune: Tune::default(),
            variation: None,
            gone: false,
        }
    }

    #[test]
    fn a_material_takes_its_changes_and_stays_in_range() {
        let lava = check(
            "glow",
            &Tune {
                glow: Some(0.3),
                roughness: Some(0.9),
                ..Default::default()
            },
        )
        .expect("in range");
        assert_eq!((lava.glow, lava.roughness, lava.alpha), (0.3, 0.9, 1.0));
        // A lantern is as bright as it always was; an ember is a few times its color.
        assert_eq!(MATERIALS[4].1.light(), GLOW);
        assert!((10.0..16.0).contains(&lava.light()) && MATERIALS[0].1.light() == 0.0);
        assert!(
            check(
                "glow",
                &Tune {
                    glow: Some(2.0),
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(
            check("lava", &Tune::default())
                .unwrap_err()
                .contains("matte, glossy")
        );
        assert!(
            check(
                "glass",
                &Tune {
                    alpha: Some(0.0),
                    ..Default::default()
                }
            )
            .is_err()
        );
        // Whatever a log says, there's a finish.
        let odd = Finish::of(
            "nothing",
            &Tune {
                glow: Some(1e9),
                alpha: Some(f64::NAN),
                ..Default::default()
            },
        );
        assert_eq!((odd.glow, odd.alpha, odd.roughness), (MAX_GLOW, 1.0, 0.9));
        // A plain color serializes as it did before colors could be tuned.
        assert_eq!(serde_json::to_string(&Tune::default()).unwrap(), "{}");
    }

    #[test]
    fn a_world_makes_changes_and_removes_its_blocks() {
        let mut content = Content::default();
        assert_eq!(serde_json::to_string(&content).unwrap(), "{}");
        content
            .define(BlockDef {
                tune: Tune {
                    glow: Some(0.3),
                    ..Default::default()
                },
                ..block("Lava", "#FF5A00", "glow")
            })
            .expect("a block");
        content
            .define(block("ice", "#bfe6ff", "glass"))
            .expect("another");
        assert_eq!(
            (content.id("lava"), content.id("ice")),
            (Some(FIRST), Some(FIRST + 1))
        );
        assert_eq!(content.name_of(FIRST + 1), Some("ice"));

        // Taken out, a block keeps its place, so the others keep theirs.
        content.remove("lava").expect("it's there");
        assert_eq!(
            (content.id("lava"), content.id("ice")),
            (None, Some(FIRST + 1))
        );
        assert!(content.remove("lava").is_err());
        content
            .define(block("lava", "#ff0000", "glow"))
            .expect("back");
        assert_eq!(content.id("lava"), Some(FIRST));

        for bad in [
            block("Hot Lava", "#ff5a00", "glow"),
            block("lava2", "red", "glow"),
            block("slime", "#00ff00", "jelly"),
            block("grass_dark", "#00ff00", "matte"),
            block("tree", "#00ff00", "matte"),
        ] {
            assert!(content.define(bad.clone()).is_err(), "{bad:?}");
        }
        let back: Content =
            serde_json::from_str(&serde_json::to_string(&content).unwrap()).unwrap();
        assert_eq!(back, content);

        let mut full = Content::default();
        for i in 0..MAX_BLOCKS {
            full.define(block(&format!("b{i}"), "#101010", "matte"))
                .expect("room");
        }
        assert!(
            full.define(block("one_more", "#101010", "matte"))
                .unwrap_err()
                .contains("64 blocks")
        );
    }

    #[test]
    fn the_ground_takes_a_color_and_gives_it_back() {
        let terrain = Terrain::new(7, false);
        let mut content = Content::default();
        let usual = content.palette(terrain);
        assert!(content.define(block("grass", "#7a4fb0", "glow")).is_err());
        content
            .define(block("grass", "#7a4fb0", "matte"))
            .expect("a family");
        assert!(content.blocks.is_empty());
        let purple = content.palette(terrain);
        let of = |p: &Palette, name: &str| p.entries[p.find(name).unwrap() as usize].color;
        assert_ne!(of(&purple, "grass"), of(&usual, "grass"));
        assert_ne!(of(&purple, "grass_dark"), of(&usual, "grass_dark"));
        assert_eq!(of(&purple, "stone"), of(&usual, "stone"));
        content.remove("grass").expect("recolored");
        assert_eq!(of(&content.palette(terrain), "grass"), of(&usual, "grass"));
    }

    #[test]
    fn every_voxel_has_a_look() {
        let terrain = Terrain::new(7, false);
        let mut content = Content::default();
        let empty = content.palette(terrain);
        content
            .define(BlockDef {
                tune: Tune {
                    glow: Some(0.3),
                    ..Default::default()
                },
                ..block("lava", "#ff5a00", "glow")
            })
            .unwrap();
        content.define(block("ice", "#bfe6ff", "glass")).unwrap();
        let palette = content.palette(terrain);
        // The same places whatever blocks there are, so a mesh made before a change
        // still has its materials.
        assert_eq!(
            (palette.entries.len(), palette.surfaces.len()),
            (256, 3 + MAX_BLOCKS)
        );
        assert_eq!(
            (empty.entries.len(), empty.surfaces.len()),
            (256, 3 + MAX_BLOCKS)
        );
        assert_eq!(palette.find("lava"), Some(FIRST));
        assert_ne!(palette.surface_of(FIRST).emissive, LinearRgba::BLACK);
        assert!(!palette.is_opaque(FIRST + 1) && palette.is_opaque(FIRST));
        for id in 1..=255 {
            let entry = palette.entry(id);
            assert!(entry.surface < palette.surfaces.len());
        }
        // The ground's own are where they were.
        let usual = terrain.palette();
        for (id, entry) in usual.entries.iter().enumerate() {
            assert_eq!(
                (entry.name.as_str(), entry.surface),
                (
                    palette.entries[id].name.as_str(),
                    palette.entries[id].surface
                )
            );
        }
    }
}
