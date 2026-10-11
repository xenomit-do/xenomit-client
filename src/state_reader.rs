//! Per-frame capture of the live game map straight from the AVM2 object
//! graph — the DarkBot-style path that replaces `DobotBridge.stateAS`.
//!
//! Slot indices are resolved once per class via the vtable and cached by
//! class pointer, so a frame is a handful of `find_class` results plus
//! direct slot reads. Every missing slot/object is tolerated: the entity is
//! skipped and the slot name is logged once.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use ruffle_core::context::UpdateContext;
use ruffle_core::introspect::{self, Obj, ObjHandle, Val};
use xenomit_game_api::live::layout::{self, LiveEntityRecord, LiveHeader};

const SCREEN_MANAGER: &str = "net.bigpoint.darkorbit.managers.ScreenManager";
const HERO: &str = "net.bigpoint.darkorbit.Hero";
#[path = "navigation_capture.rs"]
mod navigation_capture;

/// Re-attempt a missing class lookup no more often than this.
const CLASS_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// One fully captured frame, ready to publish to shared memory. Owned by the
/// caller so its buffers are reused between captures.
#[derive(Default)]
pub struct CapturedFrame {
    pub header: LiveHeader,
    pub entities: Vec<LiveEntityRecord>,
    /// UTF-8 string table backing `name_off`/`name_len`.
    pub strings: Vec<u8>,
}

/// Per-class traits resolved once from the superclass local names.
#[derive(Clone, Copy)]
struct ClassInfo {
    kind: u8,
    /// Chain contains `Ship`; `_isNPC`/`_hpTrait`/`_moveTrait`/`_info`
    /// are only read for these classes (Pets extend `Ship`).
    is_ship: bool,
    /// Chain contains `Collectable`; `_hash` is only read for these.
    is_collectable: bool,
}

