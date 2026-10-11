//! Reflection bridge: external control of the running movie, the Ruffle
//! equivalent of `windows_flash_mvp/activex_player`.
//!
//! `swf_bridge::inject` splices the `DobotBridge` class into the root movie.
//! It registers `pingBridge`/`invokeAS`/`inspectAS` through
//! `ExternalInterface.addCallback`. The `GameExternalInterface` provider makes
//! `ExternalInterface.available` true, answers the game's outbound calls
//! (`window.location.href`, `eval`), handles its `bpCloseWindow` request, and
//! logs other calls instead of failing with error 2067.
//!
//! The control pipe is `\\.\pipe\xenomit_bridge_<PID>` (Windows only), one
//! command per connection, matching `dobot_activex_<PID>` in the ActiveX
//! player so `control.py` works the same way:
//!
//! | Command | Bridge call |
//! | --- | --- |
//! | `BRIDGE` | `pingBridge` |
//! | `INSPECT` + tab + target | `inspectAS` |
//! | `STATE` (+ tab + target) | Visible display-list snapshot |
//! | `INVOKE` + tab + target + tab + method + tab + JSON array | `invokeAS` |
//! | `FINVOKE` + same as INVOKE | `invokeAS`, run inside a frame |
//! | `STATUS` / `PROBE` | Player/bridge state, no AS3 call |
//!
//! `INVOKE` runs the callback between frames (what Flash's `CallFunction`
//! did). `FINVOKE` runs it inside `post_frame_callbacks`, i.e. inside a real
//! frame — the equivalent of the ActiveX enter-frame hook used by `NUDGE`,
//! for calls like `moveHeroToCordinates` that need frame context.
//!
//! Replies are `OK ` + the ActionScript JSON string, or `ERR ` + a host
//! error, exactly like `call_on_ui` in the ActiveX player. A reply with
//! `"status":"error"` inside `OK` means the bridge ran and the AS3 call
//! failed.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ruffle_core::Player;
use ruffle_core::PostFrameCallback;
use ruffle_core::context::UpdateContext;
use ruffle_core::external::{ExternalInterfaceProvider, Value as ExternalValue};
use ruffle_core::focus_tracker::TDisplayObject;

use crate::state_reader::{CapturedFrame, StateReader};
use crate::state_shm::LiveStatePublisher;

/// Generous bound on how long a pipe call may wait for the frame pump. Long
/// startup scripts (the smoke pool init) hold the player for tens of seconds.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// Shared state between the player loop and the control pipe thread.
struct Shared {
    queue: Mutex<VecDeque<Request>>,
    injected: AtomicBool,
    ready: AtomicBool,
    player_up: AtomicBool,
    close_requested: AtomicBool,
    calls: AtomicU64,
    errors: AtomicU64,
    /// Cumulative native state-capture time (microseconds).
    capture_us_total: AtomicU64,
    capture_us_max: AtomicU64,
    /// Number of published capture frames.
    capture_frames: AtomicU64,
}

impl Shared {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            injected: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            player_up: AtomicBool::new(false),
            close_requested: AtomicBool::new(false),
            calls: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            capture_us_total: AtomicU64::new(0),
            capture_us_max: AtomicU64::new(0),
            capture_frames: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Copy)]
enum RequestKind {
    /// An `ExternalInterface` callback registered by `DobotBridge`.
    Callback(&'static str),
    /// `NINVOKE`/`NFINVOKE`: resolve and call natively, no AS3 bridge.
    Native,
}

struct Request {
    kind: RequestKind,
    args: Vec<String>,
    /// Run inside `post_frame_callbacks` instead of a plain `Player::update`.
    in_frame: bool,
    reply: Sender<String>,
}

/// Per-frame state capture plus its shared-memory publisher.
struct NativeState {
    reader: StateReader,
    publisher: LiveStatePublisher,
    frame: CapturedFrame,
    last_capture: Instant,
}

pub struct Bridge {
    shared: Arc<Shared>,
    enabled: bool,
    pipe_started: AtomicBool,
    /// Lazily initialized on the first `poll` after the player exists.
    state: Mutex<Option<NativeState>>,
}

impl Bridge {
    pub fn new(enabled: bool) -> Self {
        Self {
            shared: Arc::new(Shared::new()),
            enabled,
            pipe_started: AtomicBool::new(false),
            state: Mutex::new(None),
        }
    }

    /// Record whether `swf_bridge::inject` patched the root movie.
    pub fn mark_injected(&self, injected: bool) {
        self.shared.injected.store(injected, Ordering::Relaxed);
    }

    /// Whether the game has requested that the desktop client close.
    pub fn close_requested(&self) -> bool {
        self.shared.close_requested.load(Ordering::Relaxed)
    }

    /// The `ExternalInterface` provider. Install it even with the bridge
    /// disabled so `ExternalInterface.available` matches a real browser.
    pub fn provider(&self, page_url: Option<String>) -> GameExternalInterface {
        GameExternalInterface {
            page_url,
            shared: Arc::clone(&self.shared),
        }
    }

    /// Start the `\\.\pipe\xenomit_bridge_<PID>` server. Idempotent.
    pub fn spawn_pipe(&self) {
        if !self.enabled || self.pipe_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let pipe_name = format!(r"\\.\pipe\xenomit_bridge_{}", std::process::id());
        self.shared.player_up.store(true, Ordering::Relaxed);
        if pipe::spawn(Arc::clone(&self.shared), &pipe_name) {
            println!("Control pipe: {pipe_name}");
        }
    }

