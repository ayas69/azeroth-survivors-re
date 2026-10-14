//! Co-op: friends on the same battleground share one horde. Each tab still runs its own hero —
//! its spells, cards, level and health — exactly as alone; what is shared is the world. The first
//! player in a room hosts it: its tab spawns and moves the horde, lands every blow on it, and
//! drops the loot, and the others mirror that from its messages. A guest's spells strike the
//! mirrored enemies and the blows go to the host; the host's enemies strike guests by message.
//! Experience is shared: any wisp taken counts for everyone, each at their own growth. A level-up
//! card on anyone's screen holds the whole run, and a fallen hero rises beside a standing ally
//! after [`REVIVE_SECS`]; the run ends when nobody is left standing.
//!
//! The relay is `wenilla-host`'s `/coop/{room}`, the room being the battleground, so picking the
//! same one is how friends meet. In the browser it is the page's own host; on the desktop,
//! `WOW_COOP=ws://host:port` turns it on. `WOW_SOLO` keeps a browser run to itself.

mod link;
mod plates;

use std::collections::{HashMap, HashSet};

use benilla_world::world_point::WorldPoint;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use super::abilities::Hit;
use super::data::{self, Effect, EnemyDef};
use super::enemies::{Enemy, Gem};
use super::hero::{self, Hero};
use super::units::{self, flat, yaw_toward, Fx, COLOR_HURT};
use super::{Phase, Run, Selection};
use crate::creature_anim::{move_flags, MovementState};
use crate::net::{Guid, ObjectStore};

/// Seconds a fallen hero lies before rising beside an ally.
pub(super) const REVIVE_SECS: f32 = 12.0;
/// How often a tab tells the others where its hero is, and the host where the horde is.
const POS_EVERY: f32 = 0.066;
const SNAP_EVERY: f32 = 0.1;
const VITALS_EVERY: f32 = 0.25;

/// A message between tabs.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "k")]
pub(super) enum Msg {
    /// Who I am: my name and my hero's look.
    Hello {
        name: String,
        class: usize,
        display: u32,
        race: u8,
        sex: u8,
    },
    /// Where my hero is.
    Pos {
        p: [f32; 3],
        yaw: f32,
        spd: f32,
        fl: u32,
        vy: f32,
    },
    /// How my hero is doing; `busy` while a card (or the pause menu) is on my screen.
    Vit {
        hp: f32,
        max: f32,
        lvl: u32,
        dead: bool,
        busy: bool,
    },
    /// Host: these enemies came in.
    Spawn { e: Vec<NetSpawn> },
    /// Host: where the horde is — `[id, x·10, z·10, yaw·100, health‰]` — and the run's clock.
    Snap { t: f32, e: Vec<[i64; 5]> },
    /// Host: these enemies died, each with who landed the killing blow.
    Die { d: Vec<(u64, u32)> },
    /// Host: a pickup dropped.
    Gem { id: u64, p: [f32; 3], xp: u32, heal: f32 },
    /// Someone took a pickup; its experience is everyone's.
    Took { id: u64, xp: u32 },
    /// Guest → host: my spells' blows on the horde.
    Hits { h: Vec<NetHit> },
    /// Host → one guest: this enemy struck your hero.
    Hurt { by: u64, raw: f32 },
    /// Host: a centre-screen announcement.
    Banner { text: String, secs: f32 },
    /// What my hero's spells looked like since the last word.
    Fx { f: Vec<NetFx> },
}

/// One visual of a friend's hero, played on their hero here. Targets are shared enemies by id,
/// 0 being the caster itself.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "k")]
pub(super) enum NetFx {
    Cast {
        s: u32,
        on: Vec<u64>,
        at: Option<[f32; 3]>,
    },
    Impact {
        s: u32,
        on: u64,
    },
    Swing {
        on: u64,
    },
    Area {
        s: u32,
        r: f32,
        p: [f32; 3],
        secs: f32,
    },
    Totem {
        d: u32,
        p: [f32; 3],
        secs: f32,
    },
}

/// Most visuals one tab sends a frame: past it, the rest only cost bandwidth.
const MAX_FX: usize = 48;

/// This tab's hero's visuals waiting to go out ([`super::units::Fx`] fills it).
#[derive(Resource, Default)]
pub(super) struct FxLog {
    pub muted: bool,
    pub on: bool,
    list: Vec<NetFx>,
}

impl FxLog {
    pub fn push(&mut self, fx: NetFx) {
        if self.on && !self.muted && self.list.len() < MAX_FX {
            self.list.push(fx);
        }
    }
}

