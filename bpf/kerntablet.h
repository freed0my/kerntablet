/* SPDX-License-Identifier: GPL-2.0-only */
/* Core ABI between the BPF program and userspace (mirrored in
 * crates/common/src/kernel.rs — keep both in sync). The plugin ABI is in
 * kerntablet-plugin.h. */
#ifndef KERNTABLET_H
#define KERNTABLET_H

#define KT_MAGIC	0x3342544b /* "KTB3" little endian */
#define KT_FIX_SHIFT	20

#define KT_F_ENABLED	(1u << 0) /* 0 = pass reports through untouched */
#define KT_F_CLIP	(1u << 1) /* clamp to the screen area */
#define KT_F_LIMIT	(1u << 2) /* outside the tablet area = pen out of range */

struct kt_config {
	__u32 magic;
	__u32 flags;
	/* Input is the position in Q4 tablet units (xq, yq):
	 * out_x = (m[0]*xq + m[1]*yq + (m[2] << 4)) >> (KT_FIX_SHIFT + 4)
	 * out_y = (m[3]*xq + m[4]*yq + (m[5] << 4)) >> (KT_FIX_SHIFT + 4) */
	__s64 m[6];
	/* Screen area expressed in tablet output units (inclusive). */
	__s32 clip_min_x;
	__s32 clip_min_y;
	__s32 clip_max_x;
	__s32 clip_max_y;
	/* Logical maximum of the device axes. */
	__s32 max_x;
	__s32 max_y;
};

struct kt_state {
	__u64 events;	/* pen reports seen */
	__u32 raw_x;	/* last untransformed position */
	__u32 raw_y;
	__u32 in_prox;
	__u32 _pad;
};

#endif
