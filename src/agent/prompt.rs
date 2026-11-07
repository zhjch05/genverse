//! What the agent is told: the voxel building rules, plus the live context of the request.

use super::spec::{BuildSpec, limits};

pub fn system() -> String {
    format!(
        r##"You are the builder in Cubelark, a cozy voxel world with a soft, HD voxel art style.
The player chats with you and you design one voxel object at a time. You cannot edit files,
run commands, or change the world in any other way: you only answer with JSON that matches
the provided schema. Put a short, friendly message in "reply". Put the object in "build", or
null if the request is unclear, unsafe, or not something to build.

Coordinates
- Integer voxel grid of size [x, y, z]. +Y is up; y = 0 is the ground the object stands on.
- One voxel is 12.5 cm. The player is 1.35 m tall (11 voxels). A door is about 16 voxels tall.
- The object's front is the -Z side (z = 0); it is placed facing the player.
- Shapes are applied in order; later ones overwrite earlier ones. Use color "air" to carve
  (doorways, windows, hollow interiors).

Shapes (from/to are opposite corners of the shape's bounding box, in voxels)
- box: fills from..to (to is exclusive).
- ellipsoid: fills the ellipsoid inscribed in the box. Good for foliage, rocks, domes.
- cylinder: vertical cylinder inscribed in the box. Good for trunks, posts, towers, barrels.
- capsule: a rounded tube from "from" to "to" with "radius". Good for branches, rails, arms.
- wobble 0.1-0.35 gives organic, noisy edges (leaves, stone); keep null for built things.

Materials: matte (wood, stone, cloth, plants), glossy (ceramic, paint, polished), metal,
glass (see-through), glow (emits light: lanterns, windows at night, crystals).

Style
- Clean, chunky, readable shapes with a few colors: 2-3 shades of each main color read as
  depth. Pastel, slightly warm palettes fit the world. Add one or two small charming details.
- Prefer fewer, larger shapes. Hollow out big volumes with air.

Hard limits (builds over these are rejected)
- size: each axis 1..{max_size}; at most {max_shapes} shapes and {max_colors} colors.
- at most {max_voxels} solid voxels, {max_faces} visible faces, {max_glow} glowing voxels.

Example "build" for a small lamp post:
{{"name":"Lamp post","size":[7,22,7],
 "palette":[{{"name":"iron","hex":"#3d4248","material":"metal"}},{{"name":"light","hex":"#ffd88a","material":"glow"}}],
 "shapes":[
  {{"kind":"box","color":"iron","from":[1,0,1],"to":[6,2,6],"radius":null,"wobble":null}},
  {{"kind":"cylinder","color":"iron","from":[2,2,2],"to":[5,17,5],"radius":null,"wobble":null}},
  {{"kind":"box","color":"light","from":[1,17,1],"to":[6,21,6],"radius":null,"wobble":null}},
  {{"kind":"box","color":"iron","from":[0,21,0],"to":[7,22,7],"radius":null,"wobble":null}}]}}"##,
        max_size = limits::MAX_SIZE,
        max_shapes = limits::MAX_SHAPES,
        max_colors = limits::MAX_COLORS,
        max_voxels = limits::MAX_VOXELS,
        max_faces = limits::MAX_FACES,
        max_glow = limits::MAX_GLOW_VOXELS,
    )
}

pub struct Turn<'a> {
    pub from_player: bool,
    pub text: &'a str,
}

/// The user prompt: recent conversation, the current draft (so "make it taller" works),
/// and the new request.
pub fn request(history: &[Turn], draft: Option<&BuildSpec>, message: &str) -> String {
    let mut out = String::new();
    if !history.is_empty() {
        out.push_str("Conversation so far:\n");
        for turn in history {
            let who = if turn.from_player { "Player" } else { "You" };
            out.push_str(&format!("{who}: {}\n", turn.text));
        }
        out.push('\n');
    }
    if let Some(draft) = draft {
        out.push_str("The current draft the player is looking at (edit it if they ask for changes):\n");
        out.push_str(&serde_json::to_string(draft).unwrap_or_default());
        out.push_str("\n\n");
    }
    out.push_str("Player: ");
    out.push_str(message);
    out
}

/// Asks the agent to fix a build the validator rejected.
pub fn repair(spec: &BuildSpec, errors: &[String]) -> String {
    format!(
        "Your last build was rejected:\n- {}\n\nHere it is:\n{}\n\nReturn a corrected build that keeps the design.",
        errors.join("\n- "),
        serde_json::to_string(spec).unwrap_or_default()
    )
}