    /// Drain pending pipe commands. Called on the player thread every loop.
    /// Also captures one frame of game state and republishes it to shared
    /// memory, at most once per movie frame interval (capped at 60 Hz).
    pub fn poll(&self, player: &mut Player) {
        let pending: Vec<Request> = std::mem::take(&mut *self.shared.queue.lock().unwrap()).into();
        for request in pending {
            let Request {
                kind,
                args,
                in_frame,
                reply,
            } = request;
            if in_frame {
                player.update(move |context| {
                    let Some(data) = context.stage.root_clip() else {
                        let _ = reply.send("ERR movie has no root clip yet\n".to_owned());
                        return;
                    };
                    context.post_frame_callbacks.push(PostFrameCallback {
                        callback: Box::new(move |context, _| {
                            let _ = reply.send(run_request(context, kind, &args));
                        }),
                        data,
                    });
                });
            } else {
                player.update(move |context| {
                    let _ = reply.send(run_request(context, kind, &args));
                });
            }
        }
        if self.enabled {
            self.capture_state(player);
        }
    }

    /// Read the game map natively and republish it to shared memory. Runs on
    /// the player thread via `mutate_with_update_context` — unlike
    /// `Player::update` this executes no action queue, timers, or GC.
    fn capture_state(&self, player: &mut Player) {
        let mut guard = self.state.lock().unwrap();
        let state = guard.get_or_insert_with(|| NativeState {
            reader: StateReader::new(),
            publisher: LiveStatePublisher::new(),
            frame: CapturedFrame::default(),
            last_capture: Instant::now() - Duration::from_secs(1),
        });
        // One capture per movie frame, and never faster than 60 Hz.
        let frame_rate = player.frame_rate().max(1.0);
        let interval = Duration::from_secs_f64((1.0 / frame_rate).max(1.0 / 60.0));
        if state.last_capture.elapsed() < interval {
            return;
        }
        state.last_capture = Instant::now();
        player.mutate_with_update_context(|context| {
            state.reader.capture(context, &mut state.frame)
        });
        state.publisher.publish(
            &state.frame.header,
            &state.frame.entities,
            &state.frame.strings,
        );
        let capture_us = u64::from(state.frame.header.capture_us);
        self.shared.capture_us_total.fetch_add(capture_us, Ordering::Relaxed);
        self.shared.capture_us_max.fetch_max(capture_us, Ordering::Relaxed);
        self.shared.capture_frames.fetch_add(1, Ordering::Relaxed);
    }
}

/// Run a queued pipe request inside an update context.
fn run_request(context: &mut UpdateContext<'_>, kind: RequestKind, args: &[String]) -> String {
    match kind {
        RequestKind::Callback(name) => invoke(context, name, args),
        RequestKind::Native => native_invoke(context, args),
    }
}

/// Invoke one registered `ExternalInterface` callback inside an update
/// context. This is Ruffle's `CallFunction` equivalent.
fn invoke(context: &mut UpdateContext<'_>, name: &'static str, args: &[String]) -> String {
    // `xenomit` is a host-implemented synthetic target (typed game commands,
    // supplementary feature/event reads), not an AS3 path — serve it natively
    // on the legacy INVOKE/FINVOKE verbs exactly like NINVOKE/NFINVOKE.
    if name == "invokeAS" && args.first().is_some_and(|t| t == "xenomit") {
        let (Some(target), Some(method), Some(json_args)) =
            (args.first(), args.get(1), args.get(2))
        else {
            return "ERR usage: INVOKE target method json\n".to_owned();
        };
        let payload = match native_call(context, target, method, json_args) {
            Ok(payload) => payload,
            Err(message) => {
                serde_json::json!({"status": "error", "message": message}).to_string()
            }
        };
        return format!("OK {payload}\n");
    }
    let Some(callback) = context.external_interface.get_callback(name) else {
        return format!("ERR bridge callback '{name}' is not registered\n");
    };
    let value = callback.call(context, name, args.iter().cloned().map(ExternalValue::String));
    format_reply(&value)
}

fn format_reply(value: &ExternalValue) -> String {
    match value {
        ExternalValue::String(json) => format!("OK {json}\n"),
        ExternalValue::Null | ExternalValue::Undefined => {
            "ERR bridge callback returned no result\n".to_owned()
        }
        other => format!("OK {}\n", to_json(other)),
    }
}

fn to_json(value: &ExternalValue) -> String {
    match value {
        ExternalValue::Undefined => "null".to_owned(),
        ExternalValue::Null => "null".to_owned(),
        ExternalValue::Bool(b) => b.to_string(),
        ExternalValue::Number(n) => n.to_string(),
        ExternalValue::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        ExternalValue::List(items) => {
            format!(
                "[{}]",
                items.iter().map(to_json).collect::<Vec<_>>().join(",")
            )
        }
        ExternalValue::Object(map) => {
            let pairs = map
                .iter()
                .map(|(key, value)| {
                    format!(
                        "\"{}\":{}",
                        key.replace('\\', "\\\\").replace('"', "\\\""),
                        to_json(value)
                    )
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", pairs.join(","))
        }
    }
}

// --- Native invoke path (NINVOKE / NFINVOKE) ---------------------------
//
// Same `target\tmethod\tjson` grammar as INVOKE, but the target is resolved
// and the method is invoked natively through `ruffle_core::introspect`, so
// no AS3 code runs and `DobotBridge` is not required.

use ruffle_core::introspect::{self, Obj, Val};
#[path = "native_actions.rs"]
mod native_actions;
#[path = "client_features.rs"]
mod client_features;

/// Execute `NINVOKE`/`NFINVOKE` arguments `[target, method, jsonArray]`.
fn native_invoke(context: &mut UpdateContext<'_>, args: &[String]) -> String {
    let (Some(target), Some(method), Some(json_args)) =
        (args.first(), args.get(1), args.get(2))
    else {
        return "ERR usage: NINVOKE target method json\n".to_owned();
    };
    let payload = match native_call(context, target, method, json_args) {
        Ok(payload) => payload,
        Err(message) => {
            serde_json::json!({"status": "error", "message": message}).to_string()
        }
    };
    format!("OK {payload}\n")
}

fn native_call(
    context: &mut UpdateContext<'_>,
    target: &str,
    method: &str,
    json_args: &str,
) -> Result<String, String> {
    let args: Vec<serde_json::Value> = if json_args.is_empty() {
        Vec::new()
    } else {
        match serde_json::from_str(json_args) {
            Ok(serde_json::Value::Array(args)) => args,
            Ok(_) => return Err("arguments must be a JSON array".to_owned()),
            Err(error) => return Err(format!("bad arguments: {error}")),
        }
    };
    if target == "xenomit" && method == "gameCommand" {
        if args.len() != 1 { return Err("gameCommand requires one typed command".into()); }
        let command = serde_json::from_value(args[0].clone()).map_err(|e| format!("invalid game command: {e}"))?;
        native_actions::execute(context, command)?;
        return Ok(serde_json::json!({"status": "ok", "data": null}).to_string());
    }
    if target == "xenomit" && method == "clientFeatures" {
        if !args.is_empty() { return Err("clientFeatures takes no arguments".into()); }
        let state = client_features::feature_state(context);
        let data = serde_json::to_value(state).map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({"status": "ok", "data": data}).to_string());
    }
    if target == "xenomit" && method == "clientEvents" {
        if args.len() != 1 { return Err("clientEvents requires one cursor".into()); }
        const MAX_CURSOR: u64 = 9_007_199_254_740_991;
        let cursor = args[0].as_u64().or_else(|| {
            args[0].as_f64().filter(|n| {
                n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n <= MAX_CURSOR as f64
            }).map(|n| n as u64)
        });
        let Some(cursor) = cursor else {
            return Err("event cursor exceeds the bridge integer range".into());
        };
        let batch = crate::client_events::poll(cursor)?;
        return Ok(serde_json::json!({"status": "ok", "data": batch}).to_string());
    }
    let target = resolve_target(context, target)
        .ok_or_else(|| "Method or target not found".to_owned())?;
    let args: Vec<Val<'_>> = args
        .iter()
        .map(|value| json_to_val(context, value))
        .collect();

    // A trailing `()` forces a zero-arg call (same grammar as path steps);
    // otherwise call the property when callable and return its value when not.
    let (name, forced) = match method.strip_suffix("()") {
        Some(name) => (name, true),
        None => (method, false),
    };
    let property = target.get_property(context, name);
    let result = if forced {
        target.call_method(context, name, &args)
    } else {
        match property {
            Ok(value) if !value.is_function() => Ok(value),
            _ => target.call_method(context, name, &args),
        }
    };
    match result {
        Ok(value) => Ok(native_result_json(context, value)),
        Err(message) => Err(message),
    }
}

/// Longest dotted prefix resolving to a class, then property/`()` steps —
/// the same grammar `DobotBridge.resolvePath` uses, minus the display-list
/// walk. `root`/`stage` resolve to the stage/root clip.
fn resolve_target<'gc>(
    context: &mut UpdateContext<'gc>,
    path: &str,
) -> Option<Obj<'gc>> {
    if path.is_empty() || path == "root" {
        return context
            .stage
            .root_clip()
            .and_then(|clip| clip.object2())
            .map(|object| Obj::from_raw(object.into()));
    }
    if path == "stage" {
        return context
            .stage
            .object2()
            .map(|object| Obj::from_raw(object.into()));
    }
    let parts: Vec<&str> = path.split('.').collect();
    for len in (1..=parts.len()).rev() {
        let class_name = parts[..len].join(".");
        let Some(mut current) = introspect::find_class(context, &class_name) else {
            continue;
        };
        let mut resolved = true;
        for part in &parts[len..] {
            match path_step(context, current, part) {
                Some(next) => current = next,
                None => {
                    resolved = false;
                    break;
                }
            }
        }
        if resolved {
            return Some(current);
        }
    }
    None
}

