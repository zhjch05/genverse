mod scene;
mod voxel;

use bevy::prelude::*;

fn main() -> AppExit {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Cubelark".into(),
                // On the web, size the canvas to the page instead of a fixed resolution.
                fit_canvas_to_parent: true,
                ..default()
            }),
            ..default()
        }))
        .add_plugins((voxel::VoxelPlugin, scene::ScenePlugin))
        .run()
}
