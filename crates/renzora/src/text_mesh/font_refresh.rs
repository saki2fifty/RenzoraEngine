//! Font asset revisions shared by consumers of generated glyph geometry.

use bevy::ecs::{message::MessageCursor, system::RunSystemOnce};
use bevy::prelude::*;
use bevy::text::{load_font_assets_into_font_collection, Font, FontCx};

/// Changes after the font collection has consumed an asset change.
#[derive(Resource, Default)]
pub struct FontAssetRevision(pub u64);

/// Install shared font-asset tracking once, including same-ID byte replacement.
pub fn ensure_font_asset_tracking(app: &mut App) {
    if app.world().contains_resource::<FontAssetRevision>() {
        return;
    }
    app.init_resource::<FontAssetRevision>()
        .add_message::<AssetEvent<Font>>()
        .add_systems(
            PostUpdate,
            refresh_fonts
                .before(load_font_assets_into_font_collection)
                .run_if(bevy::ecs::schedule::common_conditions::on_message::<AssetEvent<Font>>),
        );
}

fn refresh_fonts(world: &mut World, mut cursor: Local<MessageCursor<AssetEvent<Font>>>) {
    let Some(events) = world.get_resource::<Messages<AssetEvent<Font>>>() else {
        return;
    };
    let mut changed = false;
    let mut modified = false;
    for event in cursor.read(events) {
        changed |= matches!(
            event,
            AssetEvent::Added { .. } | AssetEvent::Modified { .. } | AssetEvent::Removed { .. }
        );
        modified |= matches!(event, AssetEvent::Modified { .. });
    }
    if modified {
        // Bevy 0.19's loader remembers only asset IDs, so replacing the bytes
        // under an existing ID does not refresh its collection. Reuse the public
        // loader with fresh local state only on this rare path, not each frame.
        world.resource_mut::<FontCx>().collection.clear();
        if let Err(error) = world.run_system_once(load_font_assets_into_font_collection) {
            warn!("could not refresh modified fonts: {error}");
            return;
        }
    }
    if changed {
        let mut revision = world.resource_mut::<FontAssetRevision>();
        revision.0 = revision.0.wrapping_add(1);
    }
}