/// One path segment: `name()` is a zero-arg call, `name` a property read
/// (public getter first, private slot as fallback).
fn path_step<'gc>(
    context: &mut UpdateContext<'gc>,
    current: Obj<'gc>,
    part: &str,
) -> Option<Obj<'gc>> {
    let value = if let Some(method) = part.strip_suffix("()") {
        current.call_method(context, method, &[]).ok()?
    } else {
        match current.get_property(context, part) {
            Ok(value) => value,
            Err(_) => current.slot_by_name(part)?,
        }
    };
    value.as_object()
}

/// `{"$xenomit_entity_id": N}` arguments resolve to the live entity through
/// `ScreenManager._instance._map._entitiesById`, or a linear `_entities`
/// scan; a missing entity resolves to null.
fn entity_by_id<'gc>(context: &mut UpdateContext<'gc>, id: i64) -> Val<'gc> {
    let map = introspect::find_class(context, "net.bigpoint.darkorbit.managers.ScreenManager")
        .and_then(|class| class.slot_by_name("_instance"))
        .and_then(Val::as_object)
        .and_then(|instance| instance.slot_by_name("_map"))
        .and_then(Val::as_object);
    let Some(map) = map else {
        return Val::NULL;
    };
    if let Some(by_id) = map.slot_by_name("_entitiesById").and_then(Val::as_object)
        && let Some(value) = by_id.dynamic_get(context, &id.to_string())
        && !value.is_null()
    {
        return value;
    }
    if let Some(entities) = map
        .slot_by_name("_entities")
        .and_then(Val::as_object)
        .and_then(Obj::vector_values)
    {
        for value in entities {
            let Some(entity) = value.as_object() else {
                continue;
            };
            if entity.slot_by_name("_id").and_then(Val::as_i32) == Some(id as i32) {
                return value;
            }
        }
    }
    Val::NULL
}

