"""The scan over every client tree and the failure report."""

from __future__ import annotations

from pathlib import Path

from api_surface.kotlin import kotlin_method, kotlin_paths
from api_surface.patterns import (
    BUILD_DIRS,
    CLIENTS,
    COMMENT,
    EXTERNAL_URL,
    HELPER_CALL,
    HELPER_DECL,
    HELPER_USE,
    HELPERS,
    IMPORT,
    INTERPOLATION,
    KOTLIN_EXTS,
    LITERAL,
    METHOD_WINDOW_LINES,
    MODULE_SPECIFIER,
    MONOREPO,
    MONOREPO_MARKER,
    TRAILING_JUNK,
)
from api_surface.routes import (
    method_served,
    normalise,
    own_server_routes,
    owns,
    resolves,
    served_paths,
)
from api_surface.sites import is_test_source, logical_line, stated_method, unserved_method


def main() -> int:
    table = served_paths()
    failures: list[tuple[str, Path, int, str]] = []
    fragments: list[tuple[str, Path, int, str]] = []
    method_failures: list[tuple[str, Path, int, str, str]] = []
    scanned = 0
    missing: list[tuple[str, Path]] = []
    in_monorepo = MONOREPO_MARKER.is_file()
    for name, root, exts in CLIENTS:
        if not root.exists():
            if in_monorepo:
                missing.append((name, root))
                print(f"  MISSING {name}: {root} (monorepo checkout, root expected)")
            else:
                print(f"  skip {name}: {root} not in this checkout")
            continue
        seen = 0
        # Android literals checked by path alone: a route constant (a socket
        # path, a base) or a call whose verb the site does not state.
        path_only = 0
        own_routes = own_server_routes(root)
        # A client root is a source directory, or a single module file.
        files = [root] if root.is_file() else sorted(root.rglob("*"))
        for file in files:
            if not file.is_file() or file.suffix not in exts or is_test_source(file, root):
                continue
            if any(part in BUILD_DIRS for part in file.relative_to(root).parts[:-1]):
                continue
            try:
                text = file.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            lines = text.splitlines()
            kotlin = file.suffix in KOTLIN_EXTS
            if kotlin:
                lines = kotlin_paths(lines)
            consumed = 0
            for lineno, raw_line in enumerate(lines, 1):
                # A line already read as the continuation of an open
                # interpolation above belongs to that call, not a new one.
                if lineno <= consumed:
                    continue
                if COMMENT.match(raw_line) or IMPORT.search(raw_line):
                    continue
                joined, consumed = logical_line(lines, lineno - 1)
                line = INTERPOLATION.sub("{}", joined)
                tail = INTERPOLATION.sub(
                    "{}", "\n".join(lines[consumed : consumed + METHOD_WINDOW_LINES])
                )

                # Paths built by a helper from a prefix and a sub-path argument.
                # Read from the line before interpolations collapse, because the
                # call usually sits inside one (`${this.pluginUrl(id, "x")}`).
                helper_spans: list[tuple[int, int]] = []
                for h in HELPER_CALL.finditer(joined):
                    helper_spans.append(h.span())
                    sub = h.group("sub") or ""
                    path = normalise(HELPERS[h.group("name")] + ("/" + sub if sub else ""))
                    seen += 1
                    if sub.endswith(tuple(TRAILING_JUNK)):
                        fragments.append((name, file, lineno, path + "/"))
                        continue
                    if not resolves(path, table):
                        failures.append((name, file, lineno, path))
                        continue
                    bad = unserved_method(
                        path,
                        joined[: h.start()],
                        INTERPOLATION.sub("{}", joined[h.end() :]),
                        lines,
                        lineno,
                        line,
                        tail,
                        table,
                    )
                    if bad:
                        method_failures.append((name, file, lineno, bad, path))
                for u in HELPER_USE.finditer(joined):
                    if HELPER_DECL.search(joined[: u.start()]):
                        continue
                    if not any(s <= u.start() < e for s, e in helper_spans):
                        seen += 1
                        fragments.append(
                            (
                                name,
                                file,
                                lineno,
                                f"{HELPERS[u.group('name')]}/<non-literal sub-path>",
                            )
                        )

                for m in LITERAL.finditer(line):
                    before = line[: m.start()]
                    if MODULE_SPECIFIER.search(before) or EXTERNAL_URL.search(before):
                        continue
                    raw = m.group(0)
                    path = normalise(raw)
                    if not path.startswith(("/api", "/whep", "/hls/")):
                        continue
                    if own_routes and owns(path, own_routes):
                        continue
                    seen += 1
                    # Half of a path assembled elsewhere: `"/api/" + x`, or a
                    # literal that stops at a separator or an interpolation
                    # that never closes. It names no route as written.
                    if (
                        path == "/api"
                        or raw.endswith(tuple(TRAILING_JUNK))
                        or line[m.end() :].startswith("$")
                    ):
                        fragments.append((name, file, lineno, raw))
                        continue
                    if not resolves(path, table):
                        failures.append((name, file, lineno, path))
                        continue
                    if kotlin:
                        rest = line[m.end() :] + "\n" + tail
                        method = stated_method(before, rest) or kotlin_method(before, lines, lineno)
                        if method is None:
                            path_only += 1
                        elif not method_served(path, method, table):
                            method_failures.append((name, file, lineno, method, path))
                        continue
                    bad = unserved_method(
                        path, before, line[m.end() :], lines, lineno, line, tail, table
                    )
                    if bad:
                        method_failures.append((name, file, lineno, bad, path))
        scanned += seen
        note = f" ({path_only} checked by path alone: no verb at the site)" if path_only else ""
        print(f"  {name}: {seen} path literals{note}")

    if method_failures:
        print(f"\n{len(method_failures)} client call(s) use a method the route does not serve:\n")
        for name, file, lineno, method, path in method_failures:
            rel = file.relative_to(MONOREPO) if MONOREPO in file.parents else file
            print(f"  {method} {path}\n    {name} — {rel}:{lineno}")
    if failures:
        print(f"\n{len(failures)} client path literal(s) resolve against no agent route:\n")
        for name, file, lineno, path in failures:
            rel = file.relative_to(MONOREPO) if MONOREPO in file.parents else file
            print(f"  {path}\n    {name} — {rel}:{lineno}")
        print(
            "\nEither the route was renamed on one side only, or the client calls "
            "a surface the agent has never served. Fix the caller, add the route, "
            "or regenerate docs/api-surface.md if the agent side just changed."
        )
    if fragments:
        print(
            f"\n{len(fragments)} client path(s) are assembled in pieces and cannot be resolved:\n"
        )
        for name, file, lineno, path in fragments:
            rel = file.relative_to(MONOREPO) if MONOREPO in file.parents else file
            print(f"  {path}\n    {name} — {rel}:{lineno}")
        print(
            "\nWrite each agent path as one literal (a template literal with "
            "placeholders is fine) so a one-sided rename fails here, or, for a "
            "base the client appends to, add the base to BASE_PREFIXES."
        )
    if missing:
        print(f"\n{len(missing)} client root(s) missing from the monorepo checkout:\n")
        for name, root in missing:
            print(f"  {name}: {root}")
        print("\nA client moved its sources. Update CLIENTS to the new root.")
    if failures or fragments or method_failures or missing:
        return 1

    print(f"\nall {scanned} client path literals resolve against docs/api-surface.md")
    return 0
