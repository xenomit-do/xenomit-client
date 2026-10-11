//! Native supplementary feature observations for `xenomit/clientFeatures`.
//!
//! Everything is best-effort: each section resolves only the game model,
//! window, or display objects that actually exist, and any field that
//! cannot be read stays `None` instead of being guessed. GUI element ids
//! follow the Flash bridge convention — a visible `GuiManager` window id,
//! optionally followed by `/`-separated child indices — so `ClickGui` can
//! re-resolve and revalidate the same element later.
//!
//! This file also hosts the shared display-tree helpers used by the GUI
//! typed commands in `super::native_actions` (clicks, closes, difficulty
//! selection, and the stage-owned overlay layer).

use ruffle_core::{
    context::UpdateContext,
    introspect::{self, Obj, Val},
};
use xenomit_game_api::{
    Bounds, BoosterState, ClientFeatureState, EconomyState, EventFeatureState, GalaxyGateInfo,
    GateFeatureState, GuiElementState, PetFeatureState, Point,
};

const HERO: &str = "net.bigpoint.darkorbit.Hero";
const GUI_MANAGER: &str = "net.bigpoint.darkorbit.gui.GuiManager";
const GAME_FACADE: &str = "net.bigpoint.darkorbit.mvc.application.GameFacade";

/// Total GUI elements reported across every window (Flash used 256 too).
const MAX_GUI_ELEMENTS: usize = 256;
/// Descendants inspected per element while looking for label text.
const MAX_TEXT_SCAN: usize = 32;
/// Display objects visited while scanning the stage for windows.
const MAX_WINDOW_SCAN: usize = 16384;
/// Difficulty-selection windows served by `SelectGateDifficulty`.
pub(super) const DIFFICULTY_WINDOWS: [&str; 3] = [
    "difficultySelection",
    "trinitytrials_difficultyselect",
    "singularitydrive_difficultyselect",
];

// --- shared primitive readers -------------------------------------------

fn object<'gc>(obj: Obj<'gc>, name: &str) -> Option<Obj<'gc>> {
    obj.slot_by_name(name)?.as_object()
}
fn number(obj: Obj<'_>, name: &str) -> Option<f64> {
    obj.slot_by_name(name)?.as_f64().filter(|n| n.is_finite())
}
fn boolean(obj: Obj<'_>, name: &str) -> Option<bool> {
    obj.slot_by_name(name)?.as_bool()
}
/// `Bindable*`/`BindableCounter` values live in their `_value` slot.
fn bindable(obj: Obj<'_>, name: &str) -> Option<f64> {
    number(object(obj, name)?, "_value")
}
fn property<'gc>(
    context: &mut UpdateContext<'gc>,
    obj: Obj<'gc>,
    name: &str,
) -> Option<Val<'gc>> {
    obj.get_property(context, name).ok()
}
fn property_number<'gc>(
    context: &mut UpdateContext<'gc>,
    obj: Obj<'gc>,
    name: &str,
) -> Option<f64> {
    number(obj, name).or_else(|| {
        property(context, obj, name)?
            .as_f64()
            .filter(|n| n.is_finite())
    })
}
fn property_text<'gc>(
    context: &mut UpdateContext<'gc>,
    obj: Obj<'gc>,
    name: &str,
) -> Option<String> {
    property(context, obj, name)?.to_string_lossy()
}

fn find_class<'gc>(context: &mut UpdateContext<'gc>, name: &str) -> Option<Obj<'gc>> {
    introspect::find_class(context, name)
}

fn facade<'gc>(context: &mut UpdateContext<'gc>) -> Option<Obj<'gc>> {
    find_class(context, GAME_FACADE)
        .and_then(|class| class.call_method(context, "getInstance", &[]).ok())
        .and_then(Val::as_object)
}

fn facade_proxy<'gc>(context: &mut UpdateContext<'gc>, name: &str) -> Option<Obj<'gc>> {
    facade(context)?
        .call_method(context, "retrieveProxy", &[Val::from_str(context, name)])
        .ok()
        .and_then(Val::as_object)
}

fn sequence<'gc>(context: &mut UpdateContext<'gc>, obj: Obj<'gc>, cap: i32) -> Vec<Val<'gc>> {
    if let Some(values) = obj.vector_values() {
        return values;
    }
    let length = property_number(context, obj, "length")
        .unwrap_or(0.0)
        .clamp(0.0, cap as f64) as i32;
    (0..length)
        .filter_map(|index| property(context, obj, &index.to_string()))
        .collect()
}

fn local_name(obj: Obj<'_>) -> Option<String> {
    obj.class_names().next()
}

fn class_contains(obj: Obj<'_>, local: &str) -> bool {
    obj.class_names().any(|name| name == local)
}

// --- shared display-object helpers ---------------------------------------

/// `visible` and `alpha` are public `DisplayObject` getters.
pub(super) fn is_visible<'gc>(context: &mut UpdateContext<'gc>, obj: Obj<'gc>) -> bool {
    property(context, obj, "visible").and_then(Val::as_bool) == Some(true)
        && property(context, obj, "alpha")
            .and_then(Val::as_f64)
            .is_some_and(|alpha| alpha > 0.0)
}

/// Display-object children of a container, empty for leaf objects.
fn children<'gc>(context: &mut UpdateContext<'gc>, obj: Obj<'gc>, cap: i32) -> Vec<Obj<'gc>> {
    let count = property_number(context, obj, "numChildren")
        .unwrap_or(0.0)
        .clamp(0.0, cap as f64) as i32;
    (0..count)
        .filter_map(|index| {
            obj.call_method(context, "getChildAt", &[Val::from_i32(index)])
                .ok()
                .and_then(Val::as_object)
        })
        .collect()
}

