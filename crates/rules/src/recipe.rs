//! A world's landscape as a recipe: a few numbers the terrain generator shapes the
//! ground from (how much sea, how high the hills and mountains, how many trees, what
//! covers the ground, its colors), instead of one landscape for every world.
//!
//! A recipe is data, never code, and whole numbers only, so every machine makes exactly
//! the same ground from it. It's set when a world is made and kept in the world's first
//! line; it never changes after, because a world's edits are voxels laid over that
//! ground. The default recipe is the landscape worlds had before there were recipes.

use bevy_color::prelude::*;
use serde::{Deserialize, Serialize};

use crate::voxel::Palette;

/// What covers open ground.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cover {
    /// Grass over dirt, with tufts and flowers.
    #[default]
    Grass,
    /// Sand all the way down to the stone: a desert, a beach world.
    Sand,
    /// Snow over dirt.
    Snow,
    /// Bare rock.
    Stone,
    /// Bare earth.
    Dirt,
}

/// The colors of the ground's blocks, each `#rrggbb`; `None` is the usual color.
/// Lighter and darker shades of each follow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    #[serde(with = "hex", skip_serializing_if = "Option::is_none")]
    pub grass: Option<[u8; 3]>,
    #[serde(with = "hex", skip_serializing_if = "Option::is_none")]
    pub leaves: Option<[u8; 3]>,
    #[serde(with = "hex", skip_serializing_if = "Option::is_none")]
    pub bark: Option<[u8; 3]>,
    #[serde(with = "hex", skip_serializing_if = "Option::is_none")]
    pub water: Option<[u8; 3]>,
    #[serde(with = "hex", skip_serializing_if = "Option::is_none")]
    pub sand: Option<[u8; 3]>,
    #[serde(with = "hex", skip_serializing_if = "Option::is_none")]
    pub dirt: Option<[u8; 3]>,
    #[serde(with = "hex", skip_serializing_if = "Option::is_none")]
    pub stone: Option<[u8; 3]>,
    #[serde(with = "hex", skip_serializing_if = "Option::is_none")]
    pub snow: Option<[u8; 3]>,
}

impl Colors {
    const NONE: Self = Self { grass: None, leaves: None, bark: None, water: None, sand: None, dirt: None, stone: None, snow: None };

    fn is_default(&self) -> bool {
        *self == Self::NONE
    }
}

/// A landscape. Amounts are percents of the usual (100 is the usual); leanings run
/// from -100 to 100 around the usual 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Recipe {
    /// How wide everything is: 50 packs coasts, hills and ranges twice as close, 200
    /// spreads them twice as far. 25 to 400.
    pub scale: u16,
    /// More land (up to 100: no sea at all) or more sea (down to -100: scattered
    /// islands, then open water).
    pub land: i8,
    /// How high hills and valleys roll. 0 (level) to 300.
    pub hills: u16,
    /// How high mountain ranges rise. 0 (none) to 300.
    pub mountains: u16,
    /// How much of the land is mountain range: 100 nearly all of it, -100 none.
    pub ranges: i8,
    /// How rough the ground is up close: rocks and bumps. 0 (smooth) to 300.
    pub roughness: u16,
    /// How many trees. 0 (none) to 400 (forest nearly everywhere trees can grow).
    pub trees: u16,
    /// How many grass tufts and flowers, where grass covers the ground. 0 to 1000.
    pub plants: u16,
    /// How far down the snow comes: 100 covers everything above the sea, -100 leaves
    /// even the peaks bare.
    pub snow: i8,
    /// What covers open ground.
    pub ground: Cover,
    /// Whether the low ground is under water. Without it, the sea floor is dry land:
    /// basins and canyons.
    pub water: bool,
    #[serde(skip_serializing_if = "Colors::is_default")]
    pub colors: Colors,
}

impl Default for Recipe {
    fn default() -> Self {
        Self::NATURAL
    }
}

impl Recipe {
    /// The landscape of worlds made before there were recipes, exactly.
    pub const NATURAL: Self = Self {
        scale: 100,
        land: 0,
        hills: 100,
        mountains: 100,
        ranges: 0,
        roughness: 100,
        trees: 100,
        plants: 100,
        snow: 0,
        ground: Cover::Grass,
        water: true,
        colors: Colors::NONE,
    };

    pub fn is_default(&self) -> bool {
        *self == Self::NATURAL
    }

