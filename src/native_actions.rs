//! Verified bindings to the decompiled game model/UI request paths.
use std::cell::RefCell;
use std::collections::HashMap;

use ruffle_core::{
    context::UpdateContext,
    introspect::{self, Obj, ObjHandle, Val},
};
use xenomit_game_api::GameCommand;

use super::client_features as gui;

const HERO: &str = "net.bigpoint.darkorbit.Hero";
const FACADE: &str = "net.bigpoint.darkorbit.mvc.application.GameFacade.getInstance()";

thread_local! {
    /// Client-owned overlay layer on the stage plus its `TextField`s. The
    /// handles keep the objects rooted across update contexts; the layer is
    /// removed from the stage when the last overlay is removed.
    static OVERLAY_LAYER: RefCell<Option<ObjHandle>> = const { RefCell::new(None) };
    static OVERLAYS: RefCell<HashMap<String, ObjHandle>> =
        RefCell::new(HashMap::new());
}
pub(super) fn execute<'gc>(
    context: &mut UpdateContext<'gc>,
    command: GameCommand,
) -> Result<(), String> {
    match command {
        GameCommand::SetOverlay {
            id,
            text,
            x,
            y,
            color,
        } => {
            set_overlay(context, &id, &text, x, y, color)?;
        }
        GameCommand::RemoveOverlay { id } => {
            remove_overlay(context, &id)?;
        }
        GameCommand::ClickGui { name } => {
            let target = gui::find_gui(context, &name, true)
                .ok_or("GUI element is no longer visible or clickable")?;
            gui::dispatch_click(context, target)?;
        }
        GameCommand::CloseGui { name } => {
            let manager = gui::gui_manager(context).ok_or("Gui manager unavailable")?;
            let window = gui::window_by_id(context, manager, &name)
                .ok_or("GUI window is no longer open")?;
            if !gui::is_visible(context, window) {
                return Err("GUI window is no longer open".into());
            }
            gui::close_window(context, manager, window)?;
        }
        GameCommand::SelectGateDifficulty { gui, difficulty } => {
            select_gate_difficulty(context, &gui, difficulty)?;
        }
        GameCommand::JumpPortal { portal_id } => {
            let portal = super::entity_by_id(context, i64::from(portal_id))
                .as_object()
                .ok_or("Portal disappeared")?;
            if !portal.class_names().any(|name| name == "Portal") {
                return Err("Selected entity is not a portal".into());
            }
            if slot_bool(portal, "isJumping") == Some(true) {
                return Ok(());
            }
            let hero = hero_ship(context)?;
            let position = slot_object(portal, "_position").ok_or("Portal position unavailable")?;
            let hero_position =
                slot_object(hero, "_position").ok_or("Hero position unavailable")?;
            let dx = slot_number(position, "_x")? - slot_number(hero_position, "_x")?;
            let dy = slot_number(position, "_y")? - slot_number(hero_position, "_y")?;
            if dx.hypot(dy) > 600.0 {
                return Err("Hero is too far from the portal".into());
            }
            let request = construct(context, "JumpRequest", &[])?;
            send(context, request)?;
        }
        GameCommand::SetPet { enabled } => {
            let class = introspect::find_class(context, HERO).ok_or("Hero unavailable")?;
            if slot_bool(class, "ownsPet") != Some(true)
                || slot_bool(class, "isPetDestroyed") == Some(true)
            {
                return Err("Pet unavailable or destroyed".into());
            }
            let hero = hero_ship(context)?;
            let active = slot_object(hero, "pet")
                .is_some_and(|pet| slot_object(pet, "_container").is_some());
            if active != enabled {
                let request = construct(
                    context,
                    "PetRequest",
                    &[Val::from_i32(if enabled { 0 } else { 1 })],
                )?;
                send(context, request)?;
            }
        }
        GameCommand::SelectPetGear { gear_type, target } => {
            let hero = hero_ship(context)?;
            if slot_object(hero, "pet")
                .and_then(|pet| slot_object(pet, "_container"))
                .is_none()
            {
                return Err("Pet is not active".into());
            }
            let pet = super::resolve_target(
                context,
                "net.bigpoint.darkorbit.net.PetAssembly.getInstance()",
            )
            .ok_or("Pet manager unavailable")?;
            let window = slot_object(pet, "_petWindow").ok_or("Pet gears unavailable")?;
            let gears = slot_object(window, "_gears").ok_or("Pet gears unavailable")?;
            let length = gears
                .get_property(context, "length")?
                .as_i32()
                .unwrap_or(0)
                .clamp(0, 64);
            if !(0..length).any(|index| {
                gears
                    .get_property(context, &index.to_string())
                    .ok()
                    .and_then(Val::as_i32)
                    == Some(gear_type)
            }) {
                return Err("Pet gear is not equipped".into());
            }
            let gear = construct(context, "PetGearTypeModule", &[Val::from_i32(gear_type)])?;
            let request = construct(
                context,
                "PetGearActivationRequest",
                &[gear, Val::from_i32(target)],
            )?;
            send(context, request)?;
        }
        GameCommand::SelectConfiguration { configuration } => {
            if !(1..=2).contains(&configuration) {
                return Err("Ship configuration must be 1 or 2".into());
            }
            let settings =
                introspect::find_class(context, "net.bigpoint.darkorbit.settings.Settings")
                    .ok_or("Settings unavailable")?;
            // `_selectedConfiguration` is the slot; `selectedConfiguration` is
            // the public getter — read whichever resolves.
            if settings
                .get_property(context, "selectedConfiguration")
                .ok()
                .and_then(Val::as_i32)
                .or_else(|| {
                    settings
                        .slot_by_name("_selectedConfiguration")
                        .and_then(Val::as_i32)
                })
                == Some(i32::from(configuration))
            {
                return Ok(());
            }
            let hero = introspect::find_class(context, HERO).ok_or("Hero unavailable")?;
            let id = hero
                .slot_by_name("userID")
                .and_then(Val::as_i32)
                .ok_or("Hero ID unavailable")?;
            let session = hero
                .slot_by_name("sessionID")
                .and_then(Val::to_string_lossy)
                .ok_or("Session unavailable")?;
            let payload = serde_json::json!([
                "S",
                ["CFG", configuration.to_string(), id.to_string(), session]
            ]);
            let args = [
                Val::from_str(context, "ConnectionNotificationSendCommand"),
                super::json_to_val(context, &payload),
            ];
            super::resolve_target(context, FACADE)
                .ok_or("Game facade unavailable")?
                .call_method(context, "sendNotification", &args)?;
        }
        GameCommand::UseItem { item_id, activate } => {
            let facade = super::resolve_target(context, FACADE).ok_or("Game facade unavailable")?;
            let proxy_name = Val::from_str(context, "ItemsControlMenuProxy");
            let proxy = facade
                .call_method(context, "retrieveProxy", &[proxy_name])?
                .as_object()
                .ok_or("Item menu unavailable")?;
            let name = Val::from_str(context, &item_id);
            let item = proxy
                .call_method(context, "getItemVOByItemId", &[name])?
                .as_object()
                .ok_or("Item is not equipped")?;
            if slot_bool(item, "_available") != Some(true)
                || slot_bool(item, "_blocked") != Some(false)
                || (activate && slot_bool(item, "_activatable") != Some(true))
            {
                return Err("Item unavailable, blocked, or on cooldown".into());
            }
            if !activate && slot_bool(item, "_selected") == Some(true) {
                return Ok(());
            }
            let request = construct(
                context,
                "MenuActionRequest",
                &[name, Val::from_i32(i32::from(activate)), Val::from_i32(0)],
            )?;
            proxy.call_method(context, "sendActionRequest", &[request])?;
        }
        GameCommand::Revive { repair_type } => {
            let hero = introspect::find_class(context, HERO).ok_or("Hero unavailable")?;
            if slot_bool(hero, "isKilled") != Some(true) {
                return Err("Hero is not destroyed".into());
            }
            let screen = super::resolve_target(
                context,
                "net.bigpoint.darkorbit.gui.windows.components.Killscreen.getInstance()",
            )
            .ok_or("Kill screen unavailable")?;
            let options = slot_object(screen, "_optionTypes").ok_or("Repair options not loaded")?;
            let length = options
                .get_property(context, "length")?
                .as_i32()
                .unwrap_or(0)
                .clamp(0, 14);
            let offered = (0..length).any(|index| {
                options
                    .get_property(context, &index.to_string())
                    .ok()
                    .and_then(Val::as_i32)
                    == Some(repair_type)
            });
            if !offered {
                return Err("Repair option is not offered on this map".into());
            }
            let choices = slot_object(screen, "optionVOs")
                .and_then(Obj::vector_values)
                .ok_or("Repair prices unavailable")?;
            let choice = usize::try_from(repair_type)
                .ok()
                .and_then(|i| choices.get(i))
                .and_then(|v| v.as_object())
                .ok_or("Repair option invalid")?;
            // Automatic safety recovery only uses a free base repair.
            if ![1, 6].contains(&repair_type)
                || slot_number(choice, "repairCost")? != 0.0
                || slot_number(choice, "cooldown")? != 0.0
                || slot_bool(choice, "affordable") != Some(true)
            {
                return Err("Automatic revive requires an available free base repair".into());
            }
            let selection = construct(
                context,
                "KillScreenOptionTypeModule",
                &[Val::from_i32(repair_type)],
            )?;
            let id = hero.slot_by_name("userID").ok_or("Hero ID unavailable")?;
            let faction = slot_object(hero, "factionID")
                .and_then(|v| v.slot_by_name("_value"))
                .ok_or("Faction unavailable")?;
            let session = hero
                .slot_by_name("sessionID")
                .ok_or("Session unavailable")?;
            let login = construct(context, "LoginRequest", &[id, faction, session])?;
            let request = construct(context, "KillScreenRepairRequest", &[selection, login])?;
            if let Some(settings) =
                introspect::find_class(context, "net.bigpoint.darkorbit.settings.Settings")
            {
                if let Some(targets) = slot_object(screen, "targetMaps") {
                    let kind = screen
                        .slot_by_name("_optionType")
                        .and_then(Val::as_i32)
                        .ok_or("Kill-screen destination type unavailable")?;
                    let map = targets
                        .dynamic_get(context, &kind.to_string())
                        .ok_or("Revive destination unavailable")?;
                    settings.set_property(context, "nextMapID", map)?;
                }
                settings.set_property(context, "lastMapID", Val::from_i32(-1))?;
            }
            send(context, request)?;
            // Mirror Killscreen.cleanup using its public component operations;
            // the private cleanup method cannot be called as a public property.
            if let Some(map) = super::resolve_target(
                context,
                "net.bigpoint.darkorbit.managers.ScreenManager.getInstance().map",
            ) {
                let _ = map.call_method(context, "cleanup", &[]);
            }
            let _ = screen.call_method(context, "clearOptions", &[]);
            if let Some(window) = slot_object(screen, "repairWindow") {
                let _ = window.call_method(context, "cleanup", &[]);
            }
        }
        GameCommand::SpinGalaxyGate {
            gate_id,
            spin_amount,
            use_multiplier,
        } => {
            validate_gate(gate_id)?;
            if ![1, 5, 10, 100].contains(&spin_amount) {
                return Err("Galaxy spin amount must be 1, 5, 10, or 100".into());
            }
            gate_vo(context, gate_id)?;
            let request = construct(
                context,
                "GalaxyGateBuilderMultiEnergyRequest",
                &[
                    Val::from_i32(gate_id),
                    Val::from_bool(use_multiplier),
                    Val::from_i32(i32::from(spin_amount)),
                ],
            )?;
            send(context, request)?;
        }
        GameCommand::PlaceGalaxyGate { gate_id } => {
            validate_gate(gate_id)?;
            if slot_bool(gate_vo(context, gate_id)?, "_finishState") != Some(true) {
                return Err("Galaxy gate is not complete".into());
            }
            let request = construct(
                context,
                "GalaxyGateBuilderSetupGateRequest",
                &[Val::from_i32(gate_id)],
            )?;
            send(context, request)?;
        }
        GameCommand::BuyGalaxyLife { gate_id } => {
            validate_gate(gate_id)?;
            match slot_number(gate_vo(context, gate_id)?, "_lifePrice") {
                Ok(price) if price >= 0.0 => {}
                _ => return Err("Galaxy gate offers no purchasable life".into()),
            }
            let request = construct(
                context,
                "GalaxyGateBuilderBuyLifeRequest",
                &[Val::from_i32(gate_id)],
            )?;
            send(context, request)?;
        }
        GameCommand::AcceptGroupInvite { inviter_id } => {
            if inviter_id <= 0 {
                return Err("Group inviter id is missing".into());
            }
            let request = construct(
                context,
                "InvitationAcceptRequest",
                &[Val::from_i32(inviter_id)],
            )?;
            send(context, request)?;
        }
        GameCommand::SellOre { ore_type, amount } => {
            // The client UI only exposes sale controls for these tradeable
            // materials. Never let a generic OreTypeModule turn this into an
            // arbitrary inventory operation.
            if !matches!(ore_type, 0..=2 | 4..=8 | 28) {
                return Err("Ore type is not sellable through the trade window".into());
            }
            let count = valid_ore_amount(amount)?;
            let inventory = observed_ore_count(context, ore_type)?;
            if count > inventory {
                return Err(format!("Requested sale exceeds observed ore inventory ({inventory})"));
            }
            let ore_type = construct(context, "OreTypeModule", &[Val::from_i32(ore_type)])?;
            let count = construct(context, "OreCountModule", &[ore_type, Val::from_f64(count)])?;
            let request = construct(context, "TradeSellOreRequest", &[count])?;
            send(context, request)?;
        }
        GameCommand::RefineOre { ore_type, amount } => {
            let count = valid_ore_amount(amount)?;
            let capacity = observed_refine_capacity(context, ore_type)?;
            if count > capacity {
                return Err(format!("Requested refinement exceeds observed recipe capacity ({capacity})"));
            }
            let ore_type = construct(context, "OreTypeModule", &[Val::from_i32(ore_type)])?;
            let count = construct(context, "OreCountModule", &[ore_type, Val::from_f64(count)])?;
            let request = construct(context, "LabRefinementRequest", &[count])?;
            send(context, request)?;
        }
    }
    Ok(())
}
fn valid_ore_amount(amount: u64) -> Result<f64, String> {
    if amount == 0 {
        return Err("Ore amount must be positive".into());
    }
    Ok(amount as f64)
}
fn observed_ore_count(context: &mut UpdateContext<'_>, ore_type: i32) -> Result<f64, String> {
    let ore = introspect::find_class(context, "net.bigpoint.darkorbit.map.model.env.collectable.Ore")
        .ok_or("Ore inventory model unavailable")?;
    let ids = Val::from_obj(introspect::new_array(context, &[Val::from_i32(ore_type)]));
    let patterns = ore.call_method(context, "getOres", &[ids])?.as_object()
        .ok_or("Ore inventory unavailable")?;
    let pattern = patterns.get_property(context, "0")?.as_object()
        .ok_or("Ore inventory unavailable")?;
    pattern.get_property(context, "count")?.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
        .ok_or_else(|| "Ore inventory count unavailable".into())
}
fn observed_refine_capacity(context: &mut UpdateContext<'_>, target: i32) -> Result<f64, String> {
    let ore = introspect::find_class(context, "net.bigpoint.darkorbit.pattern.PatternManager")
        .ok_or("Refinement recipes unavailable")?;
    let patterns = ore.slot_by_name("orePatterns").and_then(Val::as_object)
        .ok_or("Refinement recipes unavailable")?;
    let target_pattern = patterns.dynamic_get(context, &target.to_string()).and_then(Val::as_object)
        .ok_or("Refinement target unavailable")?;
    let refiners = target_pattern.get_property(context, "refiners")?.as_object()
        .ok_or("Refinement recipe unavailable")?;
    let length = refiners.get_property(context, "length")?.as_i32().unwrap_or(0).clamp(0, 3);
    if !(2..=3).contains(&length) {
        return Err("Refinement recipe is not available".into());
    }
    let mut capacity = f64::INFINITY;
    for index in 0..length {
        let refiner = refiners.get_property(context, &index.to_string())?.as_object()
            .ok_or("Refinement recipe unavailable")?;
        let ore_id = refiner.get_property(context, "oreID")?.as_i32()
            .ok_or("Refinement ingredient unavailable")?;
        let required = refiner.get_property(context, "count")?.as_f64()
            .filter(|n| n.is_finite() && *n > 0.0).ok_or("Refinement recipe count unavailable")?;
        capacity = capacity.min((observed_ore_count(context, ore_id)? / required).floor());
    }
    if capacity.is_finite() { Ok(capacity.max(0.0)) } else { Err("Refinement capacity unavailable".into()) }
}
fn validate_gate(gate_id: i32) -> Result<(), String> {
    if matches!(gate_id, 1..=8 | 12 | 19) {
        Ok(())
    } else {
        Err("Unknown galaxy gate id".into())
    }
}
/// The builder model's own `getGateVO` lookup — gate commands only run
/// against observed, server-provided gate state.
fn gate_vo<'gc>(context: &mut UpdateContext<'gc>, gate_id: i32) -> Result<Obj<'gc>, String> {
    let facade = super::resolve_target(context, FACADE).ok_or("Game facade unavailable")?;
    let model = facade
        .call_method(context, "retrieveProxy", &[Val::from_str(context, "ggBuilder")])?
        .as_object()
        .and_then(|proxy| proxy.slot_by_name("data"))
        .and_then(Val::as_object)
        .ok_or("Galaxy gate builder is unavailable")?;
    model
        .call_method(context, "getGateVO", &[Val::from_i32(gate_id)])?
        .as_object()
        .ok_or_else(|| "Galaxy gate is not initialized".to_owned())
}
fn slot_object<'gc>(obj: Obj<'gc>, name: &str) -> Option<Obj<'gc>> {
    obj.slot_by_name(name)?.as_object()
}
fn slot_bool(obj: Obj<'_>, name: &str) -> Option<bool> {
    obj.slot_by_name(name)?.as_bool()
}
fn slot_number(obj: Obj<'_>, name: &str) -> Result<f64, String> {
    obj.slot_by_name(name)
        .and_then(Val::as_f64)
        .filter(|n| n.is_finite())
        .ok_or_else(|| format!("{name} unavailable"))
}
fn hero_ship<'gc>(context: &mut UpdateContext<'gc>) -> Result<Obj<'gc>, String> {
    let hero = introspect::find_class(context, HERO).ok_or("Hero unavailable")?;
    let id = hero
        .slot_by_name("userID")
        .and_then(Val::as_i32)
        .ok_or("Hero ID unavailable")?;
    super::entity_by_id(context, i64::from(id))
        .as_object()
        .ok_or_else(|| "Hero ship unavailable".into())
}
fn construct<'gc>(
    context: &mut UpdateContext<'gc>,
    name: &str,
    args: &[Val<'gc>],
) -> Result<Val<'gc>, String> {
    introspect::find_class(
        context,
        &format!("net.bigpoint.darkorbit.com.module.{name}"),
    )
    .ok_or_else(|| format!("{name} class unavailable"))?
    .construct(context, args)
}
fn send<'gc>(context: &mut UpdateContext<'gc>, request: Val<'gc>) -> Result<(), String> {
    let name = Val::from_str(context, "SEND_NETTY_REQUEST");
    super::resolve_target(context, FACADE)
        .ok_or("Game facade unavailable")?
        .call_method(context, "sendNotification", &[name, request])?;
    Ok(())
}

