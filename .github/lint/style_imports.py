from __future__ import annotations

import ast
import sys
from collections import deque
from dataclasses import dataclass, field
from pathlib import PurePosixPath

from style_scan import Finding

STANDARD_MODULES = frozenset(sys.stdlib_module_names) | {"__future__"}
FUNCTIONS = (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda)


@dataclass(frozen=True, slots=True)
class LazyImport:
    line: int
    shown: str
    targets: tuple[str, ...]


@dataclass(slots=True)
class Module:
    path: str
    names: tuple[str, ...]
    package: str
    eager: list[str] = field(default_factory=list[str])
    lazy: list[LazyImport] = field(default_factory=list[LazyImport])


def is_type_checking(node: ast.If) -> bool:
    test = node.test
    return (isinstance(test, ast.Name) and test.id == "TYPE_CHECKING") or (
        isinstance(test, ast.Attribute) and test.attr == "TYPE_CHECKING"
    )


def module_names(path: str, packages: frozenset[str]) -> tuple[tuple[str, ...], str]:
    pure = PurePosixPath(path)
    folders = pure.parent.parts
    depth = 0
    while depth < len(folders) and "/".join(folders[: len(folders) - depth]) in packages:
        depth += 1
    package = folders[len(folders) - depth :]
    stem = () if pure.stem == "__init__" else (pure.stem,)
    names = dict.fromkeys(".".join((*parts, *stem)) for parts in (package, folders))
    usable = tuple(
        name
        for name in names
        if name and name not in STANDARD_MODULES and all(part.isidentifier() for part in name.split("."))
    )
    return usable, ".".join(package)


def resolve(module: Module, node: ast.ImportFrom) -> str:
    if not node.level:
        return node.module or ""
    base = module.package.split(".") if module.package else []
    base = base[: len(base) - node.level + 1]
    return ".".join([*base, node.module] if node.module else base)


def prefixes(name: str) -> list[str]:
    parts = name.split(".")
    return [".".join(parts[:end]) for end in range(1, len(parts) + 1)]


class Index:
    def __init__(self, modules: list[Module]) -> None:
        self.by_name: dict[str, list[Module]] = {}
        known: set[str] = set()
        for module in modules:
            for name in module.names:
                self.by_name.setdefault(name, []).append(module)
                known.update(prefixes(name))
        self.known = frozenset(known - STANDARD_MODULES)
        self.importers: dict[str, set[str]] = {}
        self.heavy: frozenset[str] = frozenset()
        self.ancestors: dict[str, frozenset[str]] = {}

    def targets(self, module: Module, node: ast.Import | ast.ImportFrom) -> tuple[str, ...]:
        if isinstance(node, ast.Import):
            return tuple(alias.name for alias in node.names)
        base = resolve(module, node)
        return tuple(f"{base}.{alias.name}" if f"{base}.{alias.name}" in self.known else base for alias in node.names)

    def own_prefixes(self, name: str) -> list[str]:
        return [prefix for prefix in prefixes(name) if prefix in self.known]

    def link(self) -> None:
        direct: set[str] = set()
        for name, modules in self.by_name.items():
            for target in (target for module in modules for target in module.eager):
                own = self.own_prefixes(target)
                if not own and target.split(".", maxsplit=1)[0] not in STANDARD_MODULES:
                    direct.add(name)
                for prefix in own:
                    self.importers.setdefault(prefix, set()).add(name)
        self.heavy = self.spread(direct)

    def spread(self, start: set[str]) -> frozenset[str]:
        found = set(start)
        queue = deque(start)
        while queue:
            for importer in self.importers.get(queue.popleft(), ()):
                if importer not in found:
                    found.add(importer)
                    queue.append(importer)
        return frozenset(found)

    def is_heavy(self, name: str) -> bool:
        own = self.own_prefixes(name)
        if not own:
            return name.split(".", maxsplit=1)[0] not in STANDARD_MODULES
        return any(prefix in self.heavy for prefix in own)

    def reaches(self, name: str, module: Module) -> bool:
        if module.path not in self.ancestors:
            self.ancestors[module.path] = self.spread(set(module.names))
        return any(prefix in self.ancestors[module.path] for prefix in self.own_prefixes(name))


def collect(module: Module, index: Index, node: ast.AST, *, lazy: bool) -> None:
    if isinstance(node, ast.If) and not lazy and is_type_checking(node):
        for other in node.orelse:
            collect(module, index, other, lazy=lazy)
        return
    if isinstance(node, ast.Import | ast.ImportFrom):
        targets = index.targets(module, node)
        if lazy:
            shown = ", ".join(alias.name for alias in node.names)
            module.lazy.append(LazyImport(node.lineno, shown, targets))
        else:
            module.eager.extend(targets)
        return
    inner = lazy or isinstance(node, FUNCTIONS)
    for child in ast.iter_child_nodes(node):
        collect(module, index, child, lazy=inner)


def check_imports(sources: dict[str, str]) -> list[Finding]:
    packages = frozenset(
        str(PurePosixPath(path).parent) for path in sources if PurePosixPath(path).name == "__init__.py"
    )
    trees: dict[str, ast.Module] = {}
    modules: list[Module] = []
    for path, text in sources.items():
        try:
            trees[path] = ast.parse(text, filename=path)
        except SyntaxError:
            continue
        names, package = module_names(path, packages)
        modules.append(Module(path, names, package))
    index = Index(modules)
    for module in modules:
        collect(module, index, trees[module.path], lazy=False)
    index.link()
    findings: list[Finding] = []
    for module in modules:
        for item in module.lazy:
            if any(index.is_heavy(target) or index.reaches(target, module) for target in item.targets):
                continue
            findings.append(
                Finding(module.path, item.line, f"import of {item.shown} inside a function; it's light (PY-14)")
            )
    return findings