/// A friend's ground area or totem, gone after its time.
#[derive(Component)]
pub(super) struct Expire(f32);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct NetSpawn {
    id: u64,
    boss: bool,
    /// Index into the battleground's roster (its enemies, or its bosses for a boss).
    i: usize,
    p: [f32; 3],
    hp: f32,
    max: f32,
    dmg: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct NetHit {
    id: u64,
    dmg: f32,
    dot: f32,
    dot_dur: f32,
    /// [`Effect`] as `[kind, a, b]`.
    fx: [f32; 3],
    mark: u32,
    mark_dur: f32,
    splash: f32,
    crit: bool,
    color: u32,
}

fn effect_out(e: Effect) -> [f32; 3] {
    match e {
        Effect::None => [0.0, 0.0, 0.0],
        Effect::Slow(a, b) => [1.0, a, b],
        Effect::Root(a) => [2.0, a, 0.0],
        Effect::Stun(a) => [3.0, a, 0.0],
        Effect::Fear(a) => [4.0, a, 0.0],
        Effect::Weaken(a, b) => [5.0, a, b],
        Effect::Drain(_) => [0.0, 0.0, 0.0],
        Effect::Execute(a) => [7.0, a, 0.0],
    }
}

fn effect_in(v: [f32; 3]) -> Effect {
    match v[0] as u32 {
        1 => Effect::Slow(v[1], v[2]),
        2 => Effect::Root(v[1]),
        3 => Effect::Stun(v[1]),
        4 => Effect::Fear(v[1]),
        5 => Effect::Weaken(v[1], v[2]),
        7 => Effect::Execute(v[1]),
        _ => Effect::None,
    }
}

/// The relay's word on an enemy or pickup: the id the host gave it.
#[derive(Component, Clone, Copy)]
pub(super) struct NetId(pub u64);

/// Where the host last put a mirrored enemy.
#[derive(Component)]
pub(super) struct NetTarget {
    pos: Vec3,
    yaw: f32,
}

/// A friend's hero, moved by its owner's messages.
#[derive(Component)]
pub(super) struct Mate(pub u32);

/// A friend in the room.
pub(super) struct Peer {
    pub name: String,
    pub hello: Option<(usize, u32, u8, u8)>,
    pub entity: Option<Entity>,
    pub pos: Option<Vec3>,
    yaw: f32,
    spd: f32,
    fl: u32,
    vy: f32,
    pub hp: f32,
    pub max: f32,
    pub level: u32,
    pub dead: bool,
    pub busy: bool,
}

impl Peer {
    fn new() -> Self {
        Self {
            name: String::new(),
            hello: None,
            entity: None,
            pos: None,
            yaw: 0.0,
            spd: 0.0,
            fl: 0,
            vy: 0.0,
            hp: 1.0,
            max: 1.0,
            level: 1,
            dead: false,
            busy: false,
        }
    }

    pub fn standing(&self) -> bool {
        self.pos.is_some() && !self.dead
    }
}

/// The run's side of co-op, readable by every system.
#[derive(Resource, Default)]
pub(super) struct Coop {
    /// A relay line is up (or coming up) for this run.
    pub on: bool,
    /// Someone else hosts: the horde here is a mirror.
    pub guest: bool,
    pub me: u32,
    pub peers: HashMap<u32, Peer>,
    /// The host's id → the entity mirroring it here (enemies and pickups).
    pub net: HashMap<u64, Entity>,
    /// Pickups already counted here.
    pub took: HashSet<u64>,
    /// Shared experience banked for the next gem pass.
    pub shared_xp: u32,
    /// Kills credited to this hero by the host.
    pub credited: u32,
    /// Blows a guest's enemies landed on this hero, for the next vitals pass.
    pub hurt: Vec<(u64, f32)>,
    outbox: Vec<(Option<u32>, Msg)>,
    spawns: Vec<NetSpawn>,
    deaths: Vec<(u64, u32)>,
    hits: Vec<NetHit>,
    last_banner: String,
    /// Friends' visuals waiting to play.
    remote_fx: Vec<(u32, NetFx)>,
}

impl Coop {
    pub fn send(&mut self, msg: Msg) {
        self.outbox.push((None, msg));
    }

    pub fn send_to(&mut self, to: u32, msg: Msg) {
        self.outbox.push((Some(to), msg));
    }

    /// The host of a co-op run, or a lone run: this tab spawns and moves the horde.
    pub fn hosts(&self) -> bool {
        !self.guest
    }

