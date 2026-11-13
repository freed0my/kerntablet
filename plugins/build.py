#!/usr/bin/env python3
"""Builds the plugin repository.

    ./plugins/build.py            build every plugins/<id>/ into repo/<id>/
                                  and write repo/index.json

Each plugin directory holds plugin.toml and one or more *.bpf.c files (all
compiled and linked into the object named by `object` in the manifest).
Publish repo/ anywhere reachable over https:// and point kerntablet at its
index.json (`repository` in ~/.config/kerntablet/plugins.toml); see
PLUGINS.md. Needs clang, bpftool and a built
target/release/kerntablet-loader (used to validate each plugin).
"""
import hashlib
import json
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "plugins"
OUT = ROOT / "repo"
BPF = ROOT / "bpf"
LOADER = ROOT / "target" / "release" / "kerntablet-loader"
CFLAGS = ["-O2", "-g", "-target", "bpf", "-D__TARGET_ARCH_x86",
          "-Wall", "-Wextra", "-Wno-unused-parameter", "-Werror", "-I", str(BPF)]


def sha256(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def build(src: Path, tmp: Path) -> dict:
    m = tomllib.loads((src / "plugin.toml").read_text())
    pid = m["id"]
    if src.name != pid:
        sys.exit(f"{src}: directory name must equal the id ({pid})")
    sources = sorted(src.glob("*.bpf.c"))
    if not sources:
        sys.exit(f"{src}: no *.bpf.c")
    dest = tmp / pid
    dest.mkdir(parents=True)
    objs = []
    for c in sources:
        o = dest / (c.name[:-2] + ".o.part")
        subprocess.run(["clang", *CFLAGS, "-c", str(c), "-o", str(o)], check=True)
        objs.append(o)
    obj = dest / m["object"]
    if len(objs) == 1:
        objs[0].rename(obj)
    else:
        subprocess.run(["bpftool", "gen", "object", str(obj), *map(str, objs)], check=True)
        for o in objs:
            o.unlink()
    subprocess.run(["llvm-strip", "-g", str(obj)], check=False)
    shutil.copy(src / "plugin.toml", dest / "plugin.toml")
    subprocess.run([str(LOADER), "check", str(dest)], check=True)
    return {
        "id": pid,
        "name": m["name"],
        "version": m["version"],
        "description": " ".join(m.get("description", "").split()),
        "license": m.get("license"),
        "homepage": m.get("homepage"),
        "files": [{"name": f, "sha256": sha256(dest / f)} for f in ("plugin.toml", m["object"])],
    }


def main() -> None:
    if not LOADER.exists():
        sys.exit("build the workspace first: cargo build --release")
    tmp = ROOT / "repo.tmp"
    shutil.rmtree(tmp, ignore_errors=True)
    tmp.mkdir()
    entries = [build(d, tmp) for d in sorted(SRC.iterdir()) if (d / "plugin.toml").exists()]
    (tmp / "index.json").write_text(json.dumps({"abi": 1, "plugins": entries}, indent=2) + "\n")
    shutil.rmtree(OUT, ignore_errors=True)
    tmp.rename(OUT)
    for e in entries:
        print(f"{e['id']} {e['version']}")
    print(f"wrote {OUT / 'index.json'}")


if __name__ == "__main__":
    main()
