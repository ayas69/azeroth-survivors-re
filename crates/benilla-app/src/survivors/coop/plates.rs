//! A friend's name and health over their hero's head, so the party can find each other in the
//! crowd; grey once they have fallen.

use std::collections::HashMap;

use benilla_world::view::WorldCamera;
use bevy::prelude::*;

use super::{Coop, Mate};
use crate::survivors::hud::HudFont;
use crate::survivors::Phase;

const PLATE_W: f32 = 96.0;
const BAR_H: f32 = 7.0;
/// Yards above a hero's feet the plate floats at.
const HEAD: f32 = 2.7;

/// One plate's nodes: its root, health fill and name.
#[derive(Clone, Copy)]
pub(super) struct Plate {
    root: Entity,
    fill: Entity,
    name: Entity,
}

fn spawn_plate(commands: &mut Commands, font: &Handle<Font>) -> Plate {
    let mut fill = Entity::PLACEHOLDER;
    let mut name = Entity::PLACEHOLDER;
    let root = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                width: Val::Px(PLATE_W),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: Val::Px(2.0),
                ..default()
            },
            GlobalZIndex(390),
            Visibility::Hidden,
        ))
        .with_children(|p| {
            name = p
                .spawn((
                    Text::new(""),
                    TextFont {
                        font: font.clone(),
                        font_size: 14.0,
                        ..default()
                    },
                    TextColor(Color::srgb(0.55, 1.0, 0.55)),
                    TextShadow::default(),
                ))
                .id();
            p.spawn((
                Node {
                    width: Val::Px(PLATE_W),
                    height: Val::Px(BAR_H),
                    border: UiRect::all(Val::Px(1.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.7)),
                BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.9)),
            ))
            .with_children(|bar| {
                fill = bar
                    .spawn((
                        Node {
                            width: Val::Percent(100.0),
                            height: Val::Percent(100.0),
                            ..default()
                        },
                        BackgroundColor(Color::srgb(0.1, 0.85, 0.1)),
                    ))
                    .id();
            });
        })
        .id();
    Plate { root, fill, name }
}

/// Keep one plate per friend's hero, seated over its head.
#[allow(clippy::too_many_arguments)]
pub(super) fn mate_plates(
    mut commands: Commands,
    mut plates: Local<HashMap<u32, Plate>>,
    coop: Res<Coop>,
    phase: Res<State<Phase>>,
    font: Option<Res<HudFont>>,
    ui_scale: Res<UiScale>,
    mates: Query<(&Mate, &Transform)>,
    cams: Query<(&Camera, &Transform), With<WorldCamera>>,
    mut nodes: Query<&mut Node>,
    mut vis: Query<&mut Visibility>,
    mut texts: Query<(&mut Text, &mut TextColor)>,
    mut colors: Query<&mut BackgroundColor>,
) {
    let live = coop.on && *phase.get() != Phase::Menu;
    // Gone friends (or the run itself) take their plates with them.
    plates.retain(|id, plate| {
        let keep = live && coop.peers.contains_key(id);
        if !keep {
            commands.entity(plate.root).despawn();
        }
        keep
    });
    if !live {
        return;
    }
    let Some(font) = font else { return };
    let cam = cams.single().ok();
    let s = ui_scale.0.max(0.01);
    for (mate, tf) in &mates {
        let Some(peer) = coop.peers.get(&mate.0) else {
            continue;
        };
        let plate = *plates
            .entry(mate.0)
            .or_insert_with(|| spawn_plate(&mut commands, &font.0));
        let seat = cam.and_then(|(c, ct)| {
            c.world_to_viewport(
                &GlobalTransform::from(*ct),
                tf.translation + Vec3::Y * HEAD,
            )
            .ok()
        });
        let Ok(mut v) = vis.get_mut(plate.root) else {
            // Spawned this frame: seated from the next.
            continue;
        };
        let Some(at) = seat else {
            *v = Visibility::Hidden;
            continue;
        };
        *v = Visibility::Inherited;
        if let Ok(mut n) = nodes.get_mut(plate.root) {
            n.left = Val::Px(at.x / s - PLATE_W / 2.0);
            n.top = Val::Px(at.y / s - 24.0);
        }
        let frac = if peer.dead {
            0.0
        } else {
            (peer.hp / peer.max.max(1.0)).clamp(0.0, 1.0)
        };
        if let Ok(mut n) = nodes.get_mut(plate.fill) {
            n.width = Val::Percent(frac * 100.0);
        }
        if let Ok(mut c) = colors.get_mut(plate.fill) {
            let want = if frac < 0.3 {
                Color::srgb(0.9, 0.15, 0.1)
            } else {
                Color::srgb(0.1, 0.85, 0.1)
            };
            if c.0 != want {
                c.0 = want;
            }
        }
        if let Ok((mut text, mut color)) = texts.get_mut(plate.name) {
            let label = if peer.dead {
                format!("{} (fallen)", peer.name)
            } else {
                format!("{}  {}", peer.name, peer.level)
            };
            if text.0 != label {
                text.0 = label;
            }
            let want = if peer.dead {
                Color::srgb(0.6, 0.6, 0.6)
            } else {
                Color::srgb(0.55, 1.0, 0.55)
            };
            if color.0 != want {
                color.0 = want;
            }
        }
    }
}