    /// A guest's blow on a mirrored enemy, for the host to land.
    pub fn forward_hit(&mut self, id: u64, h: &Hit) {
        self.hits.push(NetHit {
            id,
            dmg: h.damage,
            dot: h.dot,
            dot_dur: h.dot_dur,
            fx: effect_out(h.effect),
            mark: h.mark,
            mark_dur: h.mark_dur,
            splash: h.splash,
            crit: h.crit,
            color: h.color,
        });
    }

    /// Host: an enemy died to `by` (0 for this tab's own hero).
    pub fn died(&mut self, id: u64, by: u32) {
        self.net.remove(&id);
        let by = if by == 0 { self.me } else { by };
        self.deaths.push((id, by));
    }

    /// Anyone in the room still on their feet, this hero aside.
    pub fn ally_standing(&self) -> bool {
        self.peers.values().any(Peer::standing)
    }

    /// A card or the pause menu is up on a friend's screen.
    pub fn held_by(&self) -> Option<&str> {
        self.peers
            .values()
            .find(|p| p.busy && p.pos.is_some())
            .map(|p| p.name.as_str())
    }

    /// Every hero the horde may chase: `(friend, where, down)`; `None` is this tab's own.
    pub fn mate_targets(&self) -> impl Iterator<Item = (u32, Vec3, Option<Entity>)> + '_ {
        self.peers
            .iter()
            .filter(|(_, p)| p.standing())
            .filter_map(|(&id, p)| p.pos.map(|pos| (id, pos, p.entity)))
    }
}

/// The relay line itself: browser sockets are not `Send`, so it lives off the shared resources.
pub(super) struct CoopNet {
    link: link::Link,
    pos_t: f32,
    snap_t: f32,
    vit_t: f32,
    said_hello: bool,
}

pub(super) fn plugin(app: &mut App) {
    app.init_resource::<Coop>()
        .init_resource::<FxLog>()
        .add_systems(OnEnter(Phase::Playing), open)
        .add_systems(OnEnter(Phase::Menu), close)
        .add_systems(
            Update,
            (receive, follow_mates, follow_horde, hold_for_friends)
                .chain()
                .before(benilla_world::schedule::WorldStage::Input),
        )
        .add_systems(
            Update,
            (announce, say)
                .chain()
                .after(super::hero::hero_vitals),
        )
        .add_systems(Update, plates::mate_plates.after(follow_mates))
        .add_systems(
            Update,
            (replay_fx, expire)
                .after(follow_mates)
                .before(benilla_world::schedule::WorldStage::Input),
        );
}

/// The relay's address for this battleground, if co-op is on here.
fn relay_url(room: &str) -> Option<String> {
    if crate::webenv::var("WOW_SOLO").is_some() {
        return None;
    }
    #[cfg(target_arch = "wasm32")]
    {
        let loc = web_sys::window()?.location();
        let scheme = if loc.protocol().ok()? == "https:" {
            "wss"
        } else {
            "ws"
        };
        Some(format!("{scheme}://{}/coop/{room}", loc.host().ok()?))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let base = crate::webenv::var("WOW_COOP")?;
        Some(format!("{}/coop/{room}", base.trim_end_matches('/')))
    }
}

/// Entering a run from the menu dials the battleground's room.
fn open(world: &mut World) {
    if world.resource::<Coop>().on {
        return;
    }
    let stage = world.resource::<Selection>().stage;
    let Some(url) = relay_url(&format!("bg{stage}")) else {
        return;
    };
    info!("survivors co-op: dialing {url}");
    world.insert_non_send_resource(CoopNet {
        link: link::Link::open(&url),
        pos_t: 0.0,
        snap_t: 0.0,
        vit_t: 0.0,
        said_hello: false,
    });
    let mut coop = world.resource_mut::<Coop>();
    *coop = Coop {
        on: true,
        // Until the relay says who hosts, nothing spawns here.
        guest: true,
        ..Coop::default()
    };
}

/// Back at the menu the line goes, and the run is alone again.
fn close(world: &mut World) {
    world.remove_non_send_resource::<CoopNet>();
    *world.resource_mut::<Coop>() = Coop::default();
}

fn my_name(selection: &Selection) -> String {
    crate::webenv::var("WOW_COOP_NAME")
        .filter(|n| !n.trim().is_empty())
        .map(|n| n.trim().chars().take(16).collect())
        .unwrap_or_else(|| {
            selection
                .class_def()
                .map_or("Hero", |c| c.name)
                .to_string()
        })
}

