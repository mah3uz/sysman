// SPDX-License-Identifier: MIT OR GPL-2.0
#include <stdbool.h>
#include <linux/types.h>
#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_endian.h>

#define AF_INET 2
#define AF_INET6 10

// Only the fields read here; CO-RE finds them in the running kernel's layout through BTF.
struct in6_addr {
	union {
		__u32 u6_addr32[4];
	} in6_u;
} __attribute__((preserve_access_index));

struct sock_common {
	__be32 skc_daddr;
	__be32 skc_rcv_saddr;
	unsigned short skc_family;
	struct in6_addr skc_v6_daddr;
	struct in6_addr skc_v6_rcv_saddr;
} __attribute__((preserve_access_index));

struct sock {
	struct sock_common __sk_common;
} __attribute__((preserve_access_index));

struct msghdr {
	void *msg_name;
} __attribute__((preserve_access_index));

struct counts {
	__u64 rx;
	__u64 tx;
};

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 65536);
	__type(key, __u32);
	__type(value, struct counts);
} counts SEC(".maps");

static __always_inline bool v4_loopback(__be32 addr)
{
	return (bpf_ntohl(addr) >> 24) == 127;
}

static __always_inline bool v6_loopback(const __u32 *a)
{
	if (a[0] || a[1])
		return false;
	if (!a[2])
		return a[3] == bpf_htonl(1);
	return a[2] == bpf_htonl(0xffff) && v4_loopback(a[3]);
}

// A socket bound to or connected to a loopback address only ever talks to this machine.
static __always_inline bool sock_loopback(struct sock *sk)
{
	__u16 family = BPF_CORE_READ(sk, __sk_common.skc_family);
	struct in6_addr a = {};

	if (family == AF_INET)
		return v4_loopback(BPF_CORE_READ(sk, __sk_common.skc_daddr)) ||
		       v4_loopback(BPF_CORE_READ(sk, __sk_common.skc_rcv_saddr));
	if (family != AF_INET6)
		return false;
	bpf_core_read(&a, sizeof(a), &sk->__sk_common.skc_v6_daddr);
	if (v6_loopback(a.in6_u.u6_addr32))
		return true;
	bpf_core_read(&a, sizeof(a), &sk->__sk_common.skc_v6_rcv_saddr);
	return v6_loopback(a.in6_u.u6_addr32);
}

// An unconnected UDP socket names its peer per message, in a kernel copy of the sockaddr.
static __always_inline bool name_loopback(struct msghdr *msg)
{
	void *name = BPF_CORE_READ(msg, msg_name);
	__u16 family = 0;
	__u32 a[4] = {};

	if (!name || bpf_probe_read_kernel(&family, sizeof(family), name))
		return false;
	if (family == AF_INET)
		return !bpf_probe_read_kernel(&a[0], 4, name + 4) && v4_loopback(a[0]);
	if (family == AF_INET6)
		return !bpf_probe_read_kernel(a, 16, name + 8) && v6_loopback(a);
	return false;
}

static __always_inline void add(bool rx, long bytes)
{
	__u32 tgid = bpf_get_current_pid_tgid() >> 32;
	struct counts zero = {}, *c;

	if (bytes <= 0)
		return;
	c = bpf_map_lookup_elem(&counts, &tgid);
	if (!c) {
		bpf_map_update_elem(&counts, &tgid, &zero, BPF_NOEXIST);
		c = bpf_map_lookup_elem(&counts, &tgid);
		if (!c)
			return;
	}
	if (rx)
		__sync_fetch_and_add(&c->rx, bytes);
	else
		__sync_fetch_and_add(&c->tx, bytes);
}

SEC("fexit/tcp_sendmsg")
int BPF_PROG(tcp_sendmsg_exit, struct sock *sk, struct msghdr *msg, __u64 size, int ret)
{
	if (!sock_loopback(sk))
		add(false, ret);
	return 0;
}

// Called once per recvmsg with the bytes copied to the reader, in the reader's context.
SEC("fexit/tcp_cleanup_rbuf")
int BPF_PROG(tcp_cleanup_rbuf_exit, struct sock *sk, int copied)
{
	if (!sock_loopback(sk))
		add(true, copied);
	return 0;
}

SEC("fexit/udp_sendmsg")
int BPF_PROG(udp_sendmsg_exit, struct sock *sk, struct msghdr *msg, __u64 len, int ret)
{
	if (!sock_loopback(sk) && !name_loopback(msg))
		add(false, ret);
	return 0;
}

SEC("fexit/udp_recvmsg")
int BPF_PROG(udp_recvmsg_exit, struct sock *sk, struct msghdr *msg, __u64 len, int flags, int *addr_len, int ret)
{
	if (!sock_loopback(sk) && !name_loopback(msg))
		add(true, ret);
	return 0;
}

SEC("fexit/udpv6_sendmsg")
int BPF_PROG(udpv6_sendmsg_exit, struct sock *sk, struct msghdr *msg, __u64 len, int ret)
{
	if (!sock_loopback(sk) && !name_loopback(msg))
		add(false, ret);
	return 0;
}

SEC("fexit/udpv6_recvmsg")
int BPF_PROG(udpv6_recvmsg_exit, struct sock *sk, struct msghdr *msg, __u64 len, int flags, int *addr_len, int ret)
{
	if (!sock_loopback(sk) && !name_loopback(msg))
		add(true, ret);
	return 0;
}

// Dropping a process's entry when it exits keeps the map bounded and a reused pid starting from zero.
SEC("tp_btf/sched_process_exit")
int process_exit(void *ctx)
{
	__u64 id = bpf_get_current_pid_tgid();
	__u32 tgid = id >> 32;

	if ((__u32)id == tgid)
		bpf_map_delete_elem(&counts, &tgid);
	return 0;
}

char LICENSE[] SEC("license") = "Dual MIT/GPL";