// --- Client-owned overlays (backend text, never game UI) ------------------
//
// Mirrors `DobotBridge.setOverlay`/`removeOverlay`: one mouse-transparent
// `Sprite` layered on the stage, one `TextField` per id, reused on update.

fn set_overlay<'gc>(
    context: &mut UpdateContext<'gc>,
    id: &str,
    text: &str,
    x: f64,
    y: f64,
    color: u32,
) -> Result<(), String> {
    if id.is_empty() || id.chars().count() > 128 || text.chars().count() > 2048 {
        return Err("Invalid overlay id, text, or position".into());
    }
    if !x.is_finite() || !y.is_finite() {
        return Err("Invalid overlay id, text, or position".into());
    }
    let stage = super::resolve_target(context, "stage").ok_or("Game stage unavailable")?;
    let layer = OVERLAY_LAYER.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|handle| handle.fetch(context))
    });
    let layer = match layer {
        Some(layer) => layer,
        None => {
            let layer = introspect::find_class(context, "flash.display.Sprite")
                .ok_or("Sprite class unavailable")?
                .construct(context, &[])?
                .as_object()
                .ok_or("Sprite class unavailable")?;
            layer.set_property(context, "mouseEnabled", Val::from_bool(false))?;
            layer.set_property(context, "mouseChildren", Val::from_bool(false))?;
            stage.call_method(context, "addChild", &[Val::from_obj(layer)])?;
            let handle = layer.stash(context);
            OVERLAY_LAYER.with(|cell| *cell.borrow_mut() = Some(handle));
            layer
        }
    };
    let existing = OVERLAYS.with(|cell| {
        cell.borrow()
            .get(id)
            .map(|handle| handle.fetch(context))
    });
    let field = match existing {
        Some(field) => {
            // The layer is dropped when the last overlay is removed, so a
            // retained field may be detached — re-attach it on update.
            let attached = field
                .get_property(context, "parent")
                .ok()
                .and_then(Val::as_object)
                .is_some_and(|parent| parent.raw() == layer.raw());
            if !attached {
                layer.call_method(context, "addChild", &[Val::from_obj(field)])?;
            }
            field
        }
        None => {
            let field = introspect::find_class(context, "flash.text.TextField")
                .ok_or("TextField class unavailable")?
                .construct(context, &[])?
                .as_object()
                .ok_or("TextField class unavailable")?;
            field.set_property(context, "mouseEnabled", Val::from_bool(false))?;
            field.set_property(context, "mouseWheelEnabled", Val::from_bool(false))?;
            field.set_property(context, "selectable", Val::from_bool(false))?;
            field.set_property(context, "autoSize", Val::from_str(context, "left"))?;
            layer.call_method(context, "addChild", &[Val::from_obj(field)])?;
            let handle = field.stash(context);
            OVERLAYS.with(|cell| cell.borrow_mut().insert(id.to_owned(), handle));
            field
        }
    };
    field.set_property(context, "text", Val::from_str(context, text))?;
    let format = introspect::find_class(context, "flash.text.TextFormat")
        .ok_or("TextFormat class unavailable")?
        .construct(
            context,
            &[
                Val::from_str(context, "Arial"),
                Val::from_i32(14),
                Val::from_f64(f64::from(color)),
                Val::from_bool(true),
            ],
        )?;
    field.set_property(context, "defaultTextFormat", format)?;
    field.call_method(context, "setTextFormat", &[format])?;
    field.set_property(context, "x", Val::from_f64(x))?;
    field.set_property(context, "y", Val::from_f64(y))?;
    Ok(())
}