    /// The recipe with every amount inside what the generator takes.
    pub fn bounded(self) -> Self {
        Self {
            scale: self.scale.clamp(25, 400),
            land: self.land.clamp(-100, 100),
            hills: self.hills.min(300),
            mountains: self.mountains.min(300),
            ranges: self.ranges.clamp(-100, 100),
            roughness: self.roughness.min(300),
            trees: self.trees.min(400),
            plants: self.plants.min(1000),
            snow: self.snow.clamp(-100, 100),
            ..self
        }
    }

    /// A recipe by name, for `/newworld <name> <kind>` and as a start for the agent's
    /// own (`"like"`).
    pub fn named(name: &str) -> Option<Self> {
        let n = Self::NATURAL;
        let hex = |text: &str| hex::parse(text);
        Some(match name.trim().to_lowercase().as_str() {
            "natural" => n,
            "islands" => Self { scale: 60, land: -45, hills: 80, mountains: 40, ..n },
            "mountains" => Self { land: 45, hills: 130, mountains: 190, ranges: 60, roughness: 140, snow: 25, ..n },
            "plains" => Self { land: 70, hills: 30, mountains: 0, ranges: -100, roughness: 40, trees: 40, plants: 250, ..n },
            "forest" => Self { land: 50, hills: 70, mountains: 40, trees: 320, plants: 160, ..n },
            "desert" => Self { land: 85, hills: 70, mountains: 70, roughness: 50, trees: 0, plants: 0, snow: -100, ground: Cover::Sand, ..n },
            "tundra" => Self { land: 40, hills: 60, mountains: 80, trees: 25, plants: 0, snow: 100, ground: Cover::Snow, colors: Colors { leaves: hex("#3f6b4a"), water: hex("#6aa0c0"), ..Colors::NONE }, ..n },
            "badlands" => Self {
                land: 30,
                hills: 120,
                mountains: 130,
                ranges: 30,
                roughness: 220,
                trees: 0,
                plants: 0,
                snow: -100,
                ground: Cover::Dirt,
                water: false,
                colors: Colors { dirt: hex("#b5653a"), stone: hex("#c08a5a"), sand: hex("#d9a066"), ..Colors::NONE },
                ..n
            },
            "alien" => Self {
                land: 20,
                hills: 140,
                mountains: 120,
                trees: 180,
                plants: 300,
                colors: Colors { grass: hex("#8a5fc4"), leaves: hex("#e26fb0"), bark: hex("#3b3350"), water: hex("#3fd0a0"), sand: hex("#cfc2e8"), ..Colors::NONE },
                ..n
            },
            _ => return None,
        })
    }

    /// The names [`Self::named`] knows.
    pub const NAMES: [&'static str; 9] = ["natural", "islands", "mountains", "plains", "forest", "desert", "tundra", "badlands", "alien"];

    /// Reads a recipe as the agent gives it: a named one (`"desert"`), or an object of
    /// the fields that differ from the usual, optionally starting from a named one
    /// (`{"like": "islands", "trees": 300, "colors": {"water": "#3fd0a0"}}`).
    pub fn read(given: &serde_json::Value) -> Result<Self, String> {
        let unknown = |name: &str| format!("there's no landscape called \"{name}\" ({})", Self::NAMES.join(", "));
        let fields = match given {
            serde_json::Value::String(name) => return Self::named(name).ok_or_else(|| unknown(name)),
            serde_json::Value::Object(fields) => fields,
            _ => return Err("a landscape is a name or an object of its fields".into()),
        };
        let start = match fields.get("like").and_then(|like| like.as_str()) {
            Some(name) => Self::named(name).ok_or_else(|| unknown(name))?,
            None => Self::NATURAL,
        };
        // The given fields over the start's, colors by color.
        let mut all = serde_json::to_value(Self { colors: Colors::NONE, ..start }).map_err(|e| e.to_string())?;
        let mut colors = serde_json::to_value(start.colors).map_err(|e| e.to_string())?;
        for (key, value) in fields {
            match (key.as_str(), value) {
                ("like", _) => {}
                ("colors", serde_json::Value::Object(given)) => {
                    for (color, hex) in given {
                        colors[color.as_str()] = hex.clone();
                    }
                }
                _ => all[key.as_str()] = value.clone(),
            }
        }
        all["colors"] = colors;
        let recipe: Self = serde_json::from_value(all).map_err(|e| format!("that isn't a landscape: {e}"))?;
        Ok(recipe.bounded())
    }

    /// The recipe in a line, for lists of worlds: what differs from the usual.
    pub fn describe(&self) -> String {
        if self.is_default() {
            return "the usual landscape".into();
        }
        if let Some(name) = Self::NAMES.iter().find(|name| Self::named(name) == Some(*self)) {
            return name.to_string();
        }
        serde_json::to_value(self)
            .ok()
            .and_then(|all| {
                let usual = serde_json::to_value(Self::NATURAL).ok()?;
                let differs: Vec<String> = all.as_object()?.iter().filter(|(k, v)| usual.get(k.as_str()) != Some(v)).map(|(k, v)| format!("{k} {v}")).collect();
                Some(differs.join(", "))
            })
            .unwrap_or_default()
    }

    /// The terrain's palette with this recipe's colors.
    pub fn recolor(&self, palette: &mut Palette) {
        let c = &self.colors;
        let colors = [c.grass, c.leaves, c.bark, c.water, c.sand, c.dirt, c.stone, c.snow];
        for ((family, ..), color) in FAMILIES.iter().zip(colors) {
            if let Some(color) = color {
                tint(palette, family, color);
            }
        }
    }
}

/// What a recipe is, for the agent: its fields and the named ones.
pub const DOC: &str = r##"# Landscapes

A new world's ground comes from a recipe: a few whole numbers the game shapes the
landscape from. It's set when the world is made (the newworld tool's "terrain") and
never changes after: what's built there stands on that ground. A flat world ignores the
heights but keeps the cover and the colors.

