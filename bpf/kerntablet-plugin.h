/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * kerntablet plugin SDK.
 *
 * A plugin is a HID-BPF struct_ops object attached to the tablet between the
 * core's two programs:
 *
 *   kt_head (parses the report into kt_pipe) -> plugins in order -> kt_tail
 *   (area transform of kt_pipe, writes the report)
 *
 * Plugins never touch the report bytes: they read and modify `struct kt_pipe`
 * (position in Q20 tablet units, before the area transform — OTD's
 * "PreTransform"). The loader shares the core's kt_pipe map with every plugin.
 *
 * Each plugin has one config map, `kt_plugin_config`, whose value starts with
 * `struct kt_plugin_hdr` followed by the fields declared in plugin.toml, in
 * the same order and with C alignment. Userspace computes those fields from
 * the user's settings (all float math happens there). A zeroed config — what
 * the loader writes before the user has applied anything — means "off".
 *
 * Any plugin-private state goes into its own (unpinned) maps; the object is
 * loaded once per tablet, so such maps are per device.
 */
#ifndef KERNTABLET_PLUGIN_H
#define KERNTABLET_PLUGIN_H

#include "kt-hid.h"

#define KT_PLUGIN_ABI	1
#define KT_PLUGIN_MAGIC	0x3150544b /* "KTP1" little endian */
#define KT_FIX_SHIFT	20

/* kt_pipe.flags */
#define KT_P_PEN	(1u << 0) /* in-proximity pen report: x, y, pressure are valid */
#define KT_P_LEFT	(1u << 1) /* the pen has just left proximity: reset your state */
#define KT_P_DROP	(1u << 2) /* set by a plugin: report the pen as out of range */

struct kt_pipe {
	__s64 x, y;		/* Q20 tablet units (100 units per mm on supported tablets) */
	__u64 now_ns;		/* bpf_ktime_get_ns() at kt_head */
	__u32 flags;		/* KT_P_* */
	__u32 status;		/* raw status byte (0x80 range, 0x40 prox, 0x01 tip, ...) */
	__u32 pressure;		/* may be modified */
	__u32 max_x;		/* logical axis maxima of the device */
	__u32 max_y;
	__u32 max_pressure;
};

struct kt_plugin_hdr {
	__u32 magic;		/* KT_PLUGIN_MAGIC once userspace has written a config */
	__u32 enabled;
};

/* Shared pipeline map; the loader makes it the core's instance. */
#define KT_PIPE_MAP							\
	struct {							\
		__uint(type, BPF_MAP_TYPE_ARRAY);			\
		__uint(max_entries, 1);					\
		__type(key, __u32);					\
		__type(value, struct kt_pipe);				\
	} kt_pipe SEC(".maps")

/* The plugin's config map; `cfg_type` must start with struct kt_plugin_hdr. */
#define KT_PLUGIN_CONFIG(cfg_type)						\
	struct {							\
		__uint(type, BPF_MAP_TYPE_ARRAY);			\
		__uint(max_entries, 1);					\
		__type(key, __u32);					\
		__type(value, cfg_type);					\
	} kt_plugin_config SEC(".maps")

/* Declares the struct_ops object; `fn` is the hid_device_event program. */
#define KT_PLUGIN(fn)							\
	SEC(".struct_ops.link")						\
	struct hid_bpf_ops kt_plugin = {				\
		.hid_device_event = (void *)fn,				\
	};								\
	char _license[] SEC("license") = "GPL"

static __always_inline int kt_plugin_on(const struct kt_plugin_hdr *h)
{
	return h && h->magic == KT_PLUGIN_MAGIC && h->enabled;
}

/* Hide a value from the optimiser so a bounds check stays in the object and
 * the verifier sees it. */
#define kt_opaque(v) asm volatile("" : "+r"(v))

/*
 * Distance lookup tables ("lut = dist" in plugin.toml). Indexed by a squared
 * distance q in (1/16 tablet unit)^2:
 *   q < 16:  idx = q
 *   else:    e = floor(log2 q), idx = 16 + (e - 4) * 8 + ((q >> (e - 3)) & 7)
 * linearly interpolated inside a bucket; q is clamped to KT_LUT_QMAX.
 * Userspace evaluates the table expression at d = sqrt(q)/16 units, in mm.
 */
#define KT_LUT_LEN	288
#define KT_LUT_QMAX	((1ull << 36) - 1)

static __always_inline __s64 kt_lut(const __s32 *lut, __u64 q)
{
	__u64 t, frac = 0;
	__u32 idx, e = 0, sh = 0;
	__s64 a, b;

	if (q > KT_LUT_QMAX)
		q = KT_LUT_QMAX;
	if (q < 16) {
		idx = (__u32)q;
	} else {
		t = q;
		if (t >> 32) { e += 32; t >>= 32; }
		if (t >> 16) { e += 16; t >>= 16; }
		if (t >> 8) { e += 8; t >>= 8; }
		if (t >> 4) { e += 4; t >>= 4; }
		if (t >> 2) { e += 2; t >>= 2; }
		if (t >> 1) { e += 1; }
		sh = e - 3;
		idx = 16 + (e - 4) * 8 + (__u32)((q >> sh) & 7);
		frac = q & ((1ull << sh) - 1);
	}
	kt_opaque(idx);
	if (idx > KT_LUT_LEN - 2)
		idx = KT_LUT_LEN - 2;
	a = lut[idx];
	if (!sh)
		return a;
	b = lut[idx + 1];
	return a + (((b - a) * (__s64)frac) >> sh);
}

/* Squared length of a Q20 vector in (1/16 unit)^2, the kt_lut index. */
static __always_inline __u64 kt_dist2(__s64 dx, __s64 dy)
{
	dx >>= 16;
	dy >>= 16;
	return (__u64)(dx * dx + dy * dy);
}

#endif
