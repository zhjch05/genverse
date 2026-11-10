//! Shaders a package writes: small WGSL programs the engine runs on the GPU to draw
//! things no voxel can, like a glowing ring under a character, a spell's target circle or
//! a pool of light. The engine owns the slot (the mesh, the bindings, the pipeline); the
//! package writes one function, so a game's look is code, not a menu of styles.
//!
//! The slot so far is the decal: a flat square on the ground under an entity with a
//! `decal` component. Its file defines
//!
//! ```wgsl
//! fn decal(d: Decal) -> vec4<f32>
//! ```
//!
//! and returns the color at `d.uv` (-1..1 across the square; colors above 1 glow).
//! [`DECAL_PRELUDE`] has the rest. Every shader is checked here, by the same compiler
//! the game draws with, before a package can install: what's wrong comes back as the
//! compiler's message, at the line in the file. Shaders run on every player's GPU, so
//! they may not loop (a loop that runs long stalls everyone's game) or declare bindings
//! of their own.

use serde_json::Value;

/// The most shaders a package may have.
pub const MAX_SHADERS: usize = 16;
/// The most one shader's source may weigh.
pub const MAX_SHADER_BYTES: usize = 16 * 1024;

/// What a decal shader gets, declared after its source (WGSL doesn't mind the order),
/// so the compiler's line numbers are the file's own.
pub const DECAL_PRELUDE: &str = "
struct Decal {
    // Where on the decal: -1..1 in x and y, (0, 0) at its middle.
    uv: vec2<f32>,
    // Meters across.
    size: f32,
    // Seconds, for movement; it wraps every hour.
    time: f32,
    // The decal's \"color\" (alpha too), in linear light.
    color: vec4<f32>,
    // The decal's four \"params\", for the game to tune it live.
    params: vec4<f32>,
}
";

/// The shader every decal without one of its own draws: a soft ring in its color.
pub const DEFAULT_DECAL: &str = "// A soft ring in the decal's color.
fn decal(d: Decal) -> vec4<f32> {
    let r = length(d.uv);
    let ring = smoothstep(0.72, 0.84, r) * (1.0 - smoothstep(0.9, 1.0, r));
    let fill = 0.18 * (1.0 - smoothstep(0.0, 0.84, r));
    return vec4(d.color.rgb, (ring + fill) * d.color.a);
}
";

/// The entry point used to check a decal shader: it calls `decal` the way the game does.
const CHECK_ENTRY: &str = "
@fragment
fn genverse_check(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return decal(Decal(uv, 1.0, 0.0, vec4(1.0), vec4(0.0)));
}
";

/// Whether `name` can name a shader: 1-32 lowercase letters, digits and `_`.
pub fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len()) && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Everything wrong with a decal shader's source, phrased for the agent that wrote it
/// (`file` is where it lives, for the compiler's messages).
pub fn check_decal(file: &str, source: &str) -> Result<(), String> {
    if source.len() > MAX_SHADER_BYTES {
        return Err(format!("{file} is {} bytes; the most is {MAX_SHADER_BYTES}", source.len()));
    }
    let signature = "a decal shader defines fn decal(d: Decal) -> vec4<f32>";
    if !source.contains("fn decal") {
        return Err(format!("{file}: {signature}"));
    }
    let full = format!("{source}\n{DECAL_PRELUDE}{CHECK_ENTRY}");
    // Errors past the file's own lines are in what the engine adds: the call to decal.
    let own_lines = source.lines().count() + 1;
    let past_own = |line: Option<u32>| line.is_some_and(|l| l as usize > own_lines);
    let module = match naga::front::wgsl::parse_str(&full) {
        Ok(module) => module,
        Err(e) => {
            if past_own(e.location(&full).map(|l| l.line_number)) {
                return Err(format!("{file}: {signature}: {}", e.message()));
            }
            return Err(e.emit_to_string_with_path(&full, file));
        }
    };
    if !module.global_variables.is_empty() {
        return Err(format!(
            "{file}: shaders may not declare bindings or `var`s outside functions; use `const`, and the Decal's params for what changes"
        ));
    }
    if !module.overrides.is_empty() {
        return Err(format!("{file}: shaders may not declare `override`s; use `const`"));
    }
    if module.entry_points.len() != 1 {
        return Err(format!("{file}: shaders have no entry points (@vertex, @fragment, @compute) of their own; the engine calls fn decal"));
    }
    let loops = module.functions.iter().map(|(_, f)| &f.body).chain(module.entry_points.iter().map(|e| &e.function.body)).any(has_loop);
    if loops {
        return Err(format!(
            "{file}: shaders may not loop (for, while, loop): they run on every player's GPU, and a long loop stalls their game. Unroll it: write the few steps out"
        ));
    }
    let mut validator = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty());
    if let Err(e) = validator.validate(&module) {
        if e.spans().all(|(span, _)| past_own(Some(span.location(&full).line_number))) {
            return Err(format!("{file}: {signature}: {}", e.as_inner()));
        }
        return Err(e.emit_to_string_with_path(&full, file));
    }
    Ok(())
}