/// `enabled` is a readable getter on some controls, write-only on FUI
/// `Coreponent`s — those expose `isEnabled()` instead.
pub(super) fn enabled_of<'gc>(context: &mut UpdateContext<'gc>, obj: Obj<'gc>) -> Option<bool> {
    if let Some(flag) = property(context, obj, "enabled").and_then(Val::as_bool) {
        return Some(flag);
    }
    obj.call_method(context, "isEnabled", &[])
        .ok()
        .and_then(Val::as_bool)
}

/// The Flash bridge's clickability check: a `MouseEvent.CLICK` ("click")
/// listener on the node itself.
pub(super) fn has_click_target<'gc>(
    context: &mut UpdateContext<'gc>,
    obj: Obj<'gc>,
) -> bool {
    obj.call_method(
        context,
        "hasEventListener",
        &[Val::from_str(context, "click")],
    )
    .ok()
    .and_then(Val::as_bool)
        == Some(true)
}

/// Dispatch a real bubbling `MouseEvent.CLICK` through the node's own
/// listeners (game mediators, not shortcut calls).
pub(super) fn dispatch_click<'gc>(
    context: &mut UpdateContext<'gc>,
    obj: Obj<'gc>,
) -> Result<(), String> {
    let event_class =
        find_class(context, "flash.events.MouseEvent").ok_or("MouseEvent class unavailable")?;
    let event = event_class.construct(
        context,
        &[
            Val::from_str(context, "click"),
            Val::from_bool(true),
            Val::from_bool(true),
        ],
    )?;
    obj.call_method(context, "dispatchEvent", &[event])?;
    Ok(())
}

/// `GuiManager.instance`, the owner of the window registry.
pub(super) fn gui_manager<'gc>(context: &mut UpdateContext<'gc>) -> Option<Obj<'gc>> {
    find_class(context, GUI_MANAGER).and_then(|class| object(class, "instance"))
}

/// `GuiManager.getWindow(id)` — the registered window or `None`.
pub(super) fn window_by_id<'gc>(
    context: &mut UpdateContext<'gc>,
    manager: Obj<'gc>,
    id: &str,
) -> Option<Obj<'gc>> {
    manager
        .call_method(context, "getWindow", &[Val::from_str(context, id)])
        .ok()
        .and_then(Val::as_object)
}

/// `GuiManager.closeWindow(window, true)` — the game's own close path.
pub(super) fn close_window<'gc>(
    context: &mut UpdateContext<'gc>,
    manager: Obj<'gc>,
    window: Obj<'gc>,
) -> Result<(), String> {
    manager.call_method(
        context,
        "closeWindow",
        &[Val::from_obj(window), Val::from_bool(true)],
    )?;
    Ok(())
}

/// Stage-pixel bounds of a node, `None` when `getBounds` cannot produce a
/// real rectangle (the node is not indexable then).
fn window_bounds<'gc>(context: &mut UpdateContext<'gc>, obj: Obj<'gc>) -> Option<Bounds> {
    let stage = super::resolve_target(context, "stage")?;
    bounds(context, obj, stage)
}

/// `d.getBounds(stage)` → stage-pixel bounds.
fn bounds<'gc>(
    context: &mut UpdateContext<'gc>,
    obj: Obj<'gc>,
    stage: Obj<'gc>,
) -> Option<Bounds> {
    let rect = obj
        .call_method(context, "getBounds", &[Val::from_obj(stage)])
        .ok()?
        .as_object()?;
    Some(Bounds {
        x: property_number(context, rect, "x")?,
        y: property_number(context, rect, "y")?,
        width: property_number(context, rect, "width")?,
        height: property_number(context, rect, "height")?,
    })
}

fn is_text_field(obj: Obj<'_>) -> bool {
    class_contains(obj, "TextField")
}

/// `TextField.text` for text fields; otherwise the first non-empty text of
/// a small descendant scan (labels nested inside button elements).
fn element_text<'gc>(context: &mut UpdateContext<'gc>, obj: Obj<'gc>) -> Option<String> {
    if is_text_field(obj) {
        return property_text(context, obj, "text").filter(|t| !t.is_empty());
    }
    let mut visited = 0usize;
    let mut queue: Vec<Obj<'gc>> = vec![obj];
    while let Some(node) = queue.pop() {
        if visited >= MAX_TEXT_SCAN {
            break;
        }
        visited += 1;
        for child in children(context, node, 64) {
            if visited >= MAX_TEXT_SCAN {
                break;
            }
            visited += 1;
            if is_text_field(child)
                && let Some(label) = property_text(context, child, "text").filter(|t| !t.is_empty())
            {
                return Some(label);
            }
            queue.push(child);
        }
    }
    None
}

