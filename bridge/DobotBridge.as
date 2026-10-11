package
{
    import flash.display.DisplayObject;
    import flash.display.DisplayObjectContainer;
    import flash.display.Loader;
    import flash.display.LoaderInfo;
    import flash.display.Sprite;
    import flash.geom.Point;
    import flash.geom.Rectangle;
    import flash.events.Event;
    import flash.events.TimerEvent;
    import flash.external.ExternalInterface;
    import flash.system.ApplicationDomain;
    import flash.system.Security;
    import flash.utils.Dictionary;
    import flash.utils.describeType;
    import flash.utils.getDefinitionByName;
    import flash.utils.getQualifiedClassName;
    import flash.utils.Timer;

    /**
     * Injected reflection bridge.
     *
     * The host splices this class into the movie and places one instance on the
     * first frame. It is not the document class. The instance keeps the stage,
     * which is how later calls reach classes loaded by the preloader.
     */
    public class DobotBridge extends Sprite
    {
        private static var _clip:DobotBridge;
        private static var _timer:Timer;
        private static var _stateFields:Object = {};
        private static var _ready:Boolean = false;
        private static var _booted:Boolean = boot();

        public function DobotBridge()
        {
            mouseEnabled = false;
            mouseChildren = false;
            tabEnabled = false;
            _clip = this;
            if (stage == null)
            {
                addEventListener(Event.ADDED_TO_STAGE, onAdded);
            }
        }

        private function onAdded(event:Event):void
        {
            removeEventListener(Event.ADDED_TO_STAGE, onAdded);
        }

        private static function boot():Boolean
        {
            tryRegister();
            try
            {
                _timer = new Timer(250, 120);
                _timer.addEventListener(TimerEvent.TIMER, onTimer);
                _timer.start();
            }
            catch (error:Error)
            {
            }
            return true;
        }

        private static function onTimer(event:TimerEvent):void
        {
            arm();
            tryRegister();
            if (_ready && _timer != null)
            {
                _timer.stop();
            }
        }

        private static function arm():void
        {
            var root:DisplayObject = displayRoot();
            if (root == null)
            {
                return;
            }
            if (!root.hasEventListener(Event.ENTER_FRAME))
            {
                root.addEventListener(Event.ENTER_FRAME, onFrame);
            }
            if (root.stage == null && !root.hasEventListener(Event.ADDED_TO_STAGE))
            {
                root.addEventListener(Event.ADDED_TO_STAGE, onFrame);
            }
        }

        private static function onFrame(event:Event):void
        {
            tryRegister();
        }

        private static function tryRegister():void
        {
            if (_ready || !ExternalInterface.available)
            {
                return;
            }
            try
            {
                Security.allowDomain("*");
            }
            catch (error:Error)
            {
            }
            try
            {
                Security.allowInsecureDomain("*");
            }
            catch (error:Error)
            {
            }
            ExternalInterface.addCallback("pingBridge", function():String
            {
                return "dobot-bridge";
            });
            ExternalInterface.addCallback("invokeAS", function(
                targetPath:String, method:String, jsonArgs:String):String
            {
                return invoke(targetPath, method, jsonArgs);
            });
            ExternalInterface.addCallback("inspectAS", function(targetPath:String):String
            {
                return inspect(targetPath);
            });
            ExternalInterface.addCallback("stateAS", function(targetPath:String):String
            {
                return state(targetPath);
            });
            _ready = true;
        }

        private static function displayRoot():DisplayObject
        {
            if (_clip != null)
            {
                if (_clip.root != null)
                {
                    return _clip.root;
                }
                return _clip;
            }
            try
            {
                var info:LoaderInfo = LoaderInfo.getLoaderInfoByDefinition(DobotBridge);
                return info != null ? info.content as DisplayObject : null;
            }
            catch (error:Error)
            {
                return null;
            }
            return null;
        }

        private static function collectDomains():Array
        {
            var domains:Array = [];
            var seen:Dictionary = new Dictionary(true);
            var root:DisplayObject = displayRoot();
            if (root != null)
            {
                if (root.stage != null)
                {
                    walk(root.stage, domains, seen, 0);
                }
                walk(root, domains, seen, 0);
            }
            return domains;
        }

        private static function walk(
            object:DisplayObject, domains:Array, seen:Dictionary, depth:int):void
        {
            if (object == null || depth > 48 || seen[object])
            {
                return;
            }
            seen[object] = true;
            // Loaded game content sits on the preloader as a normal child, not only
            // as a Loader. Its LoaderInfo carries the SWF's ApplicationDomain.
            try
            {
                var info:LoaderInfo = object.loaderInfo;
                if (info != null)
                {
                    var objectDomain:ApplicationDomain = info.applicationDomain;
                    if (objectDomain != null && domains.indexOf(objectDomain) < 0)
                    {
                        domains.push(objectDomain);
                    }
                }
            }
            catch (error:Error)
            {
            }
            if (object is Loader)
            {
                var loader:Loader = Loader(object);
                try
                {
                    var domain:ApplicationDomain = loader.contentLoaderInfo.applicationDomain;
                    if (domain != null && domains.indexOf(domain) < 0)
                    {
                        domains.push(domain);
                    }
                }
                catch (error:Error)
                {
                }
                try
                {
                    if (loader.content != null)
                    {
                        walk(loader.content, domains, seen, depth + 1);
                    }
                }
                catch (error:Error)
                {
                }
            }
            var container:DisplayObjectContainer = object as DisplayObjectContainer;
            if (container == null)
            {
                return;
            }
            var count:int = container.numChildren;
            for (var index:int = 0; index < count; index++)
            {
                try
                {
                    walk(container.getChildAt(index), domains, seen, depth + 1);
                }
                catch (error:Error)
                {
                }
            }
        }

        private static function findDefinition(name:String, domains:Array):*
        {
            try
            {
                return getDefinitionByName(name);
            }
            catch (error:Error)
            {
            }
            var index:int;
            for (index = 0; index < domains.length; index++)
            {
                var domain:ApplicationDomain = domains[index] as ApplicationDomain;
                try
                {
                    if (domain != null && domain.hasDefinition(name))
                    {
                        return domain.getDefinition(name);
                    }
                }
                catch (error:Error)
                {
                }
            }
            return null;
        }

        private static function step(current:Object, part:String):*
        {
            if (current == null)
            {
                return null;
            }
            var invokeNow:Boolean = false;
            var name:String = part;
            if (name.length >= 2 && name.substring(name.length - 2) == "()")
            {
                invokeNow = true;
                name = name.substring(0, name.length - 2);
            }
            var next:* = null;
            try
            {
                next = current[name];
            }
            catch (error:Error)
            {
                return null;
            }
            if (!invokeNow)
            {
                return next;
            }
            if (!(next is Function))
            {
                throw new Error("not a function: " + name);
            }
            return (next as Function).apply(current, []);
        }

        /**
         * Resolve a class, a static/property chain, or a display-list chain.
         * A segment ending in () is called with no arguments, which is how
         * singletons such as ScreenManager.getInstance() are reached.
         */
        private static function resolvePath(path:String):Object
        {
            if (path == null || path == "" || path == "root")
            {
                return displayRoot();
            }
            if (path == "stage")
            {
                if (_clip != null && _clip.stage != null)
                {
                    return _clip.stage;
                }
                var root:DisplayObject = displayRoot();
                return root != null ? root.stage : null;
            }

            var parts:Array = path.split(".");
            var domains:Array = collectDomains();
            var classLength:int;
            for (classLength = parts.length; classLength >= 1; classLength--)
            {
                var className:String = parts.slice(0, classLength).join(".");
                var defined:* = findDefinition(className, domains);
                if (defined == null)
                {
                    continue;
                }
                var current:Object = defined;
                var index:int;
                for (index = classLength; index < parts.length; index++)
                {
                    current = step(current, String(parts[index]));
                    if (current == null)
                    {
                        break;
                    }
                }
                if (index == parts.length)
                {
                    return current;
                }
            }

            var display:DisplayObject = displayRoot();
            var cursor:Object = display;
            var start:int = 0;
            if (parts.length > 0 && display is DisplayObjectContainer)
            {
                var container:DisplayObjectContainer = DisplayObjectContainer(display);
                var child:DisplayObject = container.getChildByName(String(parts[0]));
                if (child == null && display.stage != null)
                {
                    child = display.stage.getChildByName(String(parts[0]));
                }
                if (child != null)
                {
                    cursor = child;
                    start = 1;
                }
            }
            for (var partIndex:int = start; partIndex < parts.length; partIndex++)
            {
                cursor = step(cursor, String(parts[partIndex]));
                if (cursor == null)
                {
                    return null;
                }
            }
            return cursor;
        }

        private static function coerce(value:*):*
        {
            if (value is Array)
            {
                var copy:Array = [];
                var index:int;
                for (index = 0; index < value.length; index++)
                {
                    copy.push(coerce(value[index]));
                }
                return copy;
            }
            if (value != null && value is Object &&
                !(value is String) && !(value is Number) && !(value is Boolean))
            {
                // The controller refers to a live map object by the stable ID
                // included in model snapshots. Resolve it here so calls such
                // as moveHeroToCordinates(x, y, collectable) receive the real
                // ActionScript Collectable instance, not a JSON-shaped copy.
                if (value.hasOwnProperty("$xenomit_entity_id"))
                {
                    var manager:Object = resolvePath(
                        "net.bigpoint.darkorbit.managers.ScreenManager.getInstance()");
                    var gameMap:Object = member(manager, "map");
                    if (gameMap == null)
                    {
                        return null;
                    }
                    return gameMap.getEntityById(int(value["$xenomit_entity_id"]));
                }
                var object:Object = {};
                for (var key:String in value)
                {
                    object[key] = coerce(value[key]);
                }
                return object;
            }
            if (value is Number)
            {
                var number:Number = Number(value);
                if (!isNaN(number) && number == Math.floor(number) &&
                    number >= -2147483648 && number <= 2147483647)
                {
                    return int(number);
                }
            }
            return value;
        }

        private static function fail(message:String, stack:String = ""):String
        {
            var payload:Object = {status: "error", message: message};
            if (stack != null && stack != "")
            {
                payload.stack = stack;
            }
            return JSON.stringify(payload);
        }

        private static function succeed(result:*):String
        {
            var data:* = result === undefined ? null : result;
            try
            {
                return JSON.stringify({status: "ok", data: data});
            }
            catch (error:Error)
            {
                return JSON.stringify({
                    status: "ok",
                    data: String(result),
                    note: "result was not JSON-serializable"
                });
            }
            return fail("result was not JSON-serializable");
        }

        public static function invoke(targetPath:String, method:String, jsonArgs:String):String
        {
            try
            {
                var args:Array = [];
                if (jsonArgs != null && jsonArgs != "")
                {
                    var parsed:* = JSON.parse(jsonArgs);
                    if (!(parsed is Array))
                    {
                        return fail("arguments must be a JSON array");
                    }
                    args = coerce(parsed) as Array;
                }
                var target:Object = resolvePath(targetPath);
                if (target == null || !(target[method] is Function))
                {
                    return fail("Method or target not found");
                }
                var result:* = (target[method] as Function).apply(target, args);
                return succeed(result);
            }
            catch (error:Error)
            {
                var stack:String = error.getStackTrace();
                return fail(error.message, stack == null ? "" : stack);
            }
            return fail("invoke failed");
        }

        public static function inspect(targetPath:String):String
        {
            try
            {
                var target:Object = resolvePath(targetPath);
                if (target == null)
                {
                    return fail("target not found");
                }
                var xml:XML = describeType(target);
                var methods:Array = [];
                var accessors:Array = [];
                var variables:Array = [];
                collectMembers(xml.method, "declared", methods, accessors, variables, true);
                collectMembers(xml.accessor, "declared", methods, accessors, variables, false);
                collectMembers(xml.variable, "declared", methods, accessors, variables, false);
                if (xml.factory.length() > 0)
                {
                    var factory:XML = xml.factory[0];
                    collectMembers(factory.method, "instance", methods, accessors, variables, true);
                    collectMembers(factory.accessor, "instance", methods, accessors, variables, false);
                    collectMembers(factory.variable, "instance", methods, accessors, variables, false);
                }
                return JSON.stringify({
                    status: "ok",
                    name: String(xml.@name),
                    isClass: target is Class,
                    qualified: getQualifiedClassName(target),
                    methods: methods,
                    accessors: accessors,
                    variables: variables
                });
            }
            catch (error:Error)
            {
                var stack:String = error.getStackTrace();
                return fail(error.message, stack == null ? "" : stack);
            }
            return fail("inspect failed");
        }

        /** A snapshot of displayed objects, taken on the player thread. */
        public static function state(targetPath:String):String
        {
            try
            {
                var target:Object = resolvePath(targetPath);
                var root:DisplayObject = target as DisplayObject;
                if (root == null)
                {
                    return fail("target is not a display object");
                }
                var stats:Object = {visited: 0, truncated: false};
                // The map model has stable game entity IDs. Use it directly
                // and reserve the more expensive display-list walk for screens
                // where the map model is not available yet.
                var result:Array = mapEntitiesSnapshot();
                if (result == null)
                {
                    result = [];
                    snapshot(root, result, stats, "root", 0);
                }
                else
                {
                    stats.visited = result.length;
                }
                return JSON.stringify({status: "ok", data: {
                    stageWidth: root.stage == null ? null : root.stage.stageWidth,
                    stageHeight: root.stage == null ? null : root.stage.stageHeight,
                    map: mapSnapshot(),
                    entities: result,
                    visited: stats.visited,
                    truncated: stats.truncated
                }});
            }
            catch (error:Error)
            {
                return fail(error.message, error.getStackTrace());
            }
            return fail("state failed");
        }

        /**
         * Prefer actual model objects while the game is on a map. Display-list
         * wrappers do not carry the stable IDs needed by behavior decisions.
         * Keep x/y in stage pixels for the shared API and provide worldX/Y for
         * the runtime planner's movement and distance calculations.
         */
        private static function mapEntitiesSnapshot():Array
        {
            try
            {
                var screen:Object = resolvePath(
                    "net.bigpoint.darkorbit.managers.ScreenManager.getInstance()");
                var gameMap:Object = member(screen, "map");
                var mapView:Object = member(screen, "mapView");
                var view:DisplayObject = mapView as DisplayObject;
                if (gameMap == null || member(gameMap, "hero") == null || view == null)
                {
                    return null;
                }
                var source:* = member(gameMap, "entities");
                if (source == null)
                {
                    return null;
                }

                var result:Array = [];
                for each (var entity:Object in source)
                {
                    if (entity == null)
                    {
                        continue;
                    }
                    var id:* = member(entity, "id");
                    var position:Object = member(entity, "position");
                    if (id == null || position == null)
                    {
                        continue;
                    }
                    var worldX:Number = Number(member(position, "x"));
                    var worldY:Number = Number(member(position, "y"));
                    if (isNaN(worldX) || isNaN(worldY))
                    {
                        continue;
                    }

                    var stageX:Number = worldX;
                    var stageY:Number = worldY;
                    try
                    {
                        var screenPoint:Object = mapView.game2screen(int(worldX), int(worldY));
                        if (screenPoint != null)
                        {
                            var globalPoint:Point = view.localToGlobal(
                                new Point(Number(screenPoint.x), Number(screenPoint.y)));
                            stageX = globalPoint.x;
                            stageY = globalPoint.y;
                        }
                    }
                    catch (screenError:Error)
                    {
                    }

                    var className:String = getQualifiedClassName(entity);
                    var fields:Object = {id: id, worldX: worldX, worldY: worldY};
                    var npc:* = member(entity, "isNPC");
                    if (npc != null)
                    {
                        fields.isNpc = Boolean(npc);
                    }
                    if (npc != null)
                    {
                        fields.isHero = callBoolean(entity, "isHeroShip");
                        var hpTrait:Object = member(entity, "hp");
                        putNumber(fields, "health", nestedValue(hpTrait, "hp", "value"));
                        putNumber(fields, "maxHealth", nestedValue(hpTrait, "maxHP", "value"));
                        putNumber(fields, "shield", nestedValue(hpTrait, "shield", "value"));
                        putNumber(fields, "maxShield", nestedValue(hpTrait, "maxShield", "value"));
                        putNumber(fields, "nanohullHP", nestedValue(hpTrait, "nanohullHP", "value"));
                        putNumber(fields, "nanohullMaxHP", nestedValue(hpTrait, "nanohullMaxHP", "value"));
                    }

                    var hash:* = member(entity, "hash");
                    if (hash is String)
                    {
                        fields.isCollectable = true;
                        fields.hash = String(hash);
                    }
                    var typeID:* = member(entity, "typeID");
                    if (typeID is Number)
                    {
                        fields.typeID = Number(typeID);
                    }
                    var classSeparator:int = className.lastIndexOf("::");
                    var classDot:int = className.lastIndexOf(".");
                    var simpleName:String = className.substr(
                        Math.max(classSeparator + 2, classDot + 1));
                    if (simpleName == "Box")
                    {
                        fields.entityKind = "box";
                    }
                    else if (simpleName == "Ore")
                    {
                        fields.entityKind = "ore";
                    }
                    else if (simpleName == "Beacon")
                    {
                        fields.entityKind = "beacon";
                    }

                    var name:String = className;
                    var info:Object = member(entity, "info");
                    var username:* = nestedValue(info, "username", "value");
                    if (username is String && String(username) != "")
                    {
                        name = String(username);
                    }
                    else if (hash is String && String(hash) != "")
                    {
                        name = String(hash);
                    }
                    result.push({path: "map/" + String(id), className: className,
                        name: name, x: stageX, y: stageY,
                        bounds: {x: stageX, y: stageY, width: 0, height: 0}, fields: fields});
                    if (result.length >= 2000)
                    {
                        break;
                    }
                }
                return result;
            }
            catch (error:Error)
            {
                return null;
            }
            return null;
        }

        /** Current world-space map data used by the controller's live map. */
        private static function mapSnapshot():Object
        {
            try
            {
                var screen:Object = resolvePath(
                    "net.bigpoint.darkorbit.managers.ScreenManager.getInstance()");
                var gameMap:Object = member(screen, "map");
                if (gameMap == null)
                {
                    return null;
                }

                var mapId:Number = Number(member(gameMap, "mapID"));
                if (isNaN(mapId))
                {
                    mapId = Number(callMember(gameMap, "getMapID"));
                }
                var mapName:* = callMember(gameMap, "getName");
                var mapWidth:Number = Number(member(gameMap, "serious_width"));
                var mapHeight:Number = Number(member(gameMap, "serious_height"));
                if (isNaN(mapWidth) || mapWidth <= 0)
                {
                    mapWidth = 21000;
                }
                if (isNaN(mapHeight) || mapHeight <= 0)
                {
                    mapHeight = 13100;
                }

                var hero:Object = member(gameMap, "hero");
                var heroPosition:Object = member(hero, "position");
                var playerX:Number = Number(member(heroPosition, "x"));
                var playerY:Number = Number(member(heroPosition, "y"));
                var hasPlayerPosition:Boolean = heroPosition != null &&
                    !isNaN(playerX) && !isNaN(playerY);
                var playerPosition:Object = hasPlayerPosition ?
                    {x: playerX, y: playerY} : null;

                var safePlaces:Array = [];
                var source:* = member(gameMap, "entities");
                if (source != null)
                {
                    for each (var entity:Object in source)
                    {
                        if (entity == null)
                        {
                            continue;
                        }
                        var className:String = getQualifiedClassName(entity);
                        var separator:int = className.lastIndexOf("::");
                        var dot:int = className.lastIndexOf(".");
                        var simpleName:String = className.substr(
                            Math.max(separator + 2, dot + 1));
                        var kind:String = "";
                        var radius:Number = 250;
                        if (simpleName.indexOf("Portal") >= 0)
                        {
                            kind = "portal";
                        }
                        else if (simpleName.indexOf("BattleStation") >= 0)
                        {
                            kind = "battle_station";
                        }
                        else if (simpleName.indexOf("BaseSpot") >= 0 ||
                            simpleName == "BaseStation" ||
                            simpleName == "BaseHeadquarters")
                        {
                            kind = "base";
                            if (simpleName.indexOf("Headquarters") >= 0)
                            {
                                radius = 1250;
                            }
                            else if (simpleName.indexOf("BaseStation") >= 0)
                            {
                                radius = 1500;
                            }
                        }
                        if (kind == "")
                        {
                            continue;
                        }

                        var safePosition:Object = member(entity, "position");
                        var safeX:Number = Number(member(safePosition, "x"));
                        var safeY:Number = Number(member(safePosition, "y"));
                        if (safePosition == null || isNaN(safeX) || isNaN(safeY))
                        {
                            continue;
                        }
                        var safeName:* = member(entity, "name");
                        safePlaces.push({kind: kind,
                            name: safeName is String ? String(safeName) : simpleName,
                            x: safeX, y: safeY, radius: radius});
                    }
                }

                var inSafePlace:Boolean = false;
                if (hasPlayerPosition)
                {
                    for each (var safePlace:Object in safePlaces)
                    {
                        var dx:Number = playerX - Number(safePlace.x);
                        var dy:Number = playerY - Number(safePlace.y);
                        var safeRadius:Number = Number(safePlace.radius);
                        if (dx * dx + dy * dy <= safeRadius * safeRadius)
                        {
                            inSafePlace = true;
                            break;
                        }
                    }
                }

                return {
                    id: isNaN(mapId) ? null : int(mapId),
                    name: mapName is String ? String(mapName) : "",
                    width: mapWidth,
                    height: mapHeight,
                    playerPosition: playerPosition,
                    safePlaces: safePlaces,
                    inSafePlace: hasPlayerPosition ? inSafePlace : null
                };
            }
            catch (error:Error)
            {
                return null;
            }
            return null;
        }

        private static function member(value:Object, name:String):*
        {
            if (value == null)
            {
                return null;
            }
            try
            {
                return value[name];
            }
            catch (error:Error)
            {
            }
            return null;
        }

        private static function callMember(target:Object, method:String):*
        {
            var callable:* = member(target, method);
            if (callable is Function)
            {
                try
                {
                    return (callable as Function).apply(target, []);
                }
                catch (error:Error)
                {
                }
            }
            return null;
        }

        private static function nestedValue(value:Object, first:String, second:String):*
        {
            return member(member(value, first), second);
        }

        private static function putNumber(target:Object, key:String, value:*):void
        {
            if (value is Number && !isNaN(Number(value)))
            {
                target[key] = Number(value);
            }
        }

        private static function callBoolean(target:Object, method:String):Boolean
        {
            var callable:* = member(target, method);
            if (callable is Function)
            {
                try
                {
                    return Boolean((callable as Function).apply(target, []));
                }
                catch (error:Error)
                {
                }
            }
            return false;
        }

        private static function snapshot(object:DisplayObject, output:Array,
                                          stats:Object, path:String, depth:int):void
        {
            if (object == null || depth > 64 || stats.visited >= 5000)
            {
                stats.truncated = true;
                return;
            }
            stats.visited++;
            if (!object.visible || object.alpha <= 0)
            {
                return;
            }
            var position:Point;
            var bounds:Rectangle;
            try
            {
                position = object.localToGlobal(new Point(0, 0));
                bounds = object.getBounds(object.stage == null ? object : object.stage);
            }
            catch (error:Error)
            {
                return;
            }
            var width:Number = object.stage == null ? NaN : object.stage.stageWidth;
            var height:Number = object.stage == null ? NaN : object.stage.stageHeight;
            var onScreen:Boolean = object.stage == null ||
                (bounds.right >= 0 && bounds.bottom >= 0 &&
                 bounds.left <= width && bounds.top <= height);
            if (!onScreen)
            {
                return;
            }
            var fields:Object = {};
            var className:String = getQualifiedClassName(object);
            var names:Array = stateFields(className);
            for each (var field:String in names)
            {
                try
                {
                    var value:* = object[field];
                    if (value is String || value is Number || value is Boolean)
                    {
                        fields[field] = value;
                    }
                }
                catch (error:Error)
                {
                }
            }
            // Ignore tiny leaf graphics; retain containers and any object with
            // gameplay fields, including portals and ships with no children.
            var container:DisplayObjectContainer = object as DisplayObjectContainer;
            if (depth == 0 || (bounds.width > 1 && bounds.height > 1 &&
                (container != null || hasFields(fields))))
            {
                output.push({path: path, className: className,
                    name: object.name, x: position.x, y: position.y,
                    bounds: {x: bounds.x, y: bounds.y, width: bounds.width,
                             height: bounds.height}, fields: fields});
            }
            if (container != null)
            {
                for (var index:int = 0; index < container.numChildren; index++)
                {
                    if (stats.visited >= 5000)
                    {
                        stats.truncated = true;
                        return;
                    }
                    try
                    {
                        snapshot(container.getChildAt(index), output, stats,
                            path + "/" + index, depth + 1);
                    }
                    catch (error:Error)
                    {
                    }
                }
            }
        }

        private static function hasFields(fields:Object):Boolean
        {
            for (var key:String in fields)
            {
                return true;
            }
            return false;
        }

        private static function stateFields(className:String):Array
        {
            if (_stateFields[className] != null)
            {
                return _stateFields[className] as Array;
            }
            // Keep the visual fallback bounded instead of reflecting every
            // display object while walking the stage.
            var names:Array = ["health", "maxHealth", "currentHealth", "healthPoints",
                "hp", "maxHp", "hitPoints", "maxHitPoints", "shield", "maxShield",
                "id", "userId", "type", "typeID", "faction", "portalId", "isHero",
                "isNpc", "isNPC", "isHeroShip", "hash"];
            _stateFields[className] = names;
            return names;
        }

        private static function collectMembers(
            list:XMLList, owner:String, methods:Array, accessors:Array,
            variables:Array, asMethods:Boolean):void
        {
            for each (var node:XML in list)
            {
                var name:String = String(node.@name);
                if (asMethods)
                {
                    if (methods.length >= 400)
                    {
                        return;
                    }
                    methods.push({
                        name: name,
                        params: node.parameter.length(),
                        owner: owner
                    });
                }
                else if (String(node.name()) == "accessor")
                {
                    if (accessors.length < 400)
                    {
                        accessors.push({name: name, access: String(node.@access), owner: owner});
                    }
                }
                else if (variables.length < 400)
                {
                    variables.push({name: name, owner: owner});
                }
            }
        }
    }
}