fn hello(selection: &Selection) -> Option<Msg> {
    let class = selection.class?;
    let &(display, race, sex) = selection.look.and_then(|i| selection.looks.get(i))?;
    Some(Msg::Hello {
        name: my_name(selection),
        class,
        display,
        race,
        sex,
    })
}

/// The def a roster index names on this battleground.
fn def_for(selection: &Selection, boss: bool, i: usize) -> Option<&'static EnemyDef> {
    let roster = data::roster(selection.stage);
    let list = if boss { &roster.bosses } else { &roster.enemies };
    list.get(i)
}

fn index_of(selection: &Selection, def: &'static EnemyDef, boss: bool) -> Option<usize> {
    let roster = data::roster(selection.stage);
    let list = if boss { &roster.bosses } else { &roster.enemies };
    list.iter().position(|d| std::ptr::eq(d, def))
}

#[derive(Deserialize)]
struct Sys {
    sys: String,
    #[serde(default)]
    id: u32,
    #[serde(default)]
    host: Option<u32>,
    #[serde(default)]
    peers: Vec<u32>,
}

#[derive(Deserialize)]
struct From {
    from: u32,
    msg: Msg,
}

/// Everything the relay brought since the last frame.
#[allow(clippy::too_many_arguments)]
fn receive(
    mut commands: Commands,
    net: Option<NonSend<CoopNet>>,
    mut coop: ResMut<Coop>,
    mut run: ResMut<Run>,
    selection: Res<Selection>,
    phase: Res<State<Phase>>,
    world: WorldPoint,
    me: Query<(Entity, &Transform), With<Hero>>,
    mut enemies: Query<(Entity, &mut Enemy, &NetId, &mut ObjectStore, &Transform), Without<Hero>>,
    mut hits: MessageWriter<Hit>,
) {
    let Some(net) = net else { return };
    let (frames, dropped) = net.link.drain();
    if dropped && coop.guest && coop.me == 0 {
        // The relay never answered: the run goes on alone.
        warn!("survivors co-op: no relay, playing alone");
        coop.on = false;
        coop.guest = false;
    }
    let hero = me.single().ok();
    for raw in frames {
        if let Ok(sys) = serde_json::from_str::<Sys>(&raw) {
            match sys.sys.as_str() {
                "welcome" => {
                    coop.me = sys.id;
                    coop.guest = sys.host.is_some_and(|h| h != sys.id);
                    for p in sys.peers {
                        coop.peers.entry(p).or_insert_with(Peer::new);
                    }
                    info!(
                        "survivors co-op: in as {} ({})",
                        sys.id,
                        if coop.guest { "guest" } else { "host" }
                    );
                }
                "joined" => {
                    coop.peers.insert(sys.id, Peer::new());
                    if let Some(h) = hello(&selection) {
                        coop.send_to(sys.id, h);
                    }
                    if coop.hosts() {
                        sync_newcomer(&mut coop, sys.id, &selection, &enemies);
                    }
                }
                "left" => {
                    if let Some(p) = coop.peers.remove(&sys.id) {
                        if let Some(e) = p.entity {
                            commands.entity(e).despawn();
                        }
                    }
                    // The host left: the next in line takes the horde over where it stands.
                    if sys.host == Some(coop.me) && coop.guest {
                        info!("survivors co-op: the host left, hosting now");
                        coop.guest = false;
                        run.banner = Some(("You lead the run now.".into(), 3.0));
                    }
                }
                "full" => {
                    coop.on = false;
                    coop.guest = false;
                    run.banner = Some(("That battleground is full: playing alone.".into(), 4.0));
                }
                _ => {}
            }
            continue;
        }
        let Ok(From { from, msg }) = serde_json::from_str::<From>(&raw) else {
            continue;
        };
        match msg {
            Msg::Hello {
                name,
                class,
                display,
                race,
                sex,
            } => {
                let peer = coop.peers.entry(from).or_insert_with(Peer::new);
                let fresh = peer.hello.is_none();
                peer.name = name.clone();
                peer.hello = Some((class, display, race, sex));
                if fresh && *phase.get() != Phase::Menu {
                    run.banner = Some((format!("{name} joins the fight!"), 3.0));
                }
            }
            Msg::Pos { p, yaw, spd, fl, vy } => {
                let peer = coop.peers.entry(from).or_insert_with(Peer::new);
                peer.pos = Some(Vec3::from(p));
                peer.yaw = yaw;
                peer.spd = spd;
                peer.fl = fl;
                peer.vy = vy;
            }
            Msg::Vit {
                hp,
                max,
                lvl,
                dead,
                busy,
            } => {
                let peer = coop.peers.entry(from).or_insert_with(Peer::new);
                peer.hp = hp;
                peer.max = max;
                peer.level = lvl;
                peer.dead = dead;
                peer.busy = busy;
            }
            Msg::Spawn { e } if coop.guest => {
                for s in e {
                    if coop.net.contains_key(&s.id) {
                        continue;
                    }
                    let Some(def) = def_for(&selection, s.boss, s.i) else {
                        continue;
                    };
                    let pos = Vec3::from(s.p);
                    let face = hero.map_or(pos, |(_, t)| t.translation);
                    let ent = super::enemies::spawn_mirror(
                        &mut commands,
                        &mut run,
                        def,
                        pos,
                        face,
                        s.boss,
                        (s.hp, s.max, s.dmg),
                    );
                    commands.entity(ent).insert((
                        NetId(s.id),
                        NetTarget {
                            pos,
                            yaw: yaw_toward(face - pos),
                        },
                    ));
                    coop.net.insert(s.id, ent);
                }
            }
            Msg::Snap { t, e } if coop.guest => {
                if (t - run.elapsed).abs() > 0.75 {
                    run.elapsed = t;
                }
                for [id, x, z, yaw, hp] in e {
                    let Some(&ent) = coop.net.get(&(id as u64)) else {
                        continue;
                    };
                    let Ok((_, mut en, ..)) = enemies.get_mut(ent) else {
                        continue;
                    };
                    en.hp = en.max_hp * hp as f32 / 1000.0;
                    let mut pos = Vec3::new(x as f32 / 10.0, 0.0, z as f32 / 10.0);
                    pos.y = world.terrain_height_under(pos).unwrap_or(0.0);
                    commands.entity(ent).insert(NetTarget {
                        pos,
                        yaw: yaw as f32 / 100.0,
                    });
                }
            }
            Msg::Die { d } if coop.guest => {
                for (id, by) in d {
                    let Some(ent) = coop.net.remove(&id) else {
                        continue;
                    };
                    if by == coop.me {
                        coop.credited += 1;
                    }
                    if let Ok((e, mut en, _, mut store, _)) = enemies.get_mut(ent) {
                        en.dead = true;
                        en.hp = 0.0;
                        units::kill_unit(&mut store);
                        super::enemies::mark_dying(&mut commands, e);
                        if en.boss {
                            run.banner = Some((format!("{} is slain!", en.def.name), 3.0));
                        }
                    }
                }
            }
            Msg::Gem { id, p, xp, heal } if coop.guest => {
                if !coop.took.contains(&id) && !coop.net.contains_key(&id) {
                    let ent = super::enemies::spawn_mirror_gem(
                        &mut commands,
                        &mut run,
                        Vec3::from(p),
                        xp,
                        heal,
                    );
                    commands.entity(ent).insert(NetId(id));
                    coop.net.insert(id, ent);
                }
            }
            Msg::Took { id, xp } => {
                if coop.took.insert(id) {
                    coop.shared_xp += xp;
                }
                if let Some(ent) = coop.net.remove(&id) {
                    commands.entity(ent).despawn();
                }
            }
            Msg::Hits { h } if coop.hosts() => {
                for nh in h {
                    let Some(target) = find_enemy(&coop, &enemies, nh.id) else {
                        continue;
                    };
                    hits.write(Hit {
                        target,
                        damage: nh.dmg,
                        dot: nh.dot,
                        dot_dur: nh.dot_dur,
                        effect: effect_in(nh.fx),
                        mark: nh.mark,
                        mark_dur: nh.mark_dur,
                        splash: nh.splash,
                        crit: nh.crit,
                        color: nh.color,
                        from,
                    });
                }
            }
            Msg::Hurt { by, raw } => coop.hurt.push((by, raw)),
            Msg::Banner { text, secs } if coop.guest => {
                run.banner = Some((text, secs));
            }
            Msg::Fx { f } => {
                let room = 4 * MAX_FX;
                if coop.remote_fx.len() < room {
                    coop.remote_fx.extend(f.into_iter().map(|fx| (from, fx)));
                }
            }
            _ => {}
        }
    }
}