/// Emit one element row, mirroring `DobotBridge.appendGuiTree`: only the
/// window root or nodes with an actual click target are listed, and only
/// while `visible`/`alpha` hold along the path.
fn append_gui_tree<'gc>(
    context: &mut UpdateContext<'gc>,
    node: Obj<'gc>,
    window_id: &str,
    path: String,
    out: &mut Vec<GuiElementState>,
    stage: Obj<'gc>,
) {
    if out.len() >= MAX_GUI_ELEMENTS || !is_visible(context, node) {
        return;
    }
    let is_root = path.is_empty();
    if (is_root || has_click_target(context, node))
        && let Some(rect) = bounds(context, node, stage)
    {
        out.push(GuiElementState {
            id: if is_root {
                window_id.to_owned()
            } else {
                format!("{window_id}/{path}")
            },
            visible: true,
            bounds: Some(rect),
            text: element_text(context, node),
            enabled: enabled_of(context, node),
        });
    }
    for (index, child) in children(context, node, 1024).into_iter().enumerate() {
        if out.len() >= MAX_GUI_ELEMENTS {
            return;
        }
        let child_path = if is_root {
            index.to_string()
        } else {
            format!("{path}/{index}")
        };
        append_gui_tree(context, child, window_id, child_path, out, stage);
    }
}

/// Every registered `SimpleWindow` on stage keyed by its `id`. The window
/// registry itself is a `Dictionary` we cannot enumerate natively, so the
/// stage tree is scanned for `SimpleWindow` instances and each is verified
/// against `GuiManager.getWindow(id)` when the manager resolves.
fn windows<'gc>(
    context: &mut UpdateContext<'gc>,
    manager: Option<Obj<'gc>>,
    stage: Obj<'gc>,
) -> Vec<(String, Obj<'gc>)> {
    let mut found: Vec<(String, Obj<'gc>)> = Vec::new();
    let mut visited = 0usize;
    let mut stack: Vec<Obj<'gc>> = vec![stage];
    while let Some(node) = stack.pop() {
        if visited >= MAX_WINDOW_SCAN || found.len() >= MAX_GUI_ELEMENTS {
            break;
        }
        visited += 1;
        for child in children(context, node, 1024) {
            if visited >= MAX_WINDOW_SCAN {
                break;
            }
            visited += 1;
            if class_contains(child, "SimpleWindow") {
                let Some(id) = property_text(context, child, "id").filter(|id| !id.is_empty())
                else {
                    continue;
                };
                // When the manager resolved, keep only windows it still
                // registers under that id so observed ids stay actionable.
                if let Some(manager) = manager {
                    match window_by_id(context, manager, &id) {
                        Some(registered) if registered.raw() == child.raw() => {}
                        _ => continue,
                    }
                }
                if found.iter().any(|(seen, _)| *seen == id) {
                    continue;
                }
                found.push((id, child));
            } else {
                stack.push(child);
            }
        }
    }
    found
}

/// Split an observed GUI identity into its window id and child-index path:
/// `window_id` → `(id, [])`, `window_id/i/j/...` → `(id, [i, j, ...])`.
/// Empty segments and negative or non-numeric indices are rejected.
fn gui_path(name: &str) -> Option<(&str, Vec<i32>)> {
    let (window_id, path) = match name.split_once('/') {
        Some((window_id, path)) => (window_id, Some(path)),
        None => (name, None),
    };
    if window_id.is_empty() {
        return None;
    }
    let mut indices = Vec::new();
    if let Some(path) = path {
        for part in path.split('/') {
            let index: i32 = part.parse().ok()?;
            if index < 0 {
                return None;
            }
            indices.push(index);
        }
    }
    Some((window_id, indices))
}

/// Resolve an observed GUI identity (`window_id` or `window_id/i/j/...`)
/// back to a live display object. When `click_only`, the resolved node
/// must still carry a click listener and renderable bounds; visibility is
/// revalidated along the whole path either way.
pub(super) fn find_gui<'gc>(
    context: &mut UpdateContext<'gc>,
    name: &str,
    click_only: bool,
) -> Option<Obj<'gc>> {
    let (window_id, path) = gui_path(name)?;
    let manager = gui_manager(context)?;
    let window = window_by_id(context, manager, window_id)?;
    if !is_visible(context, window) {
        return None;
    }
    let mut current = window;
    for index in path {
        current = current
            .call_method(context, "getChildAt", &[Val::from_i32(index)])
            .ok()?
            .as_object()?;
        if !is_visible(context, current) {
            return None;
        }
    }
    // Click targets are the elements `append_gui_tree` would index: a real
    // listener plus renderable bounds. A bare window id resolves to the
    // window itself, so it must be a click target too.
    if click_only
        && !(has_click_target(context, current) && window_bounds(context, current).is_some())
    {
        return None;
    }
    Some(current)
}

/// `difficulty_?(\d+)$` on a locale key (`bday2025_entry_difficulty_16`,
/// `trinitytrials_difficulty3`, `voyagersascentrial_difficulty5`) — the
/// actual difficulty value offered at that dropdown index, same shape the
/// Flash bridge parses.
pub(super) fn difficulty_value(key: &str) -> Option<i32> {
    let index = key.rfind("difficulty")?;
    let rest = key[index + "difficulty".len()..].strip_prefix('_').unwrap_or(
        &key[index + "difficulty".len()..],
    );
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// Collect every `DropDownBox` under `root`, reading each one's offered
/// `_dropDownTextKeys` so only real offerings can be selected.
pub(super) fn dropdowns<'gc>(
    context: &mut UpdateContext<'gc>,
    root: Obj<'gc>,
) -> Vec<(Obj<'gc>, Vec<String>)> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    let mut visited = 0usize;
    while let Some(node) = stack.pop() {
        if visited >= 2048 {
            break;
        }
        visited += 1;
        if class_contains(node, "DropDownBox") {
            let keys = object(node, "_dropDownTextKeys")
                .and_then(Obj::vector_values)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|v| v.to_string_lossy())
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();
            found.push((node, keys));
            continue;
        }
        stack.extend(children(context, node, 512));
    }
    found
}

