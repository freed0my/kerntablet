# Writing a kerntablet plugin

A plugin is a filter that changes the pen position (or pressure) before
kerntablet maps it to the screen: smoothing, prediction, snapping, anything
that works on the pen's coordinates. It runs **inside the kernel**, once per
pen report, so it adds no latency and no background process.

This guide covers what a plugin consists of, how to write one, how to build
it, install it and pass it on.

## What a plugin is

A directory with two files:

```
my-filter/
├── plugin.toml        what the plugin is, its settings, how they reach the kernel
└── my-filter.bpf.o    the filter itself, compiled from C for the kernel (BPF)
```

You write `plugin.toml` and a C file, `my-filter.bpf.c`; the `.bpf.o` is
built from it.

Where it runs:

```
pen report → kerntablet core → your plugin → other plugins → area mapping → wacom driver → desktop
```

The core turns every pen report into a shared structure with the position
and pressure; each plugin reads and changes it; the core then maps the
result to the screen. Plugins run in the order given by their `order`
number. They never deal with the raw USB report or with the screen.

Settings are split in two:

- In the **window** (userspace) the user's settings are turned into plain
  integers. Everything expensive happens here: floating point, `pow`, `exp`,
  division, tables.
- In the **kernel** the filter only does cheap integer arithmetic on each
  report, using those integers.

`plugin.toml` describes that conversion, so most plugins need no Rust code
at all.

## Quick start

Copy this into `plugins/my-filter/` in the kerntablet source tree.

`plugin.toml`:

```toml
id = "my-filter"
name = "My Filter"
version = "1.0.0"
abi = 1
license = "GPL-2.0-only"
description = "Moves the cursor towards the pen with a fixed strength."
object = "my-filter.bpf.o"

[[param]]
key = "strength"
label = "Strength"
default = 0.5
min = 0.0
max = 1.0
step = 0.01
tip = "0 = no filtering, 1 = the cursor stays in place"

[[field]]
name = "keep"          # strength as a Q20 fixed-point number
type = "u32"
expr = "q20(strength)"
```

`my-filter.bpf.c`:

```c
#include "kerntablet-plugin.h"

struct my_config {
	struct kt_plugin_hdr hdr;   /* always first, filled in by kerntablet */
	__u32 keep;                 /* the [[field]]s, in the same order */
};

KT_PIPE_MAP;
KT_PLUGIN_CONFIG(struct my_config);

/* State kept between reports: the last output position. */
struct state { __s64 x, y; __u32 valid; };
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct state);
} my_state SEC(".maps");

SEC("struct_ops/hid_device_event")
int BPF_PROG(my_event, struct hid_bpf_ctx *hctx, enum hid_report_type rtype, __u64 source)
{
	__u32 key = 0;
	struct kt_pipe *p = bpf_map_lookup_elem(&kt_pipe, &key);
	struct my_config *cfg = bpf_map_lookup_elem(&kt_plugin_config, &key);
	struct state *s = bpf_map_lookup_elem(&my_state, &key);

	if (!p || !s)
		return 0;
	if (p->flags & KT_P_LEFT)            /* pen left: forget the old point */
		s->valid = 0;
	if (!(p->flags & KT_P_PEN) || !kt_plugin_on(cfg ? &cfg->hdr : NULL))
		return 0;

	if (s->valid) {
		/* out = in + (last - in) * keep, keep in Q20 */
		__s64 dx = (s->x - p->x) >> 10, dy = (s->y - p->y) >> 10;
		p->x += (dx * cfg->keep) >> 10;
		p->y += (dy * cfg->keep) >> 10;
	}
	s->x = p->x;
	s->y = p->y;
	s->valid = 1;
	return 0;
}

KT_PLUGIN(my_event);
```

Then:

```
cargo build --release          # once: builds the loader used for checking
./plugins/build.py             # builds every plugin in plugins/ into repo/
kerntablet plugin install ./repo/my-filter
```

Open the window: **My Filter** is on the Filters tab. Tick it, set the
strength, press Apply.

## `plugin.toml` reference

### Top-level keys

| Key | Required | Meaning |
|---|---|---|
| `id` | yes | lowercase letters, digits and `-`; the same as the directory name |
| `name` | yes | shown in the window |
| `version` | yes | change it on every release: the window offers updates by comparing it |
| `abi` | yes | `1` |
| `object` | yes | file name of the compiled filter, e.g. `my-filter.bpf.o` |
| `description` | no | one paragraph shown in the window |
| `license` | no | SPDX name, e.g. `GPL-2.0-only`; shown in the window |
| `homepage` | no | link shown in the window |
| `order` | no | position in the chain, default `100`; lower runs first |
| `lets` | no | helper values for the fields, see below |
| `otd_filter` | no | OpenTabletDriver filter path; lets File → Import from OpenTabletDriver fill in the settings |