/// Per-frame capture state: class handles plus per-class slot-index caches.
pub struct StateReader {
    navigation: navigation_capture::NavigationCapture,
    screen_manager: Option<ObjHandle>,
    hero_class: Option<ObjHandle>,
    next_class_retry: Instant,
    /// `(class_ptr, slot name) -> slot index`; `None` is cached too.
    slots: HashMap<(usize, &'static str), Option<usize>>,
    /// `class_ptr -> class info`, resolved from the superclass chain once.
    classes: HashMap<usize, ClassInfo>,
    /// `(class_ptr, slot name)` pairs already logged as missing.
    missing_logged: HashSet<(usize, &'static str)>,
    frame: u64,
    log_mediator: Option<ObjHandle>,
    log_messages: Vec<ObjHandle>,
    telemetry_at: Instant,
    log_retry_at: Instant,
    reward_templates: Vec<(usize, String, String)>,
}

impl Default for StateReader {
    fn default() -> Self {
        Self::new()
    }
}

impl StateReader {
    pub fn new() -> Self {
        Self {
            navigation: Default::default(),
            screen_manager: None,
            hero_class: None,
            next_class_retry: Instant::now(),
            slots: HashMap::new(),
            classes: HashMap::new(),
            missing_logged: HashSet::new(),
            frame: 0,
            log_mediator: None,
            log_messages: Vec::new(),
            telemetry_at: Instant::now() - std::time::Duration::from_secs(60),
            log_retry_at: Instant::now(),
            reward_templates: Vec::new(),
        }
    }

    /// Read the game model into `frame`. Never panics on absent or malformed
    /// game objects.
    pub fn capture<'gc>(&mut self, context: &mut UpdateContext<'gc>, frame: &mut CapturedFrame) {
        let started = Instant::now();
        let captured_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;

        self.frame = self.frame.wrapping_add(1);
        frame.entities.clear();
        frame.strings.clear();

        frame.header = LiveHeader {
            magic: layout::MAGIC,
            version: layout::VERSION,
            seq: 0,
            _pad: 0,
            frame: self.frame,
            captured_unix_ms,
            capture_us: 0,
            map_id: -1,
            map_width: 0.0,
            map_height: 0.0,
            hero_id: 0,
            hero_x: 0.0,
            hero_y: 0.0,
            hero_dest_x: 0.0,
            hero_dest_y: 0.0,
            hero_flags: 0,
            hero_faction: 0,
            hero_clan: 0,
            entity_count: 0,
            string_bytes: 0,
            navigation_revision: 0,
            navigation_off: 0,
            navigation_len: 0,
            _reserved: [0; 24],
        };

        let map = self.find_map(context);
        if let Some(map) = map {
            self.read_map(context, map, frame);
            self.capture_telemetry(context, frame.header.hero_id);
        }
        self.navigation.capture(context, map, frame);

        frame.header.entity_count = frame.entities.len() as u32;
        frame.header.string_bytes = frame.strings.len() as u32;
        frame.header.capture_us = started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
    }

    /// Observe the game's actual log objects, retaining their identity so
    /// identical consecutive rewards are each emitted once. The game retains
    /// 25 messages; capture at the normal frame interval to minimize loss.
    fn capture_telemetry<'gc>(&mut self, context: &mut UpdateContext<'gc>, hero_id: i32) {
        if hero_id == 0 { return; }
        if self.log_mediator.is_none() && Instant::now() >= self.log_retry_at {
            self.log_retry_at = Instant::now() + CLASS_RETRY;
            let mediator = introspect::find_class(context, "net.bigpoint.darkorbit.mvc.application.GameFacade")
                .and_then(|class| class.call_method(context, "getInstance", &[]).ok())
                .and_then(|value| value.as_object())
                .and_then(|facade| {
                    let name = Val::from_str(context, "LogWindowMediator");
                    facade.call_method(context, "retrieveMediator", &[name]).ok()
                }).and_then(|value| value.as_object());
            self.log_mediator = mediator.map(|obj| obj.stash(context));
            if self.log_mediator.is_some() { self.log_retry_at = Instant::now(); }
        }
        if let Some(handle) = &self.log_mediator {
            let mediator = handle.fetch(context);
            if self.reward_templates.len() < 4 && Instant::now() >= self.log_retry_at {
                self.log_retry_at = Instant::now() + CLASS_RETRY;
                if let Some(locale) = introspect::find_class(context, "net.bigpoint.as3toolbox.bplocale.BPLocale") {
                    for (index, key) in ["getcredits", "geturidium", "gatherep", "getexp"].iter().enumerate() {
                        if self.reward_templates.iter().any(|(known, _, _)| *known == index) { continue; }
                        let key = Val::from_str(context, key);
                        if let Some(template) = locale.call_method(context, "getItem", &[key]).ok().and_then(|v| v.to_string_lossy()) {
                            if let Some((prefix, suffix)) = template.split_once("%!") {
                                self.reward_templates.push((index, prefix.into(), suffix.into()));
                            }
                        }
                    }
                }
            }
            if let Some(buffer) = self.slot_object(context, mediator, "_messageBuffer") {
                let length = buffer.get_property(context, "length").ok().and_then(|v| v.as_i32()).unwrap_or(0).clamp(0, 25);
                let mut current = Vec::new();
                for index in 0..length {
                    let Some(message) = buffer.get_property(context, &index.to_string()).ok().and_then(|v| v.as_object()) else { continue; };
                    let seen = self.log_messages.iter().any(|old| old.fetch(context).raw() == message.raw());
                    if !seen {
                        if let Some(text) = self.slot_val(context, message, "message").to_string_lossy() {
                            let rewards = template_rewards(&text, &self.reward_templates);
                            let data = serde_json::json!({"user_id": hero_id, "rewards": rewards});
                            eprintln!("[game-log] {}", serde_json::json!({"user_id": hero_id, "message": text, "rewards": rewards}));
                            crate::client_events::push("game_log", Some(text), data);
                        }
                    }
                    current.push(message.stash(context));
                }
                self.log_messages = current;
            }
        }
        if self.telemetry_at.elapsed() >= std::time::Duration::from_secs(60) {
            if let Some(hero) = self.hero_class.as_ref().map(|h| h.fetch(context)) {
                let fields = [("credits", "creditsAmount"), ("uridium", "uridiumAmount"), ("honor", "honorPoints"), ("experience", "experiencePoints")];
                let mut assets = serde_json::Map::new();
                for (key, slot) in fields {
                    if let Some(value) = self.slot_f64(context, hero, slot).filter(|v| v.is_finite()) {
                        assets.insert(key.into(), serde_json::json!(value));
                    }
                }
                if !assets.is_empty() {
                    eprintln!("[game-assets] {}", serde_json::json!({"user_id": hero_id, "assets": assets}));
                    crate::client_events::push("game_assets", None, serde_json::json!({"user_id": hero_id, "assets": assets}));
                    self.telemetry_at = Instant::now();
                }
            }
        }
    }