/// Descendants named `name` — used to find the mediator's `button_go`.
pub(super) fn named_descendants<'gc>(
    context: &mut UpdateContext<'gc>,
    root: Obj<'gc>,
    name: &str,
) -> Vec<Obj<'gc>> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    let mut visited = 0usize;
    while let Some(node) = stack.pop() {
        if visited >= 2048 || found.len() >= 8 {
            break;
        }
        visited += 1;
        for child in children(context, node, 512) {
            if property_text(context, child, "name").as_deref() == Some(name) {
                found.push(child);
            }
            stack.push(child);
        }
    }
    found
}

// --- section captures ----------------------------------------------------

fn capture_gui<'gc>(
    context: &mut UpdateContext<'gc>,
    stage: Obj<'gc>,
) -> Option<Vec<GuiElementState>> {
    let manager = gui_manager(context)?;
    let mut out = Vec::new();
    for (id, window) in windows(context, Some(manager), stage) {
        append_gui_tree(context, window, &id, String::new(), &mut out, stage);
    }
    Some(out)
}

fn hero_ship<'gc>(context: &mut UpdateContext<'gc>) -> Option<Obj<'gc>> {
    let hero = find_class(context, HERO)?;
    let id = hero.slot_by_name("userID")?.as_i32()?;
    super::entity_by_id(context, i64::from(id)).as_object()
}

fn capture_pet<'gc>(context: &mut UpdateContext<'gc>) -> Option<PetFeatureState> {
    let hero = find_class(context, HERO)?;
    let mut state = PetFeatureState {
        owned: boolean(hero, "ownsPet"),
        destroyed: boolean(hero, "isPetDestroyed"),
        ..Default::default()
    };
    let ship = hero_ship(context);
    let pet = ship.and_then(|ship| object(ship, "pet"));
    // `Ship.pet` is only set while the pet is deployed; `container` is the
    // MapEntity getter the Flash bridge checks for the deployed state.
    let deployed = pet
        .map(|pet| {
            object(pet, "_container").is_some()
                || property(context, pet, "container")
                    .and_then(Val::as_object)
                    .is_some()
        })
        .unwrap_or(false);
    state.active = Some(deployed);
    if let Some(pet) = pet
        && deployed
    {
        state.id = number(pet, "_id")
            .or_else(|| property_number(context, pet, "id"))
            .map(|v| v as i32);
        state.position = object(pet, "_position")
            .or_else(|| property(context, pet, "position").and_then(Val::as_object))
            .and_then(|p| {
                Some(Point {
                    x: number(p, "_x").or_else(|| property_number(context, p, "x"))?,
                    y: number(p, "_y").or_else(|| property_number(context, p, "y"))?,
                })
            });
        if let Some(hp) = object(pet, "_hpTrait")
            .or_else(|| property(context, pet, "hp").and_then(Val::as_object))
        {
            state.health = bindable(hp, "hp");
            state.max_health = bindable(hp, "maxHP");
            state.shield = bindable(hp, "shield");
            state.max_shield = bindable(hp, "maxShield");
        }
        state.level = property_number(context, pet, "level")
            .or_else(|| {
                object(pet, "petTrait").and_then(|trait_obj| bindable(trait_obj, "level"))
            })
            .map(|v| v as i32);
        state.idle = boolean(pet, "idleState");
        let mut data = serde_json::Map::new();
        if let Some(owner) = property_number(context, pet, "ownerID") {
            data.insert("owner_id".into(), serde_json::json!(owner as i32));
        }
        if let Some(evasion) = property(context, pet, "evasionProtocolEquiped")
            .and_then(Val::as_bool)
        {
            data.insert("evasion_protocol".into(), serde_json::json!(evasion));
        }
        if !data.is_empty() {
            state.data = serde_json::Value::Object(data);
        }
    }
    // PetWindowAssembly: gear ids and the fuel/repair controls. The
    // `_currentGearId` locator cursor is deliberately ignored — it does
    // not report the server-active gear.
    if let Some(assembly) = find_class(context, "net.bigpoint.darkorbit.net.PetAssembly")
        .and_then(|class| object(class, "instance"))
        && let Some(window) = object(assembly, "_petWindow")
    {
        if let Some(gears) = object(window, "_gears") {
            let values: Vec<i32> = sequence(context, gears, 64)
                .into_iter()
                .filter_map(|value| value.as_i32())
                .collect();
            if !values.is_empty() {
                state.gears = Some(values);
            }
        }
        // getPetWindowContainer() -> getElement(SimpleElement id) ->
        // getChildAt(0) -> BarComponent / repair MovieClip.
        if let Some(container) = window
            .call_method(context, "getPetWindowContainer", &[])
            .ok()
            .and_then(Val::as_object)
        {
            let mut element = |id: i32| -> Option<Obj<'gc>> {
                container
                    .call_method(context, "getElement", &[Val::from_i32(id)])
                    .ok()
                    .and_then(Val::as_object)?
                    .call_method(context, "getChildAt", &[Val::from_i32(0)])
                    .ok()
                    .and_then(Val::as_object)
            };
            // SimpleElement.PET_WINDOW_FUEL_BAR = 63.
            if let Some(bar) = element(63) {
                state.fuel = number(bar, "currentAmount");
                state.max_fuel = number(bar, "maxAmount");
            }
            // SimpleElement.PET_WINDOW_REPAIR_BTN = 67: frame 1 = free or
            // priced repair offered, frame 3 = disabled through money.
            if let Some(clip) = element(67)
                && property(context, clip, "visible").and_then(Val::as_bool) == Some(true)
            {
                let frame = property_number(context, clip, "currentFrame").unwrap_or(0.0) as i32;
                state.repair_available = Some(frame == 1);
            }
        }
    }
    Some(state)
}