fn remove_overlay<'gc>(context: &mut UpdateContext<'gc>, id: &str) -> Result<(), String> {
    let field = OVERLAYS.with(|cell| cell.borrow_mut().remove(id));
    let Some(field) = field else {
        return Err("Overlay id is not owned by the bridge".into());
    };
    let field = field.fetch(context);
    let layer = OVERLAY_LAYER.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|handle| handle.fetch(context))
    });
    if let Some(layer) = layer {
        if field
            .get_property(context, "parent")
            .ok()
            .and_then(Val::as_object)
            .is_some_and(|parent| parent.raw() == layer.raw())
        {
            layer.call_method(context, "removeChild", &[Val::from_obj(field)])?;
        }
        let remaining = layer
            .get_property(context, "numChildren")
            .ok()
            .and_then(Val::as_i32)
            .unwrap_or(0);
        if remaining == 0 {
            if let Some(stage) = super::resolve_target(context, "stage") {
                let _ = stage.call_method(context, "removeChild", &[Val::from_obj(layer)]);
            }
            OVERLAY_LAYER.with(|cell| *cell.borrow_mut() = None);
        }
    }
    Ok(())
}

// --- Difficulty selection through the game's real FUI controls ------------
//
// The visible window's `DropDownBox` is located by class name, its actual
// `_dropDownTextKeys` are parsed for the offered difficulty values, the
// public `currentEntry` setter runs (dispatching SELECTED_ENTRY_CHANGED so
// the mediator re-evaluates the confirm button), and the real `button_go`
// FUI Button receives a bubbling click so the mediator sends its own
// request. No gate request object is constructed here.