/// JSON arguments to AVM2 values, matching `DobotBridge.coerce`: integral
/// numbers in `i32` range become `int`, other numbers `Number`, strings,
/// booleans, null, arrays, and objects (with the entity-id shorthand).
fn json_to_val<'gc>(context: &mut UpdateContext<'gc>, value: &serde_json::Value) -> Val<'gc> {
    match value {
        serde_json::Value::Null => Val::NULL,
        serde_json::Value::Bool(flag) => Val::from_bool(*flag),
        serde_json::Value::Number(number) => {
            if let Some(int) = number
                .as_i64()
                .and_then(|int| i32::try_from(int).ok())
            {
                Val::from_i32(int)
            } else {
                Val::from_f64(number.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(text) => Val::from_str(context, text),
        serde_json::Value::Array(items) => {
            let values: Vec<Val<'gc>> =
                items.iter().map(|item| json_to_val(context, item)).collect();
            Val::from_obj(introspect::new_array(context, &values))
        }
        serde_json::Value::Object(map) => {
            if let Some(id) = map
                .get("$xenomit_entity_id")
                .and_then(serde_json::Value::as_i64)
            {
                return entity_by_id(context, id);
            }
            let object = introspect::new_object(context);
            for (key, value) in map {
                let converted = json_to_val(context, value);
                object.dynamic_set(context, key, converted);
            }
            Val::from_obj(object)
        }
    }
}

/// AVM2 result to the bridge JSON envelope; objects degrade to `data: null`
/// with a `note` carrying the class name.
fn native_result_json<'gc>(context: &mut UpdateContext<'gc>, value: Val<'gc>) -> String {
    if value.is_null() {
        return serde_json::json!({"status": "ok", "data": null}).to_string();
    }
    if let Some(flag) = value.as_bool() {
        return serde_json::json!({"status": "ok", "data": flag}).to_string();
    }
    if value.is_integer() {
        let number = value.as_i32().unwrap_or(0);
        return serde_json::json!({"status": "ok", "data": number}).to_string();
    }
    if let Some(number) = value.as_f64() {
        let data = if number.is_finite() {
            serde_json::json!(number)
        } else {
            serde_json::Value::Null
        };
        return serde_json::json!({"status": "ok", "data": data}).to_string();
    }
    if let Some(text) = value.to_string_lossy() {
        return serde_json::json!({"status": "ok", "data": text}).to_string();
    }
    if let Some(object) = value.as_object() {
        let name = object.qualified_name(context);
        return serde_json::json!({"status": "ok", "data": null, "note": name}).to_string();
    }
    serde_json::json!({"status": "ok", "data": null}).to_string()
}

fn dispatch(shared: &Arc<Shared>, command: &str) -> String {
    if command == "BRIDGE" {
        return enqueue(shared, RequestKind::Callback("pingBridge"), Vec::new(), false);
    }
    if command == "STATUS" || command == "PROBE" {
        let frames = shared.capture_frames.load(Ordering::Relaxed);
        let total_us = shared.capture_us_total.load(Ordering::Relaxed);
        return format!(
            "OK bridge={} ready={} player={} pending={} calls={} errors={} frames={} capture_us_avg={} capture_us_max={}\n",
            shared.injected.load(Ordering::Relaxed) as u8,
            shared.ready.load(Ordering::Relaxed) as u8,
            shared.player_up.load(Ordering::Relaxed) as u8,
            shared.queue.lock().unwrap().len(),
            shared.calls.load(Ordering::Relaxed),
            shared.errors.load(Ordering::Relaxed),
            frames,
            if frames > 0 { total_us / frames } else { 0 },
            shared.capture_us_max.load(Ordering::Relaxed),
        );
    }
    if let Some(target) = command.strip_prefix("INSPECT\t") {
        return enqueue(
            shared,
            RequestKind::Callback("inspectAS"),
            vec![target.to_owned()],
            false,
        );
    }
    if command == "STATE" {
        return enqueue(
            shared,
            RequestKind::Callback("stateAS"),
            vec!["stage".to_owned()],
            false,
        );
    }
    if let Some(target) = command.strip_prefix("STATE\t") {
        return enqueue(
            shared,
            RequestKind::Callback("stateAS"),
            vec![target.to_owned()],
            false,
        );
    }
    for (prefix, in_frame, kind) in [
        ("INVOKE\t", false, RequestKind::Callback("invokeAS")),
        ("FINVOKE\t", true, RequestKind::Callback("invokeAS")),
        ("NINVOKE\t", false, RequestKind::Native),
        ("NFINVOKE\t", true, RequestKind::Native),
    ] {
        if let Some(body) = command.strip_prefix(prefix) {
            let mut parts = body.splitn(3, '\t');
            let (Some(target), Some(method), Some(json)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return format!("ERR usage: {} target method json\n", prefix.trim_end());
            };
            return enqueue(
                shared,
                kind,
                vec![target.to_owned(), method.to_owned(), json.to_owned()],
                in_frame,
            );
        }
    }
    if command.starts_with("NUDGE") {
        return "ERR NUDGE needs the native hook; use FINVOKE to run moveHeroToCordinates in-frame\n"
            .to_owned();
    }
    "ERR unknown command\n".to_owned()
}

fn enqueue(shared: &Arc<Shared>, kind: RequestKind, args: Vec<String>, in_frame: bool) -> String {
    if !shared.player_up.load(Ordering::Relaxed) {
        return "ERR player is not running\n".to_owned();
    }
    shared.calls.fetch_add(1, Ordering::Relaxed);
    let (reply, wait) = mpsc::channel();
    shared.queue.lock().unwrap().push_back(Request {
        kind,
        args,
        in_frame,
        reply,
    });
    match wait.recv_timeout(REPLY_TIMEOUT) {
        Ok(reply) => reply,
        Err(_) => {
            shared.errors.fetch_add(1, Ordering::Relaxed);
            "ERR timed out waiting for the player\n".to_owned()
        }
    }
}

/// `ExternalInterface.call` handler for the game's outbound calls. Mirrors
/// `DesktopExternalInterfaceProvider`: spoof `location.href` reads and log
/// the rest instead of failing with error 2067.
pub struct GameExternalInterface {
    page_url: Option<String>,
    shared: Arc<Shared>,
}

fn is_location_href(code: &str) -> bool {
    matches!(
        code,
        "document.location.href" | "window.location.href" | "top.location.href"
    )
}

impl ExternalInterfaceProvider for GameExternalInterface {
    fn call_method(
        &self,
        _context: &mut UpdateContext<'_>,
        name: &str,
        args: &[ExternalValue],
    ) -> ExternalValue {
        if name == "bpCloseWindow" {
            eprintln!("[bridge] game requested client shutdown via bpCloseWindow");
            self.shared.close_requested.store(true, Ordering::Relaxed);
            return ExternalValue::Undefined;
        }
        if let Some(page_url) = &self.page_url {
            if let Some(location) = name.strip_suffix(".toString")
                && is_location_href(location)
            {
                return page_url.clone().into();
            }
            if is_location_href(name) {
                return page_url.clone().into();
            }
            if name == "eval"
                && let [ExternalValue::String(code)] = args
                && is_location_href(code)
            {
                return page_url.clone().into();
            }
        }
        eprintln!(
            "[bridge] unhandled ExternalInterface.call {name} ({} args)",
            args.len()
        );
        ExternalValue::Undefined
    }

    fn on_callback_available(&self, name: &str) {
        eprintln!("[bridge] ActionScript registered callback '{name}'");
        if name == "pingBridge" {
            self.shared.ready.store(true, Ordering::Relaxed);
        }
    }

    fn get_id(&self) -> Option<String> {
        None
    }
}

#[cfg(windows)]
mod pipe {
    use super::{Shared, dispatch};
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::{
        ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FlushFileBuffers, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT,
    };

    pub fn spawn(shared: Arc<Shared>, name: &str) -> bool {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        std::thread::Builder::new()
            .name("xenomit-bridge".to_owned())
            .spawn(move || serve(&shared, &wide))
            .is_ok()
    }

    fn serve(shared: &Arc<Shared>, name: &[u16]) -> ! {
        // One persistent instance for the process lifetime. Recreating the
        // instance per command used to race clients that still held the
        // previous connection handle: the zombie counted against
        // nMaxInstances=1, CreateNamedPipeW failed, and the retry sleep left
        // the pipe name unbound for seconds — every client open failed with
        // ERROR_FILE_NOT_FOUND in that window.
        let pipe = loop {
            // SAFETY: the handle lives until the process exits.
            let pipe = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_MESSAGE
                        | PIPE_READMODE_MESSAGE
                        | PIPE_WAIT
                        | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    1 << 20,
                    1 << 20,
                    0,
                    std::ptr::null(),
                )
            };
            if pipe != INVALID_HANDLE_VALUE {
                break pipe;
            }
            let error = unsafe { GetLastError() };
            eprintln!("[bridge] CreateNamedPipeW failed: {error}");
            std::thread::sleep(std::time::Duration::from_secs(5));
        };
        let mut input = vec![0u8; 65536];
        loop {
            // SAFETY: blocking connect on a valid pipe handle.
            let connected = unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if connected {
                let mut read = 0u32;
                let ok = unsafe {
                    ReadFile(
                        pipe,
                        input.as_mut_ptr().cast(),
                        input.len() as u32 - 1,
                        &mut read,
                        std::ptr::null_mut(),
                    )
                };
                if ok != 0 && read > 0 {
                    let mut command =
                        String::from_utf8_lossy(&input[..read as usize]).into_owned();
                    while command.ends_with('\n') || command.ends_with('\r') {
                        command.pop();
                    }
                    let reply = dispatch(shared, &command);
                    let mut written = 0u32;
                    unsafe {
                        WriteFile(
                            pipe,
                            reply.as_ptr().cast(),
                            reply.len() as u32,
                            &mut written,
                            std::ptr::null_mut(),
                        );
                        // Wait until the client has read the reply before
                        // disconnecting, or DisconnectNamedPipe drops it.
                        FlushFileBuffers(pipe);
                    }
                }
            }
            // Re-arm the SAME instance for the next client — the pipe name
            // never goes away, so opens only ever see PIPE_BUSY while a
            // request is connected, never ERROR_FILE_NOT_FOUND.
            unsafe { DisconnectNamedPipe(pipe) };
        }
    }
}