Top-level keys must come before the first `[[param]]` or `[[field]]` —
that is how TOML works.

### `[[param]]`: a setting in the window

| Key | Meaning |
|---|---|
| `key` | name used in expressions |
| `label` | text in the window |
| `type` | `float` (default, a number field) or `bool` (a checkbox) |
| `default` | default value (`2.0`, `true`) |
| `min`, `max` | allowed range of a number |
| `step` | precision the user is expected to need |
| `unit` | shown after the field, e.g. `ms` |
| `tip` | hover text |
| `enabled_if` | key of a `bool` param; this one is greyed out while that is off |
| `otd` | the matching OpenTabletDriver property name, for importing |

### `[[field]]`: one value written to the kernel

The fields are packed, in order, after `struct kt_plugin_hdr` in your C
config struct. Use the same order and types in C.

| Key | Meaning |
|---|---|
| `name` | for your own reference |
| `type` | `u8 i8 u16 i16 u32 i32 u64 i64` |
| `expr` | expression giving the value; rounded and clamped to the type, NaN becomes 0 |
| `lut` | `"dist"` turns the field into a table, see below |
| `lets` | helper values for this field only |

### Expressions

Numbers, `+ - * / % ^`, comparisons, `&& || !`, and the functions
`pow exp ln log2 log10 sqrt abs floor ceil round trunc sin cos tan sinh cosh
tanh atan2 min max clamp if(cond, a, b) isnan isfinite q20(x)`.

Variables: every param `key` (a `bool` is 0 or 1), the `lets`, and the
tablet's constants:

| Name | Meaning |
|---|---|
| `mm_per_unit` | millimetres per tablet unit (0.01) |
| `max_x`, `max_y` | largest raw coordinates |
| `width_mm`, `height_mm` | size of the tablet surface |
| `max_pressure` | largest raw pressure |

`lets` are evaluated once, in order, before the fields:

```toml
lets = ["interval = 1000 / frequency", "k = exp(-interval / latency)"]
```

`q20(x)` is `x * 2^20` — the fixed-point format the kernel side uses for
fractions (see below).

### Distance tables

Some filters need a function of the pen's speed or of a distance, like
`pow(speed, -2)`. That cannot be computed in the kernel, so it becomes a
table: with `lut = "dist"` the field is an array of `KT_LUT_LEN` (288)
entries, and your expression is evaluated for each entry with `d` set to a
distance in millimetres.

```toml
[[field]]
name = "weight"
type = "i32"
lut = "dist"
lets = ["w = pow(d + 1, -strength)"]
expr = "q20(clamp(w, 0, 1))"
```

In C the field is `__s32 weight[KT_LUT_LEN];` and you look up a distance
with:

```c
__u64 q = kt_dist2(dx, dy);            /* dx, dy: Q20 tablet units */
__s64 w = kt_lut(cfg->weight, q);      /* interpolated table value */
```

The table is accurate to about 0.05 mm for smooth functions.

## The kernel side

Include `kerntablet-plugin.h`; it gives you everything below.

### What you get on every report

```c
struct kt_pipe {
	__s64 x, y;          /* position, tablet units in Q20 (100 units = 1 mm) */
	__u64 now_ns;        /* time of this report, nanoseconds */
	__u32 flags;         /* KT_P_PEN, KT_P_LEFT, KT_P_DROP */
	__u32 status;        /* raw status bits of the report */
	__u32 pressure;      /* 0..max_pressure, may be changed */
	__u32 max_x, max_y, max_pressure;
};
```

| Flag | Meaning |
|---|---|
| `KT_P_PEN` | a pen report in range: `x`, `y`, `pressure` are valid. Do nothing otherwise |
| `KT_P_LEFT` | the pen just left the tablet: reset your state |
| `KT_P_DROP` | set it to make this report count as "pen out of range" |

Change `p->x`, `p->y`, `p->pressure` to filter. Time between reports is
`p->now_ns` minus the value you stored the last time.

**Q20** means the number is multiplied by 2^20 (`KT_FIX_SHIFT`): `1.0` is
`1 << 20`. Multiplying two Q20 numbers needs a shift back by 20 — and since
a position is up to 2^34 in Q20, shift each side by 10 before multiplying,
as in the quick-start example, or the result overflows.

### Required parts

- `struct kt_plugin_hdr hdr;` as the first member of your config struct.
- `KT_PIPE_MAP;` and `KT_PLUGIN_CONFIG(struct your_config);`
- One function with `SEC("struct_ops/hid_device_event")` that always
  returns 0, and `KT_PLUGIN(that_function);` at the end.
- Check `kt_plugin_on(&cfg->hdr)`: it is false while the user has the
  plugin unticked.