/// One `GalaxyGateBuilderModel.gates` row, the same shape shared memory
/// navigation publishes.
fn capture_galaxy_gates<'gc>(
    context: &mut UpdateContext<'gc>,
    model: Obj<'gc>,
) -> Vec<GalaxyGateInfo> {
    let mut gates = Vec::new();
    let Some(values) = object(model, "gates").map(|g| sequence(context, g, 32)) else {
        return gates;
    };
    for (index, value) in values.into_iter().enumerate() {
        let Some(gate) = value.as_object() else {
            continue;
        };
        let slot_i32 = |name: &str| number(gate, name).unwrap_or(0.0) as i32;
        let id = index as i32 + 1;
        gates.push(GalaxyGateInfo {
            id,
            parts: slot_i32("_partsCount"),
            total_parts: slot_i32("_partsTotal"),
            lives: slot_i32("_livesLeft"),
            life_price: slot_i32("_lifePrice"),
            current_wave: slot_i32("_currentWave"),
            total_waves: slot_i32("_totalWave"),
            finished: boolean(gate, "_finishState").unwrap_or(false),
            multiplier: galaxy_multiplier(context, model, id),
        });
    }
    gates
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
                .and_then(Val::as_f64)
        })
        .unwrap_or(0.0) as i32
}

/// Difficulty entries actually offered by an open difficulty window:
/// `(dropdown, parsed values in entry order)`.
fn difficulty_offers<'gc>(
    context: &mut UpdateContext<'gc>,
    manager: Obj<'gc>,
) -> Vec<(String, Vec<i32>)> {
    let mut offers = Vec::new();
    for window_id in DIFFICULTY_WINDOWS {
        let Some(window) = window_by_id(context, manager, window_id) else {
            continue;
        };
        if !is_visible(context, window) {
            continue;
        }
        for (_dropdown, keys) in dropdowns(context, window) {
            let values = difficulty_entries(&keys);
            if !values.is_empty() {
                offers.push((window_id.to_owned(), values));
            }
        }
    }
    offers
}

/// Leading `difficulty*` entries of a dropdown's `_dropDownTextKeys` — the
/// Flash bridge stops scanning a box at the first non-difficulty key, so
/// only this prefix is selectable.
pub(super) fn difficulty_entries(keys: &[String]) -> Vec<i32> {
    keys.iter()
        .take_while(|key| key.contains("difficulty"))
        .filter_map(|key| difficulty_value(key))
        .collect()
}

fn capture_gate<'gc>(context: &mut UpdateContext<'gc>) -> Option<GateFeatureState> {
    let mut state = GateFeatureState::default();
    let mut data = serde_json::Map::new();
    let mut observed = false;
    if let Some(model) = facade_proxy(context, "ggBuilder").and_then(|proxy| object(proxy, "data"))
    {
        observed = true;
        state.builder = capture_galaxy_gates(context, model);
        // Static model facts the builder publishes about spin offers.
        if let Some(model_class) = find_class(
            context,
            "net.bigpoint.darkorbit.mvc.galaxygatebuilder.model.GalaxyGateBuilderModel",
        ) {
            if let Some(amounts) = object(model_class, "spinamounts") {
                let spins: Vec<i32> = sequence(context, amounts, 16)
                    .into_iter()
                    .filter_map(|value| value.as_i32())
                    .collect();
                data.insert("spin_amounts".into(), serde_json::json!(spins));
            }
            for (key, slot) in [
                ("spin_on_sale", "spinOnSale"),
                ("galaxy_gate_day", "galaxyGateDay"),
                ("bonus_rewards_day", "bonusRewardsDay"),
            ] {
                if let Some(flag) = boolean(model_class, slot) {
                    data.insert(key.into(), serde_json::json!(flag));
                }
            }
        }
        if let Some(spin_amount) = model
            .call_method(context, "getSpinAmount", &[])
            .ok()
            .and_then(Val::as_i32)
        {
            data.insert("spin_amount".into(), serde_json::json!(spin_amount));
        }
        if let Some(id) = number(model, "displayedGateID")
            .map(|v| v as i32)
            .filter(|id| *id > 0)
        {
            state.gate_id = Some(id);
            data.insert("displayed_gate_id".into(), serde_json::json!(id));
            if let Some(row) = state.builder.iter().find(|row| row.id == id) {
                state.wave = Some(row.current_wave);
                state.total_waves = Some(row.total_waves);
                state.lives = Some(row.lives);
                state.completed = Some(row.finished);
            }
        }
    }
    // In-progress gate HUD proxies: EternalGateUIData._currWave /
    // _furthestWave and the blacklight variant's BindableInt currWave.
    for (name, wave_slot, furthest_slot, extra_key) in [
        ("eternal_gate", "_currWave", "_furthestWave", "eternal_gate"),
        ("eternal_blacklight", "currWave", "_furthestWave", "eternal_blacklight"),
    ] {
        let Some(proxy_data) =
            facade_proxy(context, name).and_then(|proxy| object(proxy, "data"))
        else {
            continue;
        };
        let current = number(proxy_data, wave_slot).or_else(|| bindable(proxy_data, wave_slot));
        let furthest = number(proxy_data, furthest_slot);
        if current.is_none() && furthest.is_none() {
            continue;
        }
        observed = true;
        data.insert(
            extra_key.into(),
            serde_json::json!({"current_wave": current, "furthest_wave": furthest}),
        );
        if state.wave.is_none() {
            state.wave = current.map(|v| v as i32);
        }
    }
    // Difficulty offerings/selection from an actually open window.
    if let Some(manager) = gui_manager(context) {
        let offers = difficulty_offers(context, manager);
        if !offers.is_empty() {
            observed = true;
            let entries: Vec<serde_json::Value> = offers
                .iter()
                .map(|(id, values)| serde_json::json!({"window": id, "difficulties": values}))
                .collect();
            data.insert("difficulty_offerings".into(), serde_json::json!(entries));
            if state.difficulty.is_none()
                && let Some((window_id, _)) = offers.first()
                && let Some(window) = window_by_id(context, manager, window_id)
            {
                for (dropdown, keys) in dropdowns(context, window) {
                    if difficulty_entries(&keys).is_empty() {
                        continue;
                    }
                    if let Some(index) = property(context, dropdown, "currentEntry")
                        .and_then(Val::as_i32)
                        .filter(|index| *index >= 0)
                        && let Some(value) =
                            keys.get(index as usize).and_then(|k| difficulty_value(k))
                    {
                        state.difficulty = Some(value);
                    }
                    break;
                }
            }
        }
    }
    if observed {
        state.data = serde_json::Value::Object(data);
        Some(state)
    } else {
        None
    }
}