#[cfg(not(windows))]
mod pipe {
    use super::Shared;
    use std::sync::Arc;

    pub fn spawn(_shared: Arc<Shared>, _name: &str) -> bool {
        eprintln!("[bridge] the control pipe is only available on Windows");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruffle_core::tag_utils::SwfMovie;
    use ruffle_core::{FloatDuration, PlayerBuilder};
    use ruffle_render::backend::ViewportDimensions;
    use ruffle_render::backend::null::NullRenderer;

    /// Minimal one-frame AVM2 host movie for `DobotBridge` injection.
    fn minimal_host() -> Vec<u8> {
        let mut swf = b"FWS\x2b\x00\x00\x00\x00".to_vec();
        swf.push(0x78); // RECT nbits=15 -> 9 bytes
        swf.extend_from_slice(&[0; 8]);
        swf.extend_from_slice(&[0x00, 0x18]);
        swf.extend_from_slice(&1u16.to_le_bytes());
        for (code, payload) in [
            (69u16, &[0x08, 0x00, 0x00, 0x00][..]), // FileAttributes (AS3)
            (9, &[0x00, 0x00, 0x00][..]),           // SetBackgroundColor
            (1, &[][..]),                           // ShowFrame
            (0, &[][..]),                           // End
        ] {
            if payload.len() < 0x3f {
                swf.extend_from_slice(&((code << 6) | payload.len() as u16).to_le_bytes());
            } else {
                swf.extend_from_slice(&((code << 6) | 0x3f).to_le_bytes());
                swf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            }
            swf.extend_from_slice(payload);
        }
        let len = swf.len() as u32;
        swf[4..8].copy_from_slice(&len.to_le_bytes());
        swf
    }

    /// End-to-end: inject DobotBridge into a host movie, run it under Ruffle,
    /// then drive the ExternalInterface callbacks like the pipe does.
    #[test]
    fn dobot_bridge_answers_under_ruffle() {
        let swf = crate::swf_bridge::inject(&minimal_host()).expect("inject");
        let movie = SwfMovie::from_data(&swf, "file:///bridge_host.swf".into(), None, None)
            .expect("injected host must parse");
        let bridge = Bridge::new(true);
        let provider = bridge.provider(Some("https://example.com/map".into()));
        let player = PlayerBuilder::new()
            .with_renderer(NullRenderer::new(ViewportDimensions {
                width: 100,
                height: 100,
                scale_factor: 1.0,
            }))
            .with_external_interface(Box::new(provider))
            .with_autoplay(true)
            .with_movie(movie)
            .build();

        for _ in 0..40 {
            player
                .lock()
                .unwrap()
                .tick(FloatDuration::from_millis(33.0));
            if bridge.shared.ready.load(Ordering::Relaxed) {
                break;
            }
        }
        assert!(
            bridge.shared.ready.load(Ordering::Relaxed),
            "DobotBridge never registered pingBridge"
        );

        let result = player
            .lock()
            .unwrap()
            .call_internal_interface("pingBridge", []);
        assert_eq!(result, ExternalValue::String("dobot-bridge".to_owned()));

        // invokeAS -> resolvePath("root") -> toString() exercises the full
        // reflection path: display walk, Function.apply, JSON.stringify.
        let result = player.lock().unwrap().call_internal_interface(
            "invokeAS",
            [
                ExternalValue::String("root".into()),
                ExternalValue::String("toString".into()),
                ExternalValue::String("[]".into()),
            ],
        );
        let ExternalValue::String(json) = result else {
            panic!("invokeAS returned {result:?}");
        };
        assert!(json.contains("\"status\":\"ok\""), "{json}");
        assert!(json.contains("[object"), "{json}");

        let result = player
            .lock()
            .unwrap()
            .call_internal_interface("inspectAS", [ExternalValue::String("root".into())]);
        let ExternalValue::String(json) = result else {
            panic!("inspectAS returned {result:?}");
        };
        assert!(json.contains("\"status\":\"ok\""), "{json}");
        assert!(json.contains("methods"), "{json}");

        let result = player
            .lock()
            .unwrap()
            .call_internal_interface("stateAS", [ExternalValue::String("stage".into())]);
        let ExternalValue::String(json) = result else {
            panic!("stateAS returned {result:?}");
        };
        assert!(json.contains("\"status\":\"ok\""), "{json}");
        assert!(json.contains("\"entities\""), "{json}");
    }

    /// `DobotBridge` injected into a minimal host movie, ticked until the
    /// bridge registered `pingBridge`.
    fn bridge_host() -> (Bridge, std::sync::Arc<Mutex<Player>>) {
        let swf = crate::swf_bridge::inject(&minimal_host()).expect("inject");
        let movie = SwfMovie::from_data(&swf, "file:///bridge_host.swf".into(), None, None)
            .expect("injected host must parse");
        let bridge = Bridge::new(true);
        let provider = bridge.provider(Some("https://example.com/map".into()));
        let player = PlayerBuilder::new()
            .with_renderer(NullRenderer::new(ViewportDimensions {
                width: 100,
                height: 100,
                scale_factor: 1.0,
            }))
            .with_external_interface(Box::new(provider))
            .with_autoplay(true)
            .with_movie(movie)
            .build();
        for _ in 0..40 {
            player
                .lock()
                .unwrap()
                .tick(FloatDuration::from_millis(33.0));
            if bridge.shared.ready.load(Ordering::Relaxed) {
                break;
            }
        }
        assert!(
            bridge.shared.ready.load(Ordering::Relaxed),
            "DobotBridge never registered pingBridge"
        );
        (bridge, player)
    }

    /// Native introspection: resolve the injected class object, read its
    /// private static `_ready` slot, and run a `NINVOKE` call — none of it
    /// touches `ExternalInterface` callbacks or the AS3 bridge code.
    #[test]
    fn native_introspection_and_ninvoke() {
        let (_bridge, player) = bridge_host();
        let mut player = player.lock().unwrap();
        player.mutate_with_update_context(|context| {
            let class = introspect::find_class(context, "DobotBridge")
                .expect("DobotBridge class object");
            // `private static var _ready` sits in the class traits under a
            // private namespace; slot_index ignores the namespace.
            let index = class.slot_index("_ready").expect("_ready slot");
            assert_eq!(class.slot(index).as_bool(), Some(true));

            // NINVOKE `DobotBridge` `inspect` `["root"]` — a static call that
            // returns a JSON string.
            let reply = native_invoke(
                context,
                &[
                    "DobotBridge".to_owned(),
                    "inspect".to_owned(),
                    "[\"root\"]".to_owned(),
                ],
            );
            assert!(reply.starts_with("OK "), "{reply}");
            let json: serde_json::Value =
                serde_json::from_str(reply[3..].trim()).unwrap();
            assert_eq!(json["status"], "ok", "{reply}");
        });
    }
    #[test]
    fn native_typed_commands_reject_invalid_and_missing_game_state() {
        let (_, player) = bridge_host();
        player.lock().unwrap().mutate_with_update_context(|context| {
            for args in ["[]", "[{\"command\":\"unknown\"}]", "[{\"command\":\"select_configuration\",\"configuration\":3}]", "[{\"command\":\"jump_portal\",\"portal_id\":12}]"] {
                assert!(native_call(context,"xenomit","gameCommand",args).is_err(),"{args}");
            }
            // Request bindings depend on real AVM2 constructors and property
            // coercion. Exercise those primitives in the smoke host.
            let class = introspect::find_class(context,"Object").unwrap();
            let instance = class.construct(context,&[]).unwrap().as_object().unwrap();
            instance.set_property(context,"number",Val::from_i32(12)).unwrap();
            assert_eq!(instance.get_property(context,"number").unwrap().as_i32(),Some(12));
        });
    }

    /// `dispatch` on a worker thread + `poll` on the player thread, exactly
    /// like the named-pipe server drives queued requests.
    fn dispatch_through_poll(
        bridge: &Bridge,
        player: &std::sync::Arc<Mutex<Player>>,
        command: &str,
    ) -> String {
        bridge.shared.player_up.store(true, Ordering::Relaxed);
        let shared = Arc::clone(&bridge.shared);
        let command = command.to_owned();
        let worker = std::thread::spawn(move || dispatch(&shared, &command));
        for _ in 0..50 {
            {
                let mut player = player.lock().unwrap();
                player.tick(FloatDuration::from_millis(33.0));
                bridge.poll(&mut player);
            }
            if worker.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        worker.join().expect("dispatch thread")
    }

    /// Full pipe-shape NINVOKE: `dispatch` enqueues, `poll` runs it on the
    /// player thread. Also exercises the per-frame capture + publish path.
    #[test]
    fn ninvoke_dispatches_through_poll() {
        let (bridge, player) = bridge_host();
        let reply =
            dispatch_through_poll(&bridge, &player, "NINVOKE\tDobotBridge\tinspect\t[\"root\"]");
        assert!(reply.starts_with("OK "), "{reply}");
        assert!(reply.contains("\"status\":\"ok\""), "{reply}");
    }

    /// Legacy pipe commands keep working alongside the native ones: `STATE`
    /// reaches `stateAS`, and `INVOKE`/`FINVOKE` reach `invokeAS` (the latter
    /// inside `post_frame_callbacks`). All go through `dispatch` + `poll`.
    #[test]
    fn legacy_commands_dispatch_through_poll() {
        let (bridge, player) = bridge_host();

        let reply = dispatch_through_poll(&bridge, &player, "STATE\tstage");
        assert!(reply.starts_with("OK "), "{reply}");
        assert!(reply.contains("\"status\":\"ok\""), "{reply}");
        assert!(reply.contains("\"entities\""), "{reply}");

        let reply = dispatch_through_poll(&bridge, &player, "INVOKE\troot\ttoString\t[]");
        assert!(reply.starts_with("OK "), "{reply}");
        assert!(reply.contains("\"status\":\"ok\""), "{reply}");

        let reply = dispatch_through_poll(&bridge, &player, "FINVOKE\troot\ttoString\t[]");
        assert!(reply.starts_with("OK "), "{reply}");
        assert!(reply.contains("\"status\":\"ok\""), "{reply}");
    }

    /// The capture must never panic on a movie with no game model: the
    /// `ScreenManager` class is absent, so the frame is `map_id=-1` empty.
    /// On Windows the published mapping is read back through the public API.
    #[test]
    fn state_reader_empty_when_no_screen_manager() {
        let movie =
            SwfMovie::from_data(&minimal_host(), "file:///plain.swf".into(), None, None)
                .expect("host must parse");
        let player = PlayerBuilder::new()
            .with_renderer(NullRenderer::new(ViewportDimensions {
                width: 100,
                height: 100,
                scale_factor: 1.0,
            }))
            .with_autoplay(true)
            .with_movie(movie)
            .build();
        let mut reader = StateReader::new();
        let mut frame = CapturedFrame::default();
        player
            .lock()
            .unwrap()
            .mutate_with_update_context(|context| reader.capture(context, &mut frame));
        assert_eq!(frame.header.map_id, -1);
        assert!(frame.entities.is_empty());
    }

    /// Write a frame into the real `Local\xenomit_state_<pid>` mapping and
    /// read it back through `xenomit_game_api::live::LiveStateReader`.
    #[cfg(windows)]
    #[test]
    fn live_mapping_round_trips() {
        use xenomit_game_api::live::layout;
        use xenomit_game_api::live::layout::{LiveEntityRecord, LiveHeader};
        use xenomit_game_api::live::{LiveKind, LiveStateReader};

        let mut publisher = LiveStatePublisher::new();
        let header = LiveHeader {
            magic: layout::MAGIC,
            version: layout::VERSION,
            frame: 7,
            captured_unix_ms: 1_700_000_000_000,
            capture_us: 42,
            map_id: 4,
            map_width: 21000.0,
            map_height: 13100.0,
            hero_id: 9,
            hero_x: 10.0,
            hero_y: 20.0,
            hero_flags: layout::hero_flags::PRESENT,
            entity_count: 1,
            string_bytes: 4,
            ..LiveHeader::default()
        };
        let entity = LiveEntityRecord {
            id: 9,
            kind: layout::kind::HERO,
            flags: layout::entity_flags::HERO | layout::entity_flags::HAS_HP,
            x: 10.0,
            y: 20.0,
            hp: 5,
            max_hp: 6,
            name_off: 0,
            name_len: 4,
            ..LiveEntityRecord::default()
        };
        publisher.publish(&header, &[entity], b"hero");

        let reader = LiveStateReader::open(std::process::id()).expect("open live mapping");
        let frame = reader.read().expect("published frame");
        assert_eq!(frame.frame, 7);
        assert_eq!(frame.capture_us, 42);
        let map = frame.map.expect("map");
        assert_eq!(map.id, 4);
        let hero = frame.hero.expect("hero");
        assert_eq!(hero.id, 9);
        assert_eq!(frame.entities.len(), 1);
        assert_eq!(frame.entities[0].kind, LiveKind::Hero);
        assert_eq!(frame.entities[0].name, "hero");
        assert_eq!(frame.entities[0].hp, 5);
    }

    /// The control pipe is a real Windows named pipe; clients connect, send
    /// one message, and read one reply, like `control.py`.
    #[cfg(windows)]
    #[test]
    fn pipe_round_trips_commands() {
        use std::io::{Read, Write};
        let name = format!(r"\\.\pipe\xenomit_bridge_test_{}", std::process::id());
        assert!(super::pipe::spawn(Arc::new(Shared::new()), &name));

        // The server creates one instance at a time, so a connect can race the
        // gap between connections.
        let call = |command: &str| -> String {
            for _ in 0..50 {
                match std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&name)
                {
                    Ok(mut pipe) => {
                        pipe.write_all(command.as_bytes()).unwrap();
                        let mut reply = String::new();
                        pipe.read_to_string(&mut reply).ok();
                        return reply;
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
                }
            }
            panic!("could not connect to {name}");
        };

        assert!(
            call("STATUS").starts_with("OK bridge=0 ready=0 player=0"),
            "unexpected STATUS reply"
        );
        assert_eq!(call("BRIDGE"), "ERR player is not running\n");
        assert_eq!(call("INVOKE\ta.b\tm\t[]"), "ERR player is not running\n");
    }
}
