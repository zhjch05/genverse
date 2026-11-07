//! Loads `.vox.ron` model files. The format and rasterization live in
//! [`cubelark_rules::voxel::model`]; with Bevy's `file_watcher` feature, saving the file
//! re-rasterizes the model live.

use bevy::{
    asset::{AssetLoader, LoadContext, io::Reader},
    ecs::error::BevyError,
    prelude::*,
};
use cubelark_rules::voxel::model::{ModelDef, build};

use super::ModelData;

/// A loaded model, as a Bevy asset.
#[derive(Asset, TypePath, Clone, Deref)]
pub struct VoxelModelAsset(pub ModelData);

#[derive(Default, TypePath)]
pub struct VoxelModelLoader;

impl AssetLoader for VoxelModelLoader {
    type Asset = VoxelModelAsset;
    type Settings = ();
    type Error = BevyError;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &(),
        _load_context: &mut LoadContext<'_>,
    ) -> Result<VoxelModelAsset, BevyError> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        let def: ModelDef = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .from_bytes(&bytes)?;
        Ok(VoxelModelAsset(build(def).map_err(|e| BevyError::from(std::io::Error::other(e)))?))
    }

    fn extensions(&self) -> &[&str] {
        &["vox.ron"]
    }
}