"terrain" is a name, or an object of the fields that differ from the usual, optionally
starting from a name with "like":

    "desert"
    {"like": "islands", "trees": 300, "colors": {"water": "#3fd0a0"}}
    {"land": 100, "hills": 20, "mountains": 0, "ground": "snow", "trees": 0}

Named: natural (the usual: coasts, hills, some ranges, groves), islands, mountains,
plains, forest, desert, tundra, badlands (dry red canyons), alien (purple grass, pink
trees, a green sea).

Fields (amounts are percents of the usual, 100; leanings run -100 to 100 around 0):
- scale (25-400): how wide everything is. 50 packs coasts, hills and ranges twice as
  close; 200 spreads them twice as far.
- land (-100 to 100): more land or more sea. 100 has no sea at all; -45 is islands;
  -100 is open water.
- hills (0-300): how high hills and valleys roll. 0 is level ground.
- mountains (0-300): how high ranges rise (the usual is up to about 100 m). 0 is none.
- ranges (-100 to 100): how much of the land is mountain range.
- roughness (0-300): rocks and bumps up close. 0 is smooth.
- trees (0-400): 0 none, 100 groves and meadows, 400 forest nearly everywhere.
- plants (0-1000): grass tufts and flowers, where grass covers the ground.
- snow (-100 to 100): how far down the snow comes. 100 covers everything above the
  sea; -100 leaves even peaks bare.
- ground: grass, sand, snow, stone or dirt: what covers open ground.
- water (true/false): false leaves the low ground dry: basins and canyons where the
  sea would be.
- colors: {"grass", "leaves", "bark", "water", "sand", "dirt", "stone", "snow"}, each
  "#rrggbb": the middle shade of those blocks; lighter and darker shades follow. Leave
  out what stays as usual.
"##;

