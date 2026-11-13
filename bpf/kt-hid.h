/* SPDX-License-Identifier: GPL-2.0-only */
/* Minimal mirrors of the kernel's HID-BPF types, matched against kernel BTF
 * by name. Shared by the core program and plugins. */
#ifndef KT_HID_H
#define KT_HID_H

#include <linux/bpf.h>
#include <linux/types.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

typedef unsigned long kt_size_t;

struct hid_device;

struct list_head {
	struct list_head *next, *prev;
};

enum hid_report_type {
	HID_INPUT_REPORT = 0,
	HID_OUTPUT_REPORT = 1,
	HID_FEATURE_REPORT = 2,
};

enum hid_class_request {
	HID_REQ_GET_REPORT = 0x01,
	HID_REQ_GET_IDLE = 0x02,
	HID_REQ_GET_PROTOCOL = 0x03,
	HID_REQ_SET_REPORT = 0x09,
	HID_REQ_SET_IDLE = 0x0A,
	HID_REQ_SET_PROTOCOL = 0x0B,
};

struct hid_bpf_ctx {
	struct hid_device *hid;
	__u32 allocated_size;
	union {
		__s32 retval;
		__s32 size;
	};
} __attribute__((preserve_access_index));

struct hid_bpf_ops {
	int hid_id;
	__u32 flags;
	struct list_head list;
	int (*hid_device_event)(struct hid_bpf_ctx *ctx, enum hid_report_type report_type,
				__u64 source);
	int (*hid_rdesc_fixup)(struct hid_bpf_ctx *ctx);
	int (*hid_hw_request)(struct hid_bpf_ctx *ctx, unsigned char reportnum,
			      enum hid_report_type rtype, enum hid_class_request reqtype,
			      __u64 source);
	int (*hid_hw_output_report)(struct hid_bpf_ctx *ctx, __u64 source);
	struct hid_device *hdev;
};

extern __u8 *hid_bpf_get_data(struct hid_bpf_ctx *ctx, unsigned int offset,
			      const kt_size_t __sz) __ksym;

#endif