fn select_gate_difficulty<'gc>(
    context: &mut UpdateContext<'gc>,
    gui_name: &str,
    difficulty: i32,
) -> Result<(), String> {
    if difficulty < 1 {
        return Err("Gate difficulty must be a positive integer".into());
    }
    // The command's gui value is an observed window id (possibly with a
    // child path) — the first segment names the window, same as Flash.
    let window_key = gui_name.split('/').next().unwrap_or_default();
    let manager = gui::gui_manager(context).ok_or("Gui manager unavailable")?;
    let window = gui::window_by_id(context, manager, window_key)
        .ok_or("Difficulty selection window is no longer open")?;
    if !gui::is_visible(context, window) {
        return Err("Difficulty selection window is no longer open".into());
    }
    // Every DropDownBox under the window is a candidate; a box offers
    // difficulties only through its leading `difficulty*` entries.
    let mut chosen: Option<(Obj<'gc>, usize)> = None;
    for (node, keys) in gui::dropdowns(context, window) {
        let entries = gui::difficulty_entries(&keys);
        if entries.is_empty() {
            continue;
        }
        if let Some(index) = entries.iter().position(|value| *value == difficulty) {
            chosen = Some((node, index));
            break;
        }
    }
    let Some((dropdown, index)) = chosen else {
        return Err("Requested difficulty is not offered by the observed selection UI".into());
    };
    dropdown.set_property(context, "currentEntry", Val::from_i32(index as i32))?;
    // Verify the setter stuck and the reported key is the requested
    // difficulty before touching the enter button.
    let selected = property_i32(context, dropdown, "currentEntry");
    let selected_key = dropdown
        .get_property(context, "currentEntryKey")
        .ok()
        .and_then(|value| value.to_string_lossy());
    if selected != Some(index as i32)
        || selected_key
            .as_deref()
            .and_then(gui::difficulty_value)
            != Some(difficulty)
    {
        return Err("Difficulty selection was rejected by the window".into());
    }
    let button = gui::named_descendants(context, window, "button_go")
        .into_iter()
        .next()
        .ok_or("Difficulty selection go button unavailable")?;
    let enabled = button.get_property(context, "enabled").ok();
    if enabled.and_then(Val::as_bool) == Some(false)
        || enabled.and_then(Val::as_f64) == Some(0.0)
        || gui::enabled_of(context, button) == Some(false)
    {
        return Err("Selected difficulty is not currently enterable".into());
    }
    gui::dispatch_click(context, button)
}

fn property_i32<'gc>(
    context: &mut UpdateContext<'gc>,
    obj: Obj<'gc>,
    name: &str,
) -> Option<i32> {
    obj.get_property(context, name).ok().and_then(Val::as_i32)
}
