// SPDX-License-Identifier: GPL-2.0-only
/*
 * kerntablet: in-kernel tablet area mapping via HID-BPF.
 *
 * Two struct_ops programs are attached to the tablet's HID device; plugins
 * (filters, see kerntablet-plugin.h) are attached between them:
 *
 *   kt_head  -> [plugins] -> kt_tail -> wacom driver
 *
 * kt_head parses the pen report into the shared kt_pipe map. kt_tail applies
 *
 *     out = M * pos + b      (affine, Q20 fixed point)
 *
 * to the (possibly filtered) position, so that "tablet area -> screen area"
 * (incl. rotation, clipping and area limiting) is already baked into the
 * coordinates the evdev device emits, and writes the report back. Userspace
 * only writes the matrix into the pinned `kt_config` map; nothing in
 * userspace ever touches the event stream.
 *
 * Report layout handled here: Wacom "Bamboo pen" style (One by Wacom
 * CTL-472 / CTL-672): report id 0x02,
 *   [1] status: 0x80 range, 0x40 prox, 0x20 ready, 0x08 eraser, 0x04/0x02 btn, 0x01 tip
 *   [2..3] X le16, [4..5] Y le16, [6..7] pressure le16, [8] distance
 */
#include "kerntablet-plugin.h"
#include "kerntablet.h"

extern __u8 *hid_bpf_get_data(struct hid_bpf_ctx *ctx, unsigned int offset,
			      const kt_size_t __sz) __ksym;

/* ---- maps (pinned by the loader, written by the GUI) ---- */

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct kt_config);
} kt_config SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct kt_state);
} kt_state SEC(".maps");

KT_PIPE_MAP;

#define KT_REPORT_ID	0x02
#define KT_ST_PROX	0x40
#define KT_PKT_LEN	8

SEC("struct_ops/hid_device_event")
int BPF_PROG(kt_head_event, struct hid_bpf_ctx *hctx, enum hid_report_type rtype,
	     __u64 source)
{
	__u32 key = 0;
	struct kt_config *cfg;
	struct kt_state *st;
	struct kt_pipe *p;
	__u8 *data;

	p = bpf_map_lookup_elem(&kt_pipe, &key);
	if (!p)
		return 0;
	p->flags = 0;

	if (rtype != HID_INPUT_REPORT)
		return 0;
	data = hid_bpf_get_data(hctx, 0, KT_PKT_LEN);
	if (!data || data[0] != KT_REPORT_ID)
		return 0;

	st = bpf_map_lookup_elem(&kt_state, &key);

	if (!(data[1] & KT_ST_PROX)) {
		if (st && st->in_prox) {
			st->in_prox = 0;
			p->flags = KT_P_LEFT;
		}
		return 0;
	}

	cfg = bpf_map_lookup_elem(&kt_config, &key);
	if (!cfg)
		return 0;

	p->x = ((__s64)data[2] | ((__s64)data[3] << 8)) << KT_FIX_SHIFT;
	p->y = ((__s64)data[4] | ((__s64)data[5] << 8)) << KT_FIX_SHIFT;
	p->pressure = (__u32)data[6] | ((__u32)data[7] << 8);
	p->status = data[1];
	p->now_ns = bpf_ktime_get_ns();
	p->max_x = (__u32)cfg->max_x;
	p->max_y = (__u32)cfg->max_y;
	p->max_pressure = 2047;
	p->flags = KT_P_PEN;

	if (st) {
		st->events++;
		st->raw_x = (__u32)(p->x >> KT_FIX_SHIFT);
		st->raw_y = (__u32)(p->y >> KT_FIX_SHIFT);
		st->in_prox = 1;
	}
	return 0;
}

static __always_inline void kt_out_of_range(__u8 *data)
{
	data[1] = 0;
	data[2] = 0;
	data[3] = 0;
	data[4] = 0;
	data[5] = 0;
	data[6] = 0;
	data[7] = 0;
}

SEC("struct_ops/hid_device_event")
int BPF_PROG(kt_tail_event, struct hid_bpf_ctx *hctx, enum hid_report_type rtype,
	     __u64 source)
{
	__u32 key = 0;
	struct kt_config *cfg;
	struct kt_pipe *p;
	__s64 xq, yq, ox, oy;
	__u32 pressure;
	__u8 *data;
	int outside;

	p = bpf_map_lookup_elem(&kt_pipe, &key);
	if (!p || !(p->flags & KT_P_PEN))
		return 0;
	cfg = bpf_map_lookup_elem(&kt_config, &key);
	if (!cfg)
		return 0;
	if (cfg->magic != KT_MAGIC || !(cfg->flags & KT_F_ENABLED))
		return 0;

	data = hid_bpf_get_data(hctx, 0, KT_PKT_LEN);
	if (!data || data[0] != KT_REPORT_ID)
		return 0;

	if (p->flags & KT_P_DROP) {
		kt_out_of_range(data);
		return 0;
	}

	/* Position fed to the matrix, Q4 tablet units. */
	xq = p->x >> (KT_FIX_SHIFT - 4);
	yq = p->y >> (KT_FIX_SHIFT - 4);
	ox = (cfg->m[0] * xq + cfg->m[1] * yq + (cfg->m[2] << 4)) >> (KT_FIX_SHIFT + 4);
	oy = (cfg->m[3] * xq + cfg->m[4] * yq + (cfg->m[5] << 4)) >> (KT_FIX_SHIFT + 4);

	outside = ox < cfg->clip_min_x || ox > cfg->clip_max_x ||
		  oy < cfg->clip_min_y || oy > cfg->clip_max_y;

	if (outside && (cfg->flags & KT_F_LIMIT)) {
		/* Pen is outside the tablet area: make it look out of range. */
		kt_out_of_range(data);
		return 0;
	}

	if (cfg->flags & KT_F_CLIP) {
		if (ox < cfg->clip_min_x)
			ox = cfg->clip_min_x;
		if (ox > cfg->clip_max_x)
			ox = cfg->clip_max_x;
		if (oy < cfg->clip_min_y)
			oy = cfg->clip_min_y;
		if (oy > cfg->clip_max_y)
			oy = cfg->clip_max_y;
	}

	/* Never leave the range the kernel driver advertises. */
	if (ox < 0)
		ox = 0;
	if (ox > cfg->max_x)
		ox = cfg->max_x;
	if (oy < 0)
		oy = 0;
	if (oy > cfg->max_y)
		oy = cfg->max_y;

	pressure = p->pressure;
	if (pressure > 0xffff)
		pressure = 0xffff;

	data[2] = (__u8)(ox & 0xff);
	data[3] = (__u8)((ox >> 8) & 0xff);
	data[4] = (__u8)(oy & 0xff);
	data[5] = (__u8)((oy >> 8) & 0xff);
	data[6] = (__u8)(pressure & 0xff);
	data[7] = (__u8)((pressure >> 8) & 0xff);

	return 0;
}

/* The loader attaches kt_head first, then the plugins, then kt_tail. */
SEC(".struct_ops.link")
struct hid_bpf_ops kt_head = {
	.hid_device_event = (void *)kt_head_event,
};

SEC(".struct_ops.link")
struct hid_bpf_ops kt_tail = {
	.hid_device_event = (void *)kt_tail_event,
};

char _license[] SEC("license") = "GPL";