/// An enemy by the host's id, while it stands.
fn find_enemy(
    coop: &Coop,
    enemies: &Query<(Entity, &mut Enemy, &NetId, &mut ObjectStore, &Transform), Without<Hero>>,
    id: u64,
) -> Option<Entity> {
    let &e = coop.net.get(&id)?;
    enemies
        .get(e)
        .ok()
        .filter(|(_, en, ..)| !en.dead)
        .map(|(e, ..)| e)
}

/// Host: a newcomer gets the horde as it stands (pickups already down stay the host's).
fn sync_newcomer(
    coop: &mut Coop,
    to: u32,
    selection: &Selection,
    enemies: &Query<(Entity, &mut Enemy, &NetId, &mut ObjectStore, &Transform), Without<Hero>>,
) {
    let e: Vec<NetSpawn> = enemies
        .iter()
        .filter(|(_, en, ..)| !en.dead)
        .filter_map(|(_, en, n, _, tf)| {
            Some(NetSpawn {
                id: n.0,
                boss: en.boss,
                i: index_of(selection, en.def, en.boss)?,
                p: tf.translation.into(),
                hp: en.hp,
                max: en.max_hp,
                dmg: en.damage,
            })
        })
        .collect();
    if !e.is_empty() {
        coop.send_to(to, Msg::Spawn { e });
    }
}

