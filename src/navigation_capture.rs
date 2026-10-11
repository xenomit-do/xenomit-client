//! Native no-access geometry, star graph, and manager facts. Cached at 4 Hz;
//! readers reuse the serialized payload between revisions.
use super::CapturedFrame;
use ruffle_core::{
    context::UpdateContext,
    introspect::{self, Obj, Val},
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use xenomit_game_api::{
    Bounds, GalaxyGateInfo, GroupMemberInfo, HeroState, ItemState, NavigationState, Obstacle,
    ObstacleShape, Point, PortalInfo, StarSystem,
};

#[derive(Default)]
pub(super) struct NavigationCapture {
    state: NavigationState,
    bytes: Vec<u8>,
    updated: Option<Instant>,
    revision: u64,
    shields: BTreeMap<i32, f64>,
}
impl NavigationCapture {
    pub fn capture<'gc>(
        &mut self,
        context: &mut UpdateContext<'gc>,
        map: Option<Obj<'gc>>,
        frame: &mut CapturedFrame,
    ) {
        let map_id = (frame.header.map_id >= 0).then_some(frame.header.map_id);
        if self.state.map_id != map_id
            || self
                .updated
                .is_none_or(|at| at.elapsed() >= Duration::from_millis(250))
        {
            if self.state.map_id != map_id {
                self.shields.clear();
            }
            self.updated = Some(Instant::now());
            let mut state = NavigationState {
                map_id,
                star_system: self.state.star_system.clone(),
                ..Default::default()
            };
            if state.star_system.maps.is_empty() {
                let maps = xml(
                    context,
                    "net.bigpoint.darkorbit.map.model.Map",
                    "_staticConfig",
                );
                let space = xml(
                    context,
                    "net.bigpoint.darkorbit.mvc.gui.windows.spacemap.model.SpaceMapConfig",
                    "_spaceMapConfig",
                );
                if let (Some(maps), Some(space)) = (maps, space) {
                    if let Ok(catalog) = StarSystem::from_game_xml(&maps, &space) {
                        state.star_system = catalog;
                    }
                }
            }
            let hero = introspect::find_class(context, "net.bigpoint.darkorbit.Hero");
            if let Some(hero) = hero {
                state.hero = HeroState {
                    owns_pet: boolean(hero, "ownsPet").unwrap_or(false),
                    pet_destroyed: boolean(hero, "isPetDestroyed").unwrap_or(false),
                    killed: boolean(hero, "isKilled").unwrap_or(false),
                    level: number(hero, "level").map(|n| n.max(0.0) as u32),
                    configuration: introspect::find_class(
                        context,
                        "net.bigpoint.darkorbit.settings.Settings",
                    )
                    .and_then(|settings| number(settings, "selectedConfiguration"))
                    .map(|n| n as u8),
                    faction: number(hero, "factionID")
                        .or_else(|| bindable(hero, "factionID"))
                        .map(|n| n as i32)
                        .filter(|id| *id > 0),
                    ..Default::default()
                };
                capture_items(context, &mut state.hero);
                capture_pet(context, &mut state.hero);
                capture_galaxy(context, &mut state);
                capture_group(context, &mut state);
                if state.hero.killed {
                    state.hero.free_repair_type = free_repair(context);
                }
            }
            if let Some(entities) = map
                .and_then(|map| object(map, "_entities"))
                .and_then(Obj::vector_values)
            {
                state.geometry_ready = true;
                let mut targets = BTreeMap::<i32, Vec<i32>>::new();
                for value in &entities {
                    let Some(entity) = value.as_object() else {
                        continue;
                    };
                    let Some(id) = number(entity, "_id").map(|v| v as i32) else {
                        continue;
                    };
                    let classes: Vec<_> = entity.class_names().collect();
                    if classes.iter().any(|c| c == "POIZone")
                        && text(entity, "_zoneType").as_deref() == Some("NOA")
                    {
                        if state.obstacles.len() >= 256 {
                            state.geometry_ready = false;
                            continue;
                        }
                        if let Some(obstacle) = capture_obstacle(context, entity, id, &classes) {
                            state.obstacles.push(obstacle);
                        } else {
                            state.geometry_ready = false;
                        }
                    }
                    if classes.iter().any(|c| c == "Portal") {
                        if let Some(position) =
                            object(entity, "_position").and_then(|p| point(context, p))
                        {
                            state.portals.push(PortalInfo {
                                id,
                                position,
                                type_id: number(entity, "_designId").unwrap_or(0.0) as i32,
                                target_map: state
                                    .star_system
                                    .links
                                    .iter()
                                    .find(|l| l.from == frame.header.map_id && l.portal_id == id)
                                    .map(|l| l.to),
                                jumping: boolean(entity, "isJumping").unwrap_or(false),
                            });
                        }
                    }
                    let mut fields = serde_json::Map::new();
                    fields.insert(
                        "isValid".into(),
                        serde_json::json!(object(entity, "_container").is_some()),
                    );
                    if classes.iter().any(|c| c == "Collectable") {
                        if let Some(collected) = boolean(entity, "_isCollected") {
                            fields.insert("isCollected".into(), serde_json::json!(collected));
                        }
                    }
                    if classes.iter().any(|c| c == "Ship") {
                        if let Some(npc_id) = object(entity, "_shipPattern")
                            .and_then(|pattern| number(pattern, "npcId"))
                            .filter(|id| *id > 0.0)
                        {
                            fields.insert("npcTypeId".into(), serde_json::json!(npc_id));
                        }
                        if let Some(speed) =
                            object(entity, "_moveTrait").and_then(|m| bindable(m, "speed"))
                        {
                            fields.insert("speed".into(), serde_json::json!(speed));
                        }
                        if let Some(invisible) =
                            object(entity, "cloaked").and_then(|v| boolean(v, "_value"))
                        {
                            fields.insert("isInvisible".into(), serde_json::json!(invisible));
                        }
                        let traits = object(entity, "_traits")
                            .and_then(Obj::vector_values)
                            .unwrap_or_default();
                        if boolean(entity, "_isNPC") == Some(true)
                            && let Some(target) = traits
                                .iter()
                                .filter_map(|v| v.as_object())
                                .find(|t| t.class_names().any(|c| c == "AttackTargetTrait"))
                            && let Some(lock_type) = bindable(target, "lockType")
                                .filter(|kind| *kind == 1.0 || *kind == 2.0)
                        {
                            fields.insert("isOwned".into(), serde_json::json!(lock_type == 1.0));
                        }
                        if let Some(attacker) = traits
                            .iter()
                            .filter_map(|v| v.as_object())
                            .find(|t| t.class_names().any(|c| c == "AttackerTrait"))
                        {
                            if id == frame.header.hero_id {
                                fields.insert("attackingTargetId".into(), serde_json::Value::Null);
                            }
                            if let Some(target) = object(attacker, "_targetOwner")
                                .and_then(|t| number(t, "_id"))
                                .map(|v| v as i32)
                            {
                                targets.entry(target).or_default().push(id);
                                fields.insert(
                                    "attackingHero".into(),
                                    serde_json::json!(target == frame.header.hero_id),
                                );
                                if id == frame.header.hero_id {
                                    fields.insert(
                                        "attackingTargetId".into(),
                                        serde_json::json!(target),
                                    );
                                }
                            }
                        }
                        if let Some(shield) =
                            object(entity, "_hpTrait").and_then(|hp| bindable(hp, "shield"))
                        {
                            let increased = self
                                .shields
                                .insert(id, shield)
                                .is_some_and(|last| shield > last);
                            fields.insert("shieldIncreased".into(), serde_json::json!(increased));
                        }
                        if let Some(info) = object(entity, "_info") {
                            let faction = bindable(info, "factionID").unwrap_or(0.0) as i32;
                            let diplomacy = bindable(info, "clanDiplomacy").unwrap_or(0.0) as i32;
                            let npc = boolean(entity, "_isNPC").unwrap_or(false);
                            let enemy = !npc
                                && id != frame.header.hero_id
                                && (diplomacy == 3
                                    || (faction > 0
                                        && frame.header.hero_faction > 0
                                        && faction != frame.header.hero_faction
                                        && diplomacy != 1
                                        && diplomacy != 2));
                            fields.insert("isEnemy".into(), serde_json::json!(enemy));
                        }
                    }
                    if id == frame.header.hero_id {
                        state.hero.pet_active = object(entity, "pet")
                            .and_then(|pet| object(pet, "_container"))
                            .is_some();
                        if let Some(hero) = hero {
                            for (key, slot) in [("cargo", "cargo"), ("maxCargo", "maxCargo")] {
                                if let Some(value) = bindable(hero, slot) {
                                    fields.insert(key.into(), serde_json::json!(value));
                                }
                            }
                            let last =
                                object(hero, "lastCollectedItem").and_then(|v| number(v, "_id"));
                            if let Some(last) = last {
                                fields
                                    .insert("lastCollectedItemId".into(), serde_json::json!(last));
                            }
                        }
                        if let Some(target) = map
                            .and_then(|map| object(map, "_lockManager"))
                            .and_then(|lock| object(lock, "_target"))
                            .and_then(|t| number(t, "_id"))
                            .map(|id| id as i32)
                        {
                            fields.insert("localTargetId".into(), serde_json::json!(target));
                            fields.insert("lockedTargetId".into(), serde_json::json!(target));
                        } else {
                            fields.insert("localTargetId".into(), serde_json::Value::Null);
                            fields.insert("lockedTargetId".into(), serde_json::Value::Null);
                        }
                    }
                    state.entity_fields.insert(id, fields);
                }
                for (target, attackers) in targets {
                    if let Some(fields) = state.entity_fields.get_mut(&target) {
                        fields.insert(
                            "attackedByOthers".into(),
                            serde_json::json!(
                                attackers.iter().any(|id| *id != frame.header.hero_id)
                            ),
                        );
                    }
                }
                if let Some(effects) = map
                    .and_then(|map| object(map, "_effects"))
                    .and_then(|manager| object(manager, "_effects"))
                    .and_then(Obj::vector_values)
                {
                    let leech = effects.iter().filter_map(|v| v.as_object()).any(|effect| {
                        number(effect, "_id") == Some(11.0)
                            && object(effect, "_source").and_then(|source| number(source, "_id"))
                                == Some(f64::from(frame.header.hero_id))
                    });
                    if let Some(fields) = state.entity_fields.get_mut(&frame.header.hero_id) {
                        fields.insert("energyLeech".into(), serde_json::json!(leech));
                    }
                }
                self.shields
                    .retain(|id, _| state.entity_fields.contains_key(id));
                if entities.len() >= xenomit_game_api::live::layout::MAX_ENTITIES {
                    state.geometry_ready = false;
                }
            }
            self.bytes = serde_json::to_vec(&state).unwrap_or_default();
            self.state = state;
            self.revision = self.revision.wrapping_add(1);
        }
        let limit = xenomit_game_api::live::layout::MAX_STRING_LEN;
        frame.header.navigation_revision = self.revision;
        frame.header.navigation_off = frame.strings.len() as u32;
        if frame.strings.len().saturating_add(self.bytes.len()) <= limit {
            frame.header.navigation_len = self.bytes.len() as u32;
            frame.strings.extend_from_slice(&self.bytes);
        }
    }
}
fn free_repair<'gc>(context: &mut UpdateContext<'gc>) -> Option<i32> {
    let class = introspect::find_class(
        context,
        "net.bigpoint.darkorbit.gui.windows.components.Killscreen",
    )?;
    let screen = class
        .call_method(context, "getInstance", &[])
        .ok()?
        .as_object()?;
    let options = object(screen, "_optionTypes")?;
    let length = property_number(context, options, "length")?.clamp(0.0, 14.0) as i32;
    let choices = object(screen, "optionVOs")?.vector_values()?;
    for index in 0..length {
        let kind = options
            .get_property(context, &index.to_string())
            .ok()?
            .as_i32()?;
        if ![1, 6].contains(&kind) {
            continue;
        }
        let choice = choices.get(kind as usize)?.as_object()?;
        if number(choice, "repairCost") == Some(0.0)
            && number(choice, "cooldown") == Some(0.0)
            && boolean(choice, "affordable") == Some(true)
        {
            return Some(kind);
        }
    }
    None
}
fn capture_pet<'gc>(context: &mut UpdateContext<'gc>, hero: &mut HeroState) {
    let Some(class) = introspect::find_class(context, "net.bigpoint.darkorbit.net.PetAssembly")
    else {
        return;
    };
    let Some(manager) = object(class, "instance") else {
        return;
    };
    let Some(window) = object(manager, "_petWindow") else {
        return;
    };
    // _currentGearId controls a locator submenu, not the server-selected gear.
    // Leave selection unknown instead of reporting that UI cursor.
    let Some(gears) = object(window, "_gears") else {
        return;
    };
    let length = property_number(context, gears, "length")
        .unwrap_or(0.0)
        .clamp(0.0, 64.0) as i32;
    for index in 0..length {
        if let Some(id) = gears
            .get_property(context, &index.to_string())
            .ok()
            .and_then(Val::as_i32)
        {
            hero.pet_gears.push(id);
        }
    }
}
fn object<'gc>(obj: Obj<'gc>, name: &str) -> Option<Obj<'gc>> {
    obj.slot_by_name(name)?.as_object()
}
fn number(obj: Obj<'_>, name: &str) -> Option<f64> {
    obj.slot_by_name(name)?.as_f64().filter(|n| n.is_finite())
}
fn boolean(obj: Obj<'_>, name: &str) -> Option<bool> {
    obj.slot_by_name(name)?.as_bool()
}
fn text(obj: Obj<'_>, name: &str) -> Option<String> {
    obj.slot_by_name(name)?.to_string_lossy()
}
fn bindable(obj: Obj<'_>, name: &str) -> Option<f64> {
    number(object(obj, name)?, "_value")
}
fn property_number<'gc>(
    context: &mut UpdateContext<'gc>,
    obj: Obj<'gc>,
    name: &str,
) -> Option<f64> {
    number(obj, name).or_else(|| {
        obj.get_property(context, name)
            .ok()?
            .as_f64()
            .filter(|n| n.is_finite())
    })
}
fn point<'gc>(context: &mut UpdateContext<'gc>, obj: Obj<'gc>) -> Option<Point> {
    Some(Point {
        x: number(obj, "_x").or_else(|| property_number(context, obj, "x"))?,
        y: number(obj, "_y").or_else(|| property_number(context, obj, "y"))?,
    })
}
fn xml<'gc>(context: &mut UpdateContext<'gc>, class: &str, slot: &str) -> Option<String> {
    let xml = introspect::find_class(context, class)?
        .slot_by_name(slot)?
        .as_object()?;
    xml.call_method(context, "toXMLString", &[])
        .ok()?
        .to_string_lossy()
}
fn capture_obstacle<'gc>(
    context: &mut UpdateContext<'gc>,
    entity: Obj<'gc>,
    id: i32,
    classes: &[String],
) -> Option<Obstacle> {
    let shape = if classes.iter().any(|c| c == "CircleZone") {
        let center = point(context, object(entity, "_position")?)?;
        let radius = number(entity, "_collisionRadius")?;
        if radius <= 0.0 {
            return None;
        }
        ObstacleShape::Circle { center, radius }
    } else if classes.iter().any(|c| c == "RectangleZone") {
        let bounds = object(entity, "_collisionBounds")?;
        let bounds = Bounds {
            x: property_number(context, bounds, "x")?,
            y: property_number(context, bounds, "y")?,
            width: property_number(context, bounds, "width")?,
            height: property_number(context, bounds, "height")?,
        };
        if bounds.width <= 0.0 || bounds.height <= 0.0 {
            return None;
        }
        ObstacleShape::Rectangle { bounds }
    } else {
        let values = object(entity, "_collisionPoints")?.vector_values()?;
        if !(3..=1024).contains(&values.len()) {
            return None;
        }
        let points = values
            .into_iter()
            .map(|v| point(context, v.as_object()?))
            .collect::<Option<Vec<_>>>()?;
        ObstacleShape::Polygon { points }
    };
    Some(Obstacle {
        id,
        name: text(entity, "_zoneId").unwrap_or_else(|| "No-access zone".into()),
        shape,
        inverted: boolean(entity, "_inverted").unwrap_or(false),
    })
}
fn capture_galaxy<'gc>(context: &mut UpdateContext<'gc>, state: &mut NavigationState) {
    let Some(model) = facade_proxy(context, "ggBuilder").and_then(|proxy| object(proxy, "data"))
    else {
        return;
    };
    let Some(gates) = object(model, "gates") else {
        return;
    };
    let values = sequence(context, gates);
    for (index, value) in values.into_iter().take(32).enumerate() {
        let Some(gate) = value.as_object() else {
            continue;
        };
        let id = (index as i32) + 1;
        state.galaxy_gates.push(GalaxyGateInfo {
            id,
            parts: slot_i32(gate, "_partsCount"),
            total_parts: slot_i32(gate, "_partsTotal"),
            lives: slot_i32(gate, "_livesLeft"),
            life_price: slot_i32(gate, "_lifePrice"),
            current_wave: slot_i32(gate, "_currentWave"),
            total_waves: slot_i32(gate, "_totalWave"),
            finished: boolean(gate, "_finishState").unwrap_or(false),
            multiplier: galaxy_multiplier(context, model, id),
        });
    }
}
fn galaxy_multiplier<'gc>(context: &mut UpdateContext<'gc>, model: Obj<'gc>, gate_id: i32) -> i32 {
    let slot = match gate_id {
        1 => "_alphaAssets",
        2 => "_betaAssets",
        3 => "_gammaAssets",
        4 => "_deltaAssets",
        5 => "_epsilonAssets",
        6 => "_zetaAssets",
        7 => "_kappaAssets",
        8 => "_lambdaAssets",
        12 => "_hadesAssets",
        19 => "_streunerAssets",
        _ => return 0,
    };
    object(model, slot)
        .and_then(|assets| {
            assets
                .call_method(context, "getMultiplierValue", &[])
                .ok()
                .and_then(|value| value.as_f64())
        })
        .unwrap_or(0.0) as i32
}
fn capture_group<'gc>(context: &mut UpdateContext<'gc>, state: &mut NavigationState) {
    let Some(group) = facade_proxy(context, "GroupProxy").and_then(|proxy| object(proxy, "data"))
    else {
        return;
    };
    let Some(members) = object(group, "_members") else {
        return;
    };
    for value in sequence(context, members).into_iter().take(16) {
        let Some(member) = value.as_object() else {
            continue;
        };
        state.group.members.push(GroupMemberInfo {
            id: slot_i32(member, "id"),
            name: text(member, "nickName").unwrap_or_default(),
            map_id: slot_i32(member, "mapID"),
            leader: boolean(member, "isLead").unwrap_or(false),
        });
    }
}
fn facade_proxy<'gc>(context: &mut UpdateContext<'gc>, name: &str) -> Option<Obj<'gc>> {
    let facade = introspect::find_class(context, "net.bigpoint.darkorbit.mvc.application.GameFacade")
        .and_then(|class| class.call_method(context, "getInstance", &[]).ok())
        .and_then(Val::as_object)?;
    facade
        .call_method(context, "retrieveProxy", &[Val::from_str(context, name)])
        .ok()
        .and_then(Val::as_object)
}
fn sequence<'gc>(context: &mut UpdateContext<'gc>, obj: Obj<'gc>) -> Vec<Val<'gc>> {
    if let Some(values) = obj.vector_values() {
        return values;
    }
    let length = property_number(context, obj, "length")
        .unwrap_or(0.0)
        .clamp(0.0, 64.0) as i32;
    (0..length)
        .filter_map(|index| obj.get_property(context, &index.to_string()).ok())
        .collect()
}
fn slot_i32(obj: Obj<'_>, name: &str) -> i32 {
    number(obj, name).unwrap_or(0.0) as i32
}
fn capture_items<'gc>(context: &mut UpdateContext<'gc>, hero: &mut HeroState) {
    let Some(facade) =
        introspect::find_class(context, "net.bigpoint.darkorbit.mvc.application.GameFacade")
            .and_then(|c| c.call_method(context, "getInstance", &[]).ok())
            .and_then(Val::as_object)
    else {
        return;
    };
    let name = Val::from_str(context, "ItemsControlMenuProxy");
    let Some(proxy) = facade
        .call_method(context, "retrieveProxy", &[name])
        .ok()
        .and_then(Val::as_object)
    else {
        return;
    };
    let Some(categories) = object(proxy, "_categoryBar")
        .and_then(|v| object(v, "_categories"))
        .and_then(Obj::vector_values)
    else {
        return;
    };
    for category in categories.into_iter().take(128).filter_map(Val::as_object) {
        let Some(items) = object(category, "_items").and_then(Obj::vector_values) else {
            continue;
        };
        for item in items.into_iter().take(256).filter_map(Val::as_object) {
            let Some(id) = text(item, "_id") else {
                continue;
            };
            hero.items.insert(
                id,
                ItemState {
                    quantity: number(item, "_counterValue"),
                    available: boolean(item, "_available").unwrap_or(false),
                    activatable: boolean(item, "_activatable").unwrap_or(false),
                    selected: boolean(item, "_selected").unwrap_or(false),
                    blocked: boolean(item, "_blocked").unwrap_or(true),
                },
            );
        }
    }
}