- Keep state in your own `BPF_MAP_TYPE_ARRAY` with one entry. Each tablet
  gets its own copy of the plugin, so state is per tablet automatically.

### Rules of the kernel

The kernel checks the filter when it is loaded and refuses anything it
cannot prove safe. In practice:

- **Integers only.** Fractions are Q20 numbers prepared in `plugin.toml`.
- **No signed division** (`clang` refuses it for BPF). Divide unsigned
  numbers, or better, precompute `1 / x` as a field and multiply.
- **No `pow`, `exp`, `sqrt`, trigonometry.** Precompute values that depend
  only on settings as fields, and functions of the data as distance tables.
- **Loops need a constant upper bound** (or `bpf_loop()` for longer ones).
- **512 bytes of stack**, no sleeping, no timers.
- The filter runs once per report; it cannot produce extra reports between
  them.

The loader also refuses an object that contains anything but the filter
function, plain data maps, the config map of exactly the size `plugin.toml`
describes, and `kt_pipe`.

## Building and checking

`./plugins/build.py` builds every `plugins/<id>/` that has a `plugin.toml`:
compiles all its `*.bpf.c` with warnings as errors, links them into
`object`, checks the result with `kerntablet-loader check`, and writes
`repo/<id>/` plus `repo/index.json`. It needs `clang`, `llvm` and `bpftool`,
and a built loader (`cargo build --release`).

To build a single plugin by hand:

```
clang -O2 -g -target bpf -D__TARGET_ARCH_x86 -Wall -Werror \
      -I bpf -c plugins/my-filter/my-filter.bpf.c -o my-filter.bpf.o
./target/release/kerntablet-loader check ./my-filter/
```

`check` validates the manifest and the object without root. The kernel's
own verifier only runs at install time, so a few mistakes only show up
there; the install command prints the verifier's log when it refuses a
plugin.

Testing the arithmetic is easiest outside the kernel: compute the config
bytes with `Plugin::config_bytes()` in a Rust test (see the tests in
`crates/common/src/plugin.rs`), run a Rust copy of your C code on them and
compare with the formula you meant.

## Installing, updating, removing

```
kerntablet plugin install ./repo/my-filter      # a directory with plugin.toml + object
kerntablet plugin install my-filter             # by id, from a repository
kerntablet plugin remove my-filter
kerntablet plugin list
```

Installing needs the administrator password, because the plugin runs in the
kernel. Plugins go to `/usr/local/lib/kerntablet/plugins/<id>/` and are
attached to the tablet right away. Installing a newer `version` replaces the
old one; the user's settings are kept.

## Passing a plugin on

The simplest way is the plugin directory itself: `plugin.toml` and the
`.bpf.o`, for example in an archive or a git repository. The user installs
it with `kerntablet plugin install <dir>`.

For installing by id, publish a **repository**: a directory reachable over
`https://` (or `file://`) with an `index.json` and one subdirectory per
plugin — exactly what `./plugins/build.py` writes to `repo/`:

```json
{
  "abi": 1,
  "plugins": [
    {
      "id": "my-filter",
      "name": "My Filter",
      "version": "1.0.0",
      "description": "…",
      "license": "GPL-2.0-only",
      "homepage": null,
      "files": [
        { "name": "plugin.toml",     "sha256": "…" },
        { "name": "my-filter.bpf.o", "sha256": "…" }
      ]
    }
  ]
}
```

Users point kerntablet at it in `~/.config/kerntablet/plugins.toml`:

```toml
repository = "https://example.org/kerntablet-plugins/index.json"
```

Every downloaded file is checked against its SHA-256 before installing.

**License.** The filter is loaded into the kernel through GPL-only kernel
interfaces, so the BPF object itself must be under a GPL-compatible license
(`GPL-2.0-only`, `GPL-2.0-or-later`, `GPL-3.0-only`, …). Your plugin is a
separate work from kerntablet and can be distributed on its own under its
own license.

## Based on an OpenTabletDriver filter?

OpenTabletDriver filters are .NET code and cannot be loaded into the kernel,
so "porting" one means writing its algorithm again in C, following the rules
above. What maps to what:

| OpenTabletDriver | kerntablet |
|---|---|
| a filter (`IPositionedPipelineElement`), `PreTransform` | a plugin; it sees tablet units, before the area mapping |
| `PostTransform` (screen pixels) | not available; work in tablet units (usually the better choice for distances and speeds) |
| `[Property]`, `[SliderProperty]`, `[BooleanProperty]`, defaults | `[[param]]` |
| fields of the filter class | your state map |
| `Frequency` / async filters that run on a timer | run all the timer steps that fit since the previous report at once (`bpf_loop`); the output stays at the tablet's rate |

Mind the license: a port of someone's filter is a derived work of it and
has to be distributed under the original's license, by whoever has the
right to do so.