/// `Option<[u8; 3]>` as `"#rrggbb"`.
mod hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn parse(text: &str) -> Option<[u8; 3]> {
        let digits = text.trim().strip_prefix('#')?;
        if digits.len() != 6 || !digits.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).ok();
        Some([byte(0)?, byte(2)?, byte(4)?])
    }

    pub fn serialize<S: Serializer>(color: &Option<[u8; 3]>, to: S) -> Result<S::Ok, S::Error> {
        match color {
            Some([r, g, b]) => to.serialize_str(&format!("#{r:02x}{g:02x}{b:02x}")),
            None => to.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(from: D) -> Result<Option<[u8; 3]>, D::Error> {
        match Option::<String>::deserialize(from)? {
            None => Ok(None),
            Some(text) => parse(&text).map(Some).ok_or_else(|| serde::de::Error::custom(format!("\"{text}\" isn't a color like #7fb04f"))),
        }
    }
}

/// The ground's own blocks, by family: its name, the block that's its middle shade, and
/// every block of it. In the order of [`Colors`].
pub const FAMILIES: [(&str, &str, &[&str]); 8] = [
    ("grass", "grass", &["grass_light", "grass", "grass_dark", "tuft"]),
    ("leaves", "leaves", &["leaves_light", "leaves", "leaves_dark"]),
    ("bark", "bark", &["bark", "bark_dark"]),
    ("water", "water", &["water"]),
    ("sand", "sand", &["sand", "sand_dark"]),
    ("dirt", "dirt", &["dirt", "dirt_dark"]),
    ("stone", "stone", &["stone_light", "stone", "stone_dark"]),
    ("snow", "snow", &["snow"]),
];

/// Gives a family of the ground's blocks a color: it replaces the middle shade, and the
/// lighter and darker shades keep their distance from it.
pub fn tint(palette: &mut Palette, family: &str, [r, g, b]: [u8; 3]) {
    let Some((_, middle, names)) = FAMILIES.iter().find(|(name, ..)| *name == family) else { return };
    let Some(from) = palette.entries.iter().find(|e| e.name == *middle).map(|e| e.color) else { return };
    let to: LinearRgba = Srgba::rgb_u8(r, g, b).into();
    let shade = |was: f32, from: f32, to: f32| if from > 0.0 { (was / from * to).clamp(0.0, 1.0) } else { to };
    for entry in palette.entries.iter_mut().filter(|e| names.contains(&e.name.as_str())) {
        let was = entry.color;
        entry.color = LinearRgba::new(shade(was.red, from.red, to.red), shade(was.green, from.green, to.green), shade(was.blue, from.blue, to.blue), was.alpha);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_usual_recipe_leaves_nothing_in_a_log() {
        // A world made before recipes reads as the usual one, and writes nothing new.
        assert_eq!(serde_json::from_value::<Recipe>(json!({})).unwrap(), Recipe::NATURAL);
        assert!(Recipe::default().is_default());
        let desert = Recipe::named("desert").unwrap();
        let back: Recipe = serde_json::from_str(&serde_json::to_string(&desert).unwrap()).unwrap();
        assert_eq!(back, desert);
        assert!(!serde_json::to_string(&desert).unwrap().contains("colors"));
    }

    #[test]
    fn recipes_are_read_by_name_or_by_what_differs() {
        for name in Recipe::NAMES {
            let recipe = Recipe::named(name).unwrap_or_else(|| panic!("{name} is named"));
            assert_eq!(recipe, recipe.bounded(), "{name} is inside the bounds");
            assert_eq!(Recipe::read(&json!(name)), Ok(recipe));
            assert_eq!(recipe.describe(), if name == "natural" { "the usual landscape" } else { name });
        }
        let mine = Recipe::read(&json!({"like": "islands", "trees": 300, "colors": {"water": "#3fd0a0"}})).unwrap();
        assert_eq!((mine.land, mine.trees, mine.colors.water), (-45, 300, Some([0x3f, 0xd0, 0xa0])));
        // A color given over a named one's keeps the others.
        let alien = Recipe::read(&json!({"like": "alien", "colors": {"grass": "#ff0000"}})).unwrap();
        assert_eq!((alien.colors.grass, alien.colors.leaves), (Some([255, 0, 0]), Recipe::named("alien").unwrap().colors.leaves));
        // Out of range is brought in; what isn't a recipe says so.
        assert_eq!(Recipe::read(&json!({"scale": 9000, "hills": 9000})).map(|r| (r.scale, r.hills)), Ok((400, 300)));
        assert!(Recipe::read(&json!({"heigth": 3})).is_err());
        assert!(Recipe::read(&json!({"colors": {"grass": "green"}})).is_err());
        assert!(Recipe::read(&json!("moon")).unwrap_err().contains("islands"));
        assert!(mine.describe().contains("trees 300"));
    }

    #[test]
    fn colors_move_every_shade_of_their_blocks() {
        let usual = crate::terrain::palette();
        let mut tinted = crate::terrain::palette();
        Recipe::read(&json!({"colors": {"grass": "#8a5fc4"}})).unwrap().recolor(&mut tinted);
        let color = |p: &Palette, name: &str| p.entries.iter().find(|e| e.name == name).map(|e| e.color).unwrap();
        let purple: LinearRgba = Srgba::rgb_u8(0x8a, 0x5f, 0xc4).into();
        assert_eq!(color(&tinted, "grass"), purple);
        // The light shade is still lighter, and what wasn't named stays.
        assert!(color(&tinted, "grass_light").blue > purple.blue && color(&tinted, "grass_dark").blue < purple.blue);
        assert_ne!(color(&tinted, "tuft"), color(&usual, "tuft"));
        assert_eq!(color(&tinted, "leaves"), color(&usual, "leaves"));
        // No colors: the palette is untouched.
        let mut same = crate::terrain::palette();
        Recipe::NATURAL.recolor(&mut same);
        assert!(same.entries.iter().zip(&usual.entries).all(|(a, b)| a.color == b.color));
    }
}