/// `-?[0-9.,']+` score text → the cleaned number, same cleanup the Flash
/// bridge applies to spaceball scoreboard digits.
fn score_number(text: &str) -> Option<f64> {
    let rest = text.strip_prefix('-').unwrap_or(text);
    if rest.is_empty()
        || !rest
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',' || c == '\'')
    {
        return None;
    }
    text.chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect::<String>()
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite())
}

/// Event facts are taken only from the scoreboard widgets and data proxies
/// the game itself maintains (same contract as the Flash bridge):
/// `GuiManager.getScoreElement` rows for the spaceball scoreboard, the
/// `invasion`/`ctb`/`tdm` windows dictionary, `SpaceMapDataProxy` stations,
/// and `EventProxy`'s active event records.
fn capture_event<'gc>(context: &mut UpdateContext<'gc>) -> Option<EventFeatureState> {
    let mut state = EventFeatureState::default();
    let mut data = serde_json::Map::new();
    let mut found = false;
    let mut ball: Option<serde_json::Value> = None;
    let mut active: Vec<String> = Vec::new();
    if let Some(manager) = gui_manager(context) {
        // Spaceball scoreboard: per-company ScoreElement rows. A numeric
        // TextField child is the score; a MovieClip above frame 1 marks the
        // ball owner and carries the speed in its frame number.
        let container_id =
            find_class(context, "net.bigpoint.darkorbit.gui.container.SimpleContainer")
                .and_then(|class| number(class, "CLASS_SPACEBALL"))
                .map(|v| v as i32);
        if let Some(container_id) = container_id {
            let mut scores = serde_json::Map::new();
            let mut owner = 0i32;
            let mut speed = -1i32;
            for company in 1..=3 {
                let Some(element) = manager
                    .call_method(
                        context,
                        "getScoreElement",
                        &[
                            Val::from_str(context, "spaceball"),
                            Val::from_i32(container_id),
                            Val::from_i32(company),
                        ],
                    )
                    .ok()
                    .and_then(Val::as_object)
                else {
                    continue;
                };
                for child in children(context, element, 16) {
                    if is_text_field(child) {
                        if let Some(score) = property_text(context, child, "text")
                            .as_deref()
                            .and_then(score_number)
                        {
                            scores.insert(company.to_string(), serde_json::json!(score));
                        }
                    } else if class_contains(child, "MovieClip")
                        && let Some(frame) = property_number(context, child, "currentFrame")
                        && frame > 1.0
                    {
                        owner = company;
                        speed = frame as i32 - 1;
                    }
                }
            }
            if !scores.is_empty() || speed >= 0 {
                let mut entry = serde_json::Map::new();
                entry.insert("scores".into(), serde_json::Value::Object(scores));
                if owner > 0 {
                    entry.insert("owner_faction".into(), serde_json::json!(owner));
                    state.team = Some(owner);
                }
                if speed >= 0 {
                    entry.insert("speed".into(), serde_json::json!(speed));
                }
                ball = Some(serde_json::Value::Object(entry));
                found = true;
            }
        }
        if ball.is_some() {
            active.push("spaceball".to_owned());
        }
        // `windows` is a `Dictionary` of all known SimpleWindows; the event
        // widgets count as active while visible or maximized.
        if let Some(windows) = property(context, manager, "windows").and_then(Val::as_object) {
            for name in ["invasion", "ctb", "tdm"] {
                let Some(window) = property(context, windows, name).and_then(Val::as_object)
                else {
                    continue;
                };
                let visible =
                    property(context, window, "visible").and_then(Val::as_bool) == Some(true);
                let maximized =
                    property(context, window, "maximized").and_then(Val::as_bool) == Some(true);
                if visible || maximized {
                    active.push(name.to_owned());
                }
            }
        }
    }
    // Station facts the SpaceMapDataProxy maintains (getters on the VO).
    if let Some(stations) = facade_proxy(context, "SpaceMapDataProxy")
        .and_then(|proxy| property(context, proxy, "spaceMapStations").and_then(Val::as_object))
    {
        let mut rows = Vec::new();
        for value in sequence(context, stations, 16) {
            let Some(station) = value.as_object() else {
                continue;
            };
            rows.push(serde_json::json!({
                "map_id": property_number(context, station, "mapId"),
                "status": property_number(context, station, "status"),
                "faction": property_number(context, station, "owningFactionId"),
                "clan": property_text(context, station, "clanName"),
                "asteroid": property_text(context, station, "asteroidName"),
                "last_changed_unix_ms": property_number(context, station, "lastChangedTimestamp"),
            }));
        }
        if !rows.is_empty() {
            data.insert("stations".into(), serde_json::json!(rows));
            found = true;
        }
    }
    // EventProxy's VO records: active event class names are the real
    // observed identifiers.
    if let Some(events) = facade_proxy(context, "EventProxy")
        .and_then(|proxy| object(proxy, "data"))
        .and_then(Obj::vector_values)
    {
        for value in events.iter().take(64) {
            let Some(event) = value.as_object() else {
                continue;
            };
            let is_active = boolean(event, "_active").or_else(|| {
                event
                    .get_property(context, "isActive")
                    .ok()
                    .and_then(Val::as_bool)
            });
            if is_active == Some(true)
                && let Some(name) = local_name(event)
                && !active.contains(&name)
            {
                active.push(name);
            }
        }
    }
    if let Some(instance) = find_class(context, "net.bigpoint.darkorbit.utils.time.ServerTime")
        .and_then(|class| object(class, "instance"))
    {
        let ms = object(instance, "currentTime")
            .and_then(|counter| number(counter, "_value"))
            .filter(|v| *v > 0.0 && v.is_finite());
        if let Some(ms) = ms {
            state.server_time_unix_ms = Some(ms as u64);
        }
    }
    if let Some(hero) = find_class(context, HERO)
        && let Some(faction) = number(hero, "factionID")
            .or_else(|| bindable(hero, "factionID"))
            .map(|v| v as i32)
            .filter(|id| *id > 0)
    {
        data.insert("hero_faction".into(), serde_json::json!(faction));
    }
    if let Some(ball) = ball {
        data.insert("spaceball".into(), ball);
    }
    if !active.is_empty() {
        state.active = Some(active);
    }
    if !found && state.active.is_none() {
        return None;
    }
    state.data = serde_json::Value::Object(data);
    Some(state)
}