/// Friends' heroes: spawned on their first word, eased toward where they say they are.
fn follow_mates(
    mut commands: Commands,
    time: Res<Time<Real>>,
    mut coop: ResMut<Coop>,
    mut run: ResMut<Run>,
    phase: Res<State<Phase>>,
    mut mates: Query<(&mut Transform, &mut MovementState, &mut ObjectStore), With<Mate>>,
) {
    if !coop.on || *phase.get() == Phase::Menu {
        return;
    }
    let dt = time.delta_secs();
    let mut spawn = Vec::new();
    for (&id, p) in coop.peers.iter_mut() {
        let (Some((class, display, race, sex)), Some(pos)) = (p.hello, p.pos) else {
            continue;
        };
        match p.entity.and_then(|e| mates.get_mut(e).ok()) {
            Some((mut tf, mut mv, mut store)) => {
                let k = (dt * 12.0).min(1.0);
                tf.translation = tf.translation.lerp(pos, k);
                let target = Quat::from_rotation_y(p.yaw);
                tf.rotation = tf.rotation.slerp(target, k);
                mv.speed = p.spd;
                mv.flags = p.fl;
                mv.vertical_speed = p.vy;
                if p.dead {
                    units::kill_unit(&mut store);
                } else {
                    units::revive_unit(&mut store);
                }
            }
            None => spawn.push((id, class, display, race, sex, pos)),
        }
    }
    for (id, class, display, race, sex, pos) in spawn {
        let Some(def) = data::CLASSES.get(class) else {
            continue;
        };
        let serial = run.guid();
        let e = hero::spawn_hero(&mut commands, def, display, race, sex, serial, pos);
        commands.entity(e).remove::<Hero>().insert(Mate(id));
        if let Some(p) = coop.peers.get_mut(&id) {
            p.entity = Some(e);
        }
    }
}

/// A guest's mirrored horde eases toward the host's last word on it.
fn follow_horde(
    time: Res<Time<Real>>,
    coop: Res<Coop>,
    mut horde: Query<(&mut Transform, &mut MovementState, &NetTarget, &Enemy)>,
) {
    if !coop.guest {
        return;
    }
    let dt = time.delta_secs();
    for (mut tf, mut mv, to, en) in &mut horde {
        if en.dead {
            if mv.speed != 0.0 || mv.flags != 0 {
                mv.speed = 0.0;
                mv.flags = 0;
            }
            continue;
        }
        let gap = flat(to.pos - tf.translation).length();
        if gap > 12.0 {
            tf.translation = to.pos;
        } else {
            tf.translation = tf.translation.lerp(to.pos, (dt * 10.0).min(1.0));
        }
        tf.rotation = tf
            .rotation
            .slerp(Quat::from_rotation_y(to.yaw), (dt * 10.0).min(1.0));
        if gap > 0.05 {
            mv.speed = en.speed;
            mv.flags = move_flags::FORWARD;
        } else if mv.speed != 0.0 || mv.flags != 0 {
            mv.speed = 0.0;
            mv.flags = 0;
        }
    }
}

/// A friend's card (or pause menu) holds the run here too.
fn hold_for_friends(
    coop: Res<Coop>,
    phase: Res<State<Phase>>,
    mut time: ResMut<Time<Virtual>>,
    mut run: ResMut<Run>,
) {
    if !coop.on || *phase.get() != Phase::Playing {
        return;
    }
    match coop.held_by() {
        Some(name) => {
            if !time.is_paused() {
                time.pause();
                run.banner = Some((format!("{name} is choosing..."), 1.0e6));
            }
        }
        None => {
            if time.is_paused() {
                time.unpause();
                run.banner = None;
            }
        }
    }
}