    /// `ScreenManager._instance._map`, or `None` when the game is not on a
    /// map. Class handles are re-resolved at most once per second while
    /// missing so a late-loading game model still gets picked up.
    fn find_map<'gc>(&mut self, context: &mut UpdateContext<'gc>) -> Option<Obj<'gc>> {
        if (self.screen_manager.is_none() || self.hero_class.is_none())
            && Instant::now() >= self.next_class_retry
        {
            self.next_class_retry = Instant::now() + CLASS_RETRY;
            if self.screen_manager.is_none() {
                self.screen_manager = introspect::find_class(context, SCREEN_MANAGER)
                    .map(|class| class.stash(context));
            }
            if self.hero_class.is_none() {
                self.hero_class = introspect::find_class(context, HERO)
                    .map(|class| class.stash(context));
            }
        }
        let screen_manager = self.screen_manager.as_ref()?.fetch(context);
        let instance = self.slot_object(context, screen_manager, "_instance")?;
        self.slot_object(context, instance, "_map")
    }

    /// Slot index cached per class; missing names are logged once.
    fn slot_index<'gc>(
        &mut self,
        context: &UpdateContext<'gc>,
        obj: Obj<'gc>,
        name: &'static str,
    ) -> Option<usize> {
        self.slot_index_impl(context, obj, name, true)
    }

    /// Slot index for genuinely-optional slots: no missing-slot logging.
    fn silent_index<'gc>(
        &mut self,
        context: &UpdateContext<'gc>,
        obj: Obj<'gc>,
        name: &'static str,
    ) -> Option<usize> {
        self.slot_index_impl(context, obj, name, false)
    }

    fn slot_index_impl<'gc>(
        &mut self,
        context: &UpdateContext<'gc>,
        obj: Obj<'gc>,
        name: &'static str,
        log_missing: bool,
    ) -> Option<usize> {
        let class = obj.class_ptr();
        let key = (class, name);
        if let Some(&cached) = self.slots.get(&key) {
            return cached;
        }
        let index = obj.slot_index(name);
        if index.is_none() && log_missing && self.missing_logged.insert(key) {
            eprintln!(
                "[state] {} has no slot '{name}'",
                obj.qualified_name(context)
            );
        }
        self.slots.insert(key, index);
        index
    }

    fn slot_val<'gc>(
        &mut self,
        context: &UpdateContext<'gc>,
        obj: Obj<'gc>,
        name: &'static str,
    ) -> Val<'gc> {
        match self.slot_index(context, obj, name) {
            Some(index) => obj.slot(index),
            None => Val::UNDEFINED,
        }
    }

    /// `slot_val` backed by `silent_index` for optional slots.
    fn slot_val_silent<'gc>(
        &mut self,
        context: &UpdateContext<'gc>,
        obj: Obj<'gc>,
        name: &'static str,
    ) -> Val<'gc> {
        match self.silent_index(context, obj, name) {
            Some(index) => obj.slot(index),
            None => Val::UNDEFINED,
        }
    }

    fn slot_object<'gc>(
        &mut self,
        context: &UpdateContext<'gc>,
        obj: Obj<'gc>,
        name: &'static str,
    ) -> Option<Obj<'gc>> {
        self.slot_val(context, obj, name).as_object()
    }

    fn slot_f64<'gc>(
        &mut self,
        context: &UpdateContext<'gc>,
        obj: Obj<'gc>,
        name: &'static str,
    ) -> Option<f64> {
        self.slot_val(context, obj, name).as_f64()
    }

    fn slot_i32<'gc>(
        &mut self,
        context: &UpdateContext<'gc>,
        obj: Obj<'gc>,
        name: &'static str,
    ) -> Option<i32> {
        self.slot_val(context, obj, name).as_i32()
    }

    /// `obj.name` dereferenced through a `Bindable*` wrapper's `_value`.
    fn bindable_f64<'gc>(
        &mut self,
        context: &UpdateContext<'gc>,
        obj: Obj<'gc>,
        name: &'static str,
    ) -> Option<f64> {
        let bindable = self.slot_object(context, obj, name)?;
        self.slot_f64(context, bindable, "_value")
    }

    fn bindable_i32<'gc>(
        &mut self,
        context: &UpdateContext<'gc>,
        obj: Obj<'gc>,
        name: &'static str,
    ) -> Option<i32> {
        self.bindable_f64(context, obj, name).map(|v| v as i32)
    }

    fn bindable_string<'gc>(
        &mut self,
        context: &UpdateContext<'gc>,
        obj: Obj<'gc>,
        name: &'static str,
    ) -> Option<String> {
        let bindable = self.slot_object(context, obj, name)?;
        self.slot_val(context, bindable, "_value").to_string_lossy()
    }

    fn read_map<'gc>(
        &mut self,
        context: &mut UpdateContext<'gc>,
        map: Obj<'gc>,
        frame: &mut CapturedFrame,
    ) {
        frame.header.map_id = self.slot_i32(context, map, "_mapID").unwrap_or(-1);
        frame.header.map_width =
            self.slot_i32(context, map, "_serious_width").unwrap_or(21000) as f32;
        frame.header.map_height =
            self.slot_i32(context, map, "_serious_height").unwrap_or(13100) as f32;

        let hero_id = self
            .hero_class
            .as_ref()
            .map(|class| class.fetch(context))
            .and_then(|class| self.slot_i32(context, class, "userID"));

        let Some(entities) = self
            .slot_object(context, map, "_entities")
            .and_then(|vector| vector.vector_values())
        else {
            return;
        };

        for value in entities {
            if frame.entities.len() >= layout::MAX_ENTITIES {
                break;
            }
            let Some(entity) = value.as_object() else {
                continue;
            };
            self.read_entity(context, entity, hero_id, frame);
        }
    }

    /// Classify once per class pointer by superclass local names.
    fn class_of<'gc>(&mut self, entity: Obj<'gc>) -> ClassInfo {
        let class = entity.class_ptr();
        *self.classes.entry(class).or_insert_with(|| {
            let names: Vec<String> = entity.class_names().collect();
            let has = |needle: &str| names.iter().any(|name| name == needle);
            let contains = |needle: &str| names.iter().any(|name| name.contains(needle));
            let is_ship = has("Ship");
            let is_collectable = has("Collectable");
            let kind = if has("Pet") {
                layout::kind::PET
            } else if has("Box") {
                layout::kind::BOX
            } else if has("Ore") {
                layout::kind::ORE
            } else if has("Beacon") {
                layout::kind::BEACON
            } else if contains("Portal") {
                layout::kind::PORTAL
            } else if contains("BattleStation") {
                layout::kind::STATION
            } else if has("Headquarters")
                || contains("BaseStation")
                || contains("BaseHeadquarters")
                || contains("BaseSpot")
            {
                layout::kind::BASE
            } else if contains("Mine") {
                layout::kind::MINE
            } else if is_ship {
                layout::kind::PLAYER
            } else {
                layout::kind::OTHER
            };
            ClassInfo {
                kind,
                is_ship,
                is_collectable,
            }
        })
    }

    fn read_entity<'gc>(
        &mut self,
        context: &mut UpdateContext<'gc>,
        entity: Obj<'gc>,
        hero_id: Option<i32>,
        frame: &mut CapturedFrame,
    ) {
        let Some(id) = self.slot_i32(context, entity, "_id") else {
            return;
        };
        let Some(position) = self.slot_object(context, entity, "_position") else {
            return;
        };
        let (Some(x), Some(y)) = (
            self.slot_f64(context, position, "_x"),
            self.slot_f64(context, position, "_y"),
        ) else {
            return;
        };
        if !x.is_finite() || !y.is_finite() {
            return;
        }

        let class = self.class_of(entity);
        let mut kind = class.kind;
        let is_ship = class.is_ship;
        // Optional slots are read silently: absence is expected on
        // non-ship/non-ore classes.
        let is_npc = is_ship
            && self
                .slot_val_silent(context, entity, "_isNPC")
                .as_bool()
                .unwrap_or(false);
        let is_hero = hero_id == Some(id);
        if kind == layout::kind::PLAYER && is_npc {
            kind = layout::kind::NPC;
        }
        if is_hero {
            kind = layout::kind::HERO;
        }

        let mut record = LiveEntityRecord {
            id,
            kind,
            x: x as f32,
            y: y as f32,
            ..LiveEntityRecord::default()
        };
        if is_npc {
            record.flags |= layout::entity_flags::NPC;
        }
        if is_hero {
            record.flags |= layout::entity_flags::HERO;
        }

        let mut username = None;
        if is_ship {
            // Health/shield/nanohull via HPTrait's BindableInt members.
            if let Some(hp_trait) = self.slot_object(context, entity, "_hpTrait") {
                record.flags |= layout::entity_flags::HAS_HP;
                record.hp = self.bindable_i32(context, hp_trait, "hp").unwrap_or(0);
                record.max_hp = self.bindable_i32(context, hp_trait, "maxHP").unwrap_or(0);
                record.shield = self.bindable_i32(context, hp_trait, "shield").unwrap_or(0);
                record.max_shield = self.bindable_i32(context, hp_trait, "maxShield").unwrap_or(0);
                record.nanohull = self.bindable_i32(context, hp_trait, "nanohullHP").unwrap_or(0);
                record.nanohull_max =
                    self.bindable_i32(context, hp_trait, "nanohullMaxHP").unwrap_or(0);
            }

            // Movement destination via MoveTrait._destination (null when idle).
            if let Some(move_trait) = self.slot_object(context, entity, "_moveTrait")
                && let Some(destination) = self.slot_object(context, move_trait, "_destination")
                && let (Some(dx), Some(dy)) = (
                    self.slot_f64(context, destination, "_x"),
                    self.slot_f64(context, destination, "_y"),
                )
            {
                record.flags |= layout::entity_flags::MOVING;
                record.dest_x = dx as f32;
                record.dest_y = dy as f32;
            }

            // Name/faction/clan via MapObjectInfoTrait's bindables.
            if let Some(info) = self.slot_object(context, entity, "_info") {
                username = self
                    .bindable_string(context, info, "username")
                    .filter(|name| !name.is_empty());
                record.faction = self.bindable_i32(context, info, "factionID").unwrap_or(0);
                record.clan = self.bindable_i32(context, info, "clanID").unwrap_or(0);
                record.clan_diplomacy =
                    self.bindable_i32(context, info, "clanDiplomacy").unwrap_or(0);
            }
        } else if kind == layout::kind::BASE {
            record.faction = self.slot_val_silent(context, entity, "_factionId").as_i32().unwrap_or(0);
        }

        // Collectable hash doubles as the display name.
        let hash = if class.is_collectable {
            self.slot_val_silent(context, entity, "_hash")
                .to_string_lossy()
                .filter(|hash| !hash.is_empty())
        } else {
            None
        };
        if hash.is_some() {
            record.flags |= layout::entity_flags::COLLECTABLE;
        }
        // Ore's `_typeID`; absent on most classes and silently skipped.
        if let Some(index) = self.silent_index(context, entity, "_typeID")
            && let Some(type_id) = entity.slot(index).as_i32()
        {
            record.flags |= layout::entity_flags::HAS_TYPE_ID;
            record.type_id = type_id;
        }

        let name = username.or(hash).unwrap_or_else(|| {
            entity
                .class_names()
                .next()
                .unwrap_or_else(|| "MapEntity".to_owned())
        });
        record.name_off = frame.strings.len() as u32;
        record.name_len = name.len() as u32;
        frame.strings.extend_from_slice(name.as_bytes());

        if is_hero {
            frame.header.hero_id = id;
            frame.header.hero_x = record.x;
            frame.header.hero_y = record.y;
            frame.header.hero_dest_x = record.dest_x;
            frame.header.hero_dest_y = record.dest_y;
            frame.header.hero_flags = layout::hero_flags::PRESENT
                | if record.flags & layout::entity_flags::MOVING != 0 {
                    layout::hero_flags::MOVING
                } else {
                    0
                };
            frame.header.hero_faction = record.faction;
            frame.header.hero_clan = record.clan;
        }

        frame.entities.push(record);
    }
}