/// Whether whole WGSL compiles, as the compiler words it if not.
pub fn compile(source: &str) -> Result<(), String> {
    let module = naga::front::wgsl::parse_str(source).map_err(|e| e.emit_to_string(source))?;
    let mut validator = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty());
    validator.validate(&module).map(|_| ()).map_err(|e| e.emit_to_string(source))
}

fn has_loop(block: &naga::Block) -> bool {
    block.iter().any(|s| match s {
        naga::Statement::Loop { .. } => true,
        naga::Statement::Block(b) => has_loop(b),
        naga::Statement::If { accept, reject, .. } => has_loop(accept) || has_loop(reject),
        naga::Statement::Switch { cases, .. } => cases.iter().any(|c| has_loop(&c.body)),
        _ => false,
    })
}

/// A `decal` component, read: what to draw under an entity.
#[derive(Clone, Debug, PartialEq)]
pub struct DecalLook {
    /// One of the package's shaders; none for [`DEFAULT_DECAL`].
    pub shader: Option<String>,
    /// Meters across.
    pub size: f32,
    /// "#rrggbb".
    pub color: String,
    /// 0..1.
    pub alpha: f32,
    pub params: [f32; 4],
}

impl DecalLook {
    pub const HELP: &str = "\"decal\" is {\"shader\": one of the package's shaders (left out: a soft ring), \"size\": 0.2-32 m across, \"color\": \"#rrggbb\", \"alpha\": 0-1, \"params\": up to 4 numbers the shader reads}";

    /// The component's value, if it's well formed and names one of `shaders`.
    pub fn read(value: &Value, shaders: &[String]) -> Option<Self> {
        let obj = value.as_object()?;
        if !obj.keys().all(|k| ["shader", "size", "color", "alpha", "params"].contains(&k.as_str())) {
            return None;
        }
        let shader = match &value["shader"] {
            Value::Null => None,
            Value::String(s) if shaders.contains(s) => Some(s.clone()),
            _ => return None,
        };
        let size = match &value["size"] {
            Value::Null => 2.0,
            v => v.as_f64().filter(|s| (0.2..=32.0).contains(s))? as f32,
        };
        let color = match &value["color"] {
            Value::Null => "#ffffff".to_string(),
            Value::String(c) if super::present::is_color(c) => c.clone(),
            _ => return None,
        };
        let alpha = match &value["alpha"] {
            Value::Null => 1.0,
            v => v.as_f64().filter(|a| (0.0..=1.0).contains(a))? as f32,
        };
        let mut params = [0.0; 4];
        match &value["params"] {
            Value::Null => {}
            Value::Array(list) if list.len() <= 4 => {
                for (p, v) in params.iter_mut().zip(list) {
                    *p = v.as_f64().filter(|v| v.is_finite())? as f32;
                }
            }
            _ => return None,
        }
        Some(Self { shader, size, color, alpha, params })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_default_decal_compiles() {
        check_decal("default", DEFAULT_DECAL).unwrap();
    }

    #[test]
    fn mistakes_come_back_at_their_line() {
        let err = check_decal("shaders/halo.wgsl", "// halo\nfn decal(d: Decal) -> vec4<f32> {\n    return vec4(d.nope);\n}\n").unwrap_err();
        assert!(err.contains("shaders/halo.wgsl:3"), "{err}");
        let err = check_decal("shaders/halo.wgsl", "fn decal(d: Decal) -> f32 { return 1.0; }").unwrap_err();
        assert!(err.contains("fn decal(d: Decal) -> vec4<f32>"), "{err}");
        assert!(check_decal("x", "fn halo() -> f32 { return 1.0; }").unwrap_err().contains("fn decal"));
    }

    #[test]
    fn no_loops_bindings_or_entry_points() {
        let looped = "fn decal(d: Decal) -> vec4<f32> { var a = 0.0; for (var i = 0; i < 4; i++) { a += 0.1; } return vec4(a); }";
        assert!(check_decal("x", looped).unwrap_err().contains("may not loop"));
        let bound = "@group(0) @binding(0) var<uniform> u: vec4<f32>;\nfn decal(d: Decal) -> vec4<f32> { return u; }";
        assert!(check_decal("x", bound).unwrap_err().contains("bindings"));
        let entry = "fn decal(d: Decal) -> vec4<f32> { return d.color; }\n@fragment fn f() -> @location(0) vec4<f32> { return vec4(1.0); }";
        assert!(check_decal("x", entry).unwrap_err().contains("entry points"));
    }

    #[test]
    fn reads_the_component() {
        let shaders = vec!["halo".to_string()];
        let d = DecalLook::read(&json!({"shader": "halo", "size": 3, "color": "#ffcc00", "params": [1, 2]}), &shaders).unwrap();
        assert_eq!((d.size, d.params), (3.0, [1.0, 2.0, 0.0, 0.0]));
        assert_eq!(DecalLook::read(&json!({}), &shaders).unwrap().shader, None);
        assert!(DecalLook::read(&json!({"shader": "ring"}), &shaders).is_none());
        assert!(DecalLook::read(&json!({"size": 100}), &shaders).is_none());
        assert!(DecalLook::read(&json!({"glow": true}), &shaders).is_none());
    }
}
