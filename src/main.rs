#[cfg(not(target_arch = "wasm32"))]
mod agent;
mod graphics;
mod player;
mod scene;
mod voxel;
mod world;

use bevy::prelude::*;

fn main() -> AppExit {
    let mut window = Window {
        title: "Cubewish".into(),
        // On the web, size the canvas to the page instead of a fixed resolution.
        fit_canvas_to_parent: true,
        ..default()
    };
    // On the desktop it opens maximized, filling the screen but keeping its title bar.
    window.set_maximized(true);
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin { primary_window: Some(window), ..default() }))
        .add_plugins((
            voxel::VoxelPlugin,
            world::WorldPlugin,
            player::PlayerPlugin,
            scene::ScenePlugin,
            graphics::GraphicsPlugin,
        ));
    // Local agents (Claude Code, Codex CLI) only exist on desktop.
    #[cfg(not(target_arch = "wasm32"))]
    app.add_plugins(agent::AgentPlugin);
    app.run()
}