/// Match the game's localized reward sentence around its numeric wildcard.
/// This avoids relying on English words or interpreting deductions as income.
fn template_rewards(text: &str, templates: &[(usize, String, String)]) -> Option<[f64; 4]> {
    for (index, prefix, suffix) in templates {
        let Some(number) = text.strip_prefix(prefix).and_then(|s| s.strip_suffix(suffix)) else { continue; };
        let number = number.trim();
        if number.starts_with('-') || !number.chars().all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '.' | ',' | '\'')) { continue; }
        let digits: String = number.chars().filter(char::is_ascii_digit).collect();
        if let Ok(amount) = digits.parse::<f64>() {
            if amount.is_finite() { let mut rewards = [0.0; 4]; rewards[*index] = amount; return Some(rewards); }
        }
    }
    None
}

#[cfg(test)]
mod telemetry_tests {
    use super::template_rewards;
    #[test]
    fn localized_rewards_match_templates_without_matching_deductions() {
        let templates = vec![(0, "Kazandınız: ".into(), " kredi".into())];
        assert_eq!(template_rewards("Kazandınız: 12.345 kredi", &templates), Some([12345.0, 0.0, 0.0, 0.0]));
        assert_eq!(template_rewards("Kazandınız: -100 kredi", &templates), None);
        assert_eq!(template_rewards("Harcadınız: 100 kredi", &templates), None);
    }
}
