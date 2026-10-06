/* The eBPF program of the guest tests, built by testbed/launch.sh with the image's clang (-target bpf) before the
 * guest boots, so the slow emulated guest only has to load it. A cgroup v2 device filter: how cgroup v2 isolates
 * devices (it has no devices controller; the policy is a BPF program attached to the cgroup).
 * It refuses character device 1:3 (/dev/null) and allows everything else. */
#include <linux/bpf.h>
#define SEC(n) __attribute__((section(n), used))

SEC("cgroup/dev") int refuse_null(struct bpf_cgroup_dev_ctx *ctx) {
	if ((ctx->access_type & 0xffff) == BPF_DEVCG_DEV_CHAR && ctx->major == 1 && ctx->minor == 3)
		return 0;
	return 1;
}

char _license[] SEC("license") = "GPL";