/// Host: give every new enemy and pickup an id and tell the room; tell it the dead.
#[allow(clippy::type_complexity)]
fn announce(
    mut commands: Commands,
    mut coop: ResMut<Coop>,
    selection: Res<Selection>,
    run: Res<Run>,
    new_enemies: Query<(Entity, &Guid, &Transform, &Enemy), (Added<Enemy>, Without<NetId>)>,
    new_gems: Query<(Entity, &Guid, &Transform, &Gem), (Added<Gem>, Without<NetId>)>,
) {
    if !coop.on || coop.guest {
        return;
    }
    for (e, guid, tf, en) in &new_enemies {
        let id = guid.0 & 0xFFFF_FFFF_FFFF;
        commands.entity(e).insert(NetId(id));
        coop.net.insert(id, e);
        if let Some(i) = index_of(&selection, en.def, en.boss) {
            coop.spawns.push(NetSpawn {
                id,
                boss: en.boss,
                i,
                p: tf.translation.into(),
                hp: en.hp,
                max: en.max_hp,
                dmg: en.damage,
            });
        }
    }
    for (e, guid, tf, gem) in &new_gems {
        let id = guid.0 & 0xFFFF_FFFF_FFFF;
        commands.entity(e).insert(NetId(id));
        coop.net.insert(id, e);
        let (xp, heal) = gem.amounts();
        coop.send(Msg::Gem {
            id,
            p: tf.translation.into(),
            xp,
            heal,
        });
    }
    if let Some((text, secs)) = run.banner.as_ref() {
        if *text != coop.last_banner && *secs < 1000.0 {
            coop.last_banner = text.clone();
            let msg = Msg::Banner {
                text: text.clone(),
                secs: *secs,
            };
            coop.send(msg);
        }
    }
}

/// Everything this tab has to say, on its clock: its hero, its vitals, the host's horde, and the
/// queued messages.
#[allow(clippy::too_many_arguments)]
fn say(
    net: Option<NonSendMut<CoopNet>>,
    time: Res<Time<Real>>,
    mut coop: ResMut<Coop>,
    run: Res<Run>,
    selection: Res<Selection>,
    phase: Res<State<Phase>>,
    me: Query<(&Transform, &MovementState), With<Hero>>,
    horde: Query<(&Transform, &Enemy, &NetId)>,
    mut log: ResMut<FxLog>,
) {
    let Some(mut net) = net else {
        log.on = false;
        log.list.clear();
        return;
    };
    log.on = coop.on;
    if !net.link.is_open() || coop.me == 0 {
        return;
    }
    if !log.list.is_empty() {
        let f = std::mem::take(&mut log.list);
        if !coop.peers.is_empty() {
            coop.send(Msg::Fx { f });
        }
    }
    let dt = time.delta_secs();
    if !net.said_hello {
        if let Some(h) = hello(&selection) {
            coop.send(h);
            net.said_hello = true;
        }
    }
    net.pos_t -= dt;
    net.snap_t -= dt;
    net.vit_t -= dt;
    if net.pos_t <= 0.0 {
        net.pos_t = POS_EVERY;
        if let Ok((tf, mv)) = me.single() {
            let (yaw, ..) = tf.rotation.to_euler(EulerRot::YXZ);
            coop.send(Msg::Pos {
                p: tf.translation.into(),
                yaw,
                spd: mv.speed,
                fl: mv.flags,
                vy: mv.vertical_speed,
            });
        }
    }
    if net.vit_t <= 0.0 {
        net.vit_t = VITALS_EVERY;
        let p = *phase.get();
        coop.send(Msg::Vit {
            hp: run.hp,
            max: run.stats().max_hp,
            lvl: run.level,
            dead: run.dead_t.is_some() || p == Phase::GameOver,
            busy: matches!(p, Phase::LevelUp | Phase::Paused),
        });
    }
    if coop.hosts() {
        if !coop.spawns.is_empty() {
            let e = std::mem::take(&mut coop.spawns);
            coop.send(Msg::Spawn { e });
        }
        if !coop.deaths.is_empty() {
            let d = std::mem::take(&mut coop.deaths);
            coop.send(Msg::Die { d });
        }
        if net.snap_t <= 0.0 && !coop.peers.is_empty() {
            net.snap_t = SNAP_EVERY;
            let e: Vec<[i64; 5]> = horde
                .iter()
                .filter(|(_, en, _)| !en.dead)
                .map(|(tf, en, id)| {
                    let (yaw, ..) = tf.rotation.to_euler(EulerRot::YXZ);
                    [
                        id.0 as i64,
                        (tf.translation.x * 10.0).round() as i64,
                        (tf.translation.z * 10.0).round() as i64,
                        (yaw * 100.0).round() as i64,
                        (en.hp / en.max_hp.max(1.0) * 1000.0).round() as i64,
                    ]
                })
                .collect();
            coop.send(Msg::Snap {
                t: run.elapsed,
                e,
            });
        }
    } else if !coop.hits.is_empty() {
        let h = std::mem::take(&mut coop.hits);
        coop.send(Msg::Hits { h });
    }
    for (to, msg) in std::mem::take(&mut coop.outbox) {
        let frame = match to {
            Some(to) => serde_json::json!({ "to": to, "msg": msg }),
            None => serde_json::json!({ "msg": msg }),
        };
        net.link.send(frame.to_string());
    }
}