fn capture_economy<'gc>(context: &mut UpdateContext<'gc>) -> Option<EconomyState> {
    let hero = find_class(context, HERO)?;
    let mut state = EconomyState {
        credits: number(hero, "creditsAmount"),
        uridium: number(hero, "uridiumAmount"),
        cargo: bindable(hero, "cargo"),
        max_cargo: bindable(hero, "maxCargo"),
        ..Default::default()
    };
    // `Hero.tradeAreaType` (`OreTradeInfoCommand.TRADETYPE_INACTIVE` = 0)
    // is the real trade-state fact; `inTradeArea` is kept as an extra.
    state.trade_available = number(hero, "tradeAreaType").map(|area| area != 0.0);
    // PatternManager.orePatterns is a dense Array of OrePattern.
    if let Some(patterns) = find_class(context, "net.bigpoint.darkorbit.pattern.PatternManager")
        .and_then(|class| object(class, "orePatterns"))
    {
        for (index, value) in sequence(context, patterns, 64).into_iter().enumerate() {
            let Some(pattern) = value.as_object() else {
                continue;
            };
            if let Some(count) = property_number(context, pattern, "count")
                .filter(|count| *count >= 0.0)
            {
                state.ores.insert(index as i32, count);
            }
        }
    }
    let mut data = serde_json::Map::new();
    for (key, slot) in [
        ("honor", "honorPoints"),
        ("experience", "experiencePoints"),
        ("jackpot", "jackpotAmount"),
        ("trade_area_type", "tradeAreaType"),
    ] {
        if let Some(value) = number(hero, slot) {
            data.insert(key.into(), serde_json::json!(value));
        }
    }
    if let Some(in_area) = boolean(hero, "inTradeArea") {
        data.insert("in_trade_area".into(), serde_json::json!(in_area));
    }
    if let Some(manager) = gui_manager(context)
        && let Some(window) = window_by_id(context, manager, "ore_trade")
    {
        data.insert(
            "ore_trade_window_visible".into(),
            serde_json::json!(is_visible(context, window)),
        );
    }
    state.data = serde_json::Value::Object(data);
    Some(state)
}

/// Active booster categories from the game's `BoosterProxy`: per category
/// the summed percentage (`BoosterCategory.totalPercent`) and the soonest
/// timer (`getMinimumTimer`, a `BindableCountDown` in seconds).
fn capture_boosters<'gc>(context: &mut UpdateContext<'gc>) -> Option<Vec<BoosterState>> {
    let proxy = facade_proxy(context, "BoosterProxy")?;
    let model = object(proxy, "data")
        .or_else(|| property(context, proxy, "data").and_then(Val::as_object))?;
    let categories = object(model, "_activeBoosterCategories")?;
    let mut boosters = Vec::new();
    for value in sequence(context, categories, 128) {
        let Some(category) = value.as_object() else {
            continue;
        };
        let Some(percent) =
            property_number(context, category, "totalPercent").filter(|percent| *percent > 0.0)
        else {
            continue;
        };
        let seconds_left = category
            .call_method(context, "getMinimumTimer", &[])
            .ok()
            .and_then(Val::as_object)
            .and_then(|timer| property_number(context, timer, "value"))
            .filter(|seconds| *seconds > 0.0);
        boosters.push(BoosterState {
            category: category
                .slot_by_name("_category")
                .and_then(|name| name.to_string_lossy())
                .unwrap_or_default(),
            percent,
            seconds_left,
        });
    }
    Some(boosters)
}

