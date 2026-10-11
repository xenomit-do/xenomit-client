r"""Control a running xenomit_client through its named pipe.

Same wire protocol as windows_flash_mvp/activex_player/control.py, against
the ruffle client's \\.\pipe\\xenomit_bridge_<PID> channel. `finvoke` runs the
call inside a real frame (post-frame callback), the equivalent of the ActiveX
enter-frame hook used by `move` — use it for calls like moveHeroToCordinates.
"""

from __future__ import annotations

import argparse


def command(pid: int, value: str) -> str:
    pipe = rf"\\.\pipe\xenomit_bridge_{pid}"
    try:
        with open(pipe, "r+b", buffering=0) as connection:
            connection.write(value.encode("utf-8"))
            # Replies are newline-terminated. Read that protocol record rather
            # than waiting for the server to disconnect the named pipe; on
            # Windows, the post-reply disconnect can make read-to-EOF raise
            # EINVAL even though the reply was already received.
            return connection.readline().decode("utf-8", "replace").strip()
    except PermissionError as error:
        raise SystemExit(
            "Access denied opening the player control pipe. Run this command "
            "from an Administrator PowerShell when the player is elevated."
        ) from error


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pid", type=int)
    sub = parser.add_subparsers(dest="action", required=True)
    sub.add_parser("status")
    sub.add_parser("bridge")
    state = sub.add_parser("state", help="snapshot visible display objects and gameplay fields")
    state.add_argument("target", nargs="?", default="stage")
    inspect = sub.add_parser("inspect")
    inspect.add_argument("target", help="class, singleton chain, root, or stage")
    for name in ("invoke", "finvoke"):
        call = sub.add_parser(name)
        call.add_argument("target")
        call.add_argument("method")
        call.add_argument("args", nargs="?", default="[]", help="JSON array of arguments")
    args = parser.parse_args()
    if args.action == "status": value = "STATUS"
    elif args.action == "bridge": value = "BRIDGE"
    elif args.action == "state": value = "STATE\t" + args.target
    elif args.action == "inspect": value = "INSPECT\t" + args.target
    elif args.action == "invoke": value = f"INVOKE\t{args.target}\t{args.method}\t{args.args}"
    else: value = f"FINVOKE\t{args.target}\t{args.method}\t{args.args}"
    print(command(args.pid, value))


if __name__ == "__main__":
    main()