/// Guest: the host's enemies' blows on this hero, landed as a local enemy's would be.
pub(super) fn take_blows(
    coop: &mut Coop,
    run: &mut Run,
    hero_e: Entity,
    fx: &mut Fx,
    hits: &mut MessageWriter<Hit>,
) {
    for (by, raw) in std::mem::take(&mut coop.hurt) {
        let attacker = coop.net.get(&by).copied();
        let taken = hero::hurt_hero(run, raw);
        if let Some(a) = attacker {
            fx.swing(a, hero_e, taken);
            if run.thorns_t > 0.0 {
                hits.write(Hit::direct(a, run.thorns * run.stats().might, false));
            }
        }
        if taken > 0.0 {
            fx.number(hero_e, taken, false, COLOR_HURT);
        }
    }
}

/// Where a fallen hero rises: beside the first friend still standing.
pub(super) fn rise_beside(coop: &Coop) -> Option<Vec3> {
    coop.peers
        .values()
        .find(|p| p.standing())
        .and_then(|p| p.pos)
        .map(|p| p + Vec3::X * 1.5)
}


/// Friends' spells, played on their heroes here: the cast on the caster, missiles and impacts on
/// the shared enemies, ground areas and totems where they were set down.
fn replay_fx(
    mut commands: Commands,
    mut coop: ResMut<Coop>,
    mut run: ResMut<Run>,
    mut fx: Fx,
) {
    if coop.remote_fx.is_empty() {
        return;
    }
    fx.mute(true);
    for (from, f) in std::mem::take(&mut coop.remote_fx) {
        let Some(caster) = coop.peers.get(&from).and_then(|p| p.entity) else {
            continue;
        };
        let target = |id: u64| {
            if id == 0 {
                Some(caster)
            } else {
                coop.net.get(&id).copied()
            }
        };
        match f {
            NetFx::Cast { s, on, at } => {
                let hits: Vec<Entity> = on.into_iter().filter_map(target).collect();
                fx.cast(caster, s, &hits, at.map(Vec3::from));
            }
            NetFx::Impact { s, on } => {
                if let Some(t) = target(on) {
                    fx.impact(t, s);
                }
            }
            NetFx::Swing { on } => {
                if let Some(t) = target(on) {
                    fx.swing(caster, t, 0.0);
                }
            }
            NetFx::Area { s, r, p, secs } => {
                let serial = run.guid();
                let e = units::spawn_area_object(&mut commands, serial, s, r, Vec3::from(p));
                commands.entity(e).insert(Expire(secs));
            }
            NetFx::Totem { d, p, secs } => {
                let serial = run.guid();
                let e = units::spawn_unit(
                    &mut commands,
                    &units::UnitSpec {
                        serial,
                        display: d,
                        scale: 1.0,
                        level: 60,
                        faction: units::FACTION_FRIENDLY,
                        bytes0: 0,
                        weapons: &[],
                        sheath: 0,
                    },
                    Vec3::from(p),
                    0.0,
                );
                commands.entity(e).insert(Expire(secs));
            }
        }
    }
    fx.mute(false);
}

/// A friend's area or totem runs its time and goes.
fn expire(mut commands: Commands, time: Res<Time>, mut left: Query<(Entity, &mut Expire)>) {
    for (e, mut t) in &mut left {
        t.0 -= time.delta_secs();
        if t.0 <= 0.0 {
            commands.entity(e).despawn();
        }
    }
}