/// Full supplementary feature state, captured fresh at request time.
pub(super) fn feature_state<'gc>(context: &mut UpdateContext<'gc>) -> ClientFeatureState {
    let observed_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let stage = super::resolve_target(context, "stage");
    ClientFeatureState {
        observed_at_unix_ms,
        // Capability names follow the Flash bridge's contract; the extra
        // entries name features this client actually implements.
        capabilities: [
            "gui.observe",
            "gui.click",
            "gui.close",
            "ore.observe",
            "ore.refine",
            "ore.sell",
            "pet.observe",
            "pet.activate",
            "pet.gear",
            "economy.observe",
            "booster.observe",
            "events",
            "gate.builder",
            "gate.difficulty",
            "gate.spin",
            "gate.place",
            "gate.buy_life",
            "group.accept_invite",
            "overlay.set",
            "overlay.remove",
            "item.use",
            "config.select",
            "revive.free",
            "portal.jump",
        ]
        .iter()
        .map(|cap| cap.to_string())
        .collect(),
        gui: stage.and_then(|stage| capture_gui(context, stage)),
        pet: capture_pet(context),
        gate: capture_gate(context),
        event: capture_event(context),
        economy: capture_economy(context),
        boosters: capture_boosters(context),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difficulty_values_parse_trailing_integers() {
        assert_eq!(difficulty_value("bday2025_entry_difficulty_16"), Some(16));
        assert_eq!(difficulty_value("bday2025_entry_difficulty_4"), Some(4));
        assert_eq!(difficulty_value("trinitytrials_difficulty3"), Some(3));
        assert_eq!(difficulty_value("voyagersascentrial_difficulty5"), Some(5));
        // `difficulty_?(\d+)$` — digits must sit right at the end and a
        // bare trailing integer without `difficulty` is not an offer.
        assert_eq!(difficulty_value("trinitytrials_gate_map_3"), None);
        assert_eq!(difficulty_value("difficulty_16_extra"), None);
        assert_eq!(difficulty_value("quality_low"), None);
        assert_eq!(difficulty_value(""), None);
        assert_eq!(difficulty_value("difficulty_"), None);
        // Absurdly long suffixes must not panic or wrap.
        assert_eq!(
            difficulty_value("difficulty_99999999999999999999999999"),
            None
        );
    }

    #[test]
    fn gui_paths_split_window_and_indices() {
        assert_eq!(gui_path("spaceball"), Some(("spaceball", vec![])));
        assert_eq!(gui_path("spaceball/0/2"), Some(("spaceball", vec![0, 2])));
        assert_eq!(gui_path("difficultySelection/3"), Some(("difficultySelection", vec![3])));
        assert_eq!(gui_path("w/-1"), None);
        assert_eq!(gui_path("w/"), None);
        assert_eq!(gui_path("/0"), None);
        assert_eq!(gui_path(""), None);
        assert_eq!(gui_path("w/abc"), None);
        assert_eq!(gui_path("w/1x"), None);
        assert_eq!(gui_path("w/1/2/0"), Some(("w", vec![1, 2, 0])));
    }

    #[test]
    fn feature_state_round_trips_through_serde() {
        let state = ClientFeatureState {
            observed_at_unix_ms: 1_700_000_000_000,
            capabilities: vec!["gui.observe".into()],
            gui: Some(vec![GuiElementState {
                id: "spaceball/0/2".into(),
                visible: true,
                bounds: Some(Bounds {
                    x: 1.0,
                    y: 2.0,
                    width: 30.0,
                    height: 40.0,
                }),
                text: Some("MMO".into()),
                enabled: Some(true),
            }]),
            pet: Some(PetFeatureState {
                owned: Some(true),
                gears: Some(vec![1, 2, 6]),
                ..Default::default()
            }),
            gate: Some(GateFeatureState {
                gate_id: Some(1),
                builder: vec![GalaxyGateInfo {
                    id: 1,
                    parts: 34,
                    total_parts: 34,
                    lives: 2,
                    life_price: 500,
                    current_wave: 5,
                    total_waves: 10,
                    finished: false,
                    multiplier: 3,
                }],
                ..Default::default()
            }),
            event: Some(EventFeatureState {
                server_time_unix_ms: Some(1_700_000_000_500),
                active: Some(vec!["FrostedGatesEvent".into()]),
                ..Default::default()
            }),
            economy: Some(EconomyState {
                credits: Some(1_000.0),
                trade_available: Some(false),
                ..Default::default()
            }),
            boosters: Some(vec![BoosterState {
                category: "HP".into(),
                percent: 10.0,
                seconds_left: Some(10_800.0),
            }]),
        };
        let json = serde_json::to_value(&state).unwrap();
        let back: ClientFeatureState = serde_json::from_value(json).unwrap();
        assert_eq!(back, state);
    }
}
