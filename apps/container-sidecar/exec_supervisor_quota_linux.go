//go:build linux

package main

import (
	"errors"
	"runtime"
	"unsafe"

	"golang.org/x/sys/unix"
)

// Filesystem owners can change XFS project IDs without CAP_SYS_ADMIN.
// Stack this small filter on Docker's profile before executing tenant code.
func containerQuotaFilter() error {
	var native, compat, ioctl uint32
	switch runtime.GOARCH {
	case "amd64":
		native, compat, ioctl = unix.AUDIT_ARCH_X86_64, unix.AUDIT_ARCH_I386, 16
	case "arm64":
		native, compat, ioctl = unix.AUDIT_ARCH_AARCH64, unix.AUDIT_ARCH_ARM, 29
	default:
		return errors.New("CONTAINER_QUOTA_FILTER_UNAVAILABLE")
	}
	filters := containerQuotaRules(native, compat, ioctl)
	program := unix.SockFprog{Len: uint16(len(filters)), Filter: &filters[0]}
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	if unix.Prctl(unix.PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != nil {
		return errors.New("CONTAINER_QUOTA_FILTER_UNAVAILABLE")
	}
	thread, _, errno := unix.Syscall(unix.SYS_SECCOMP, unix.SECCOMP_SET_MODE_FILTER,
		unix.SECCOMP_FILTER_FLAG_TSYNC, uintptr(unsafe.Pointer(&program)))
	runtime.KeepAlive(filters)
	if errno != 0 || thread != 0 {
		return errors.New("CONTAINER_QUOTA_FILTER_UNAVAILABLE")
	}
	return nil
}

func containerQuotaRules(native, compat, ioctl uint32) []unix.SockFilter {
	return []unix.SockFilter{
		{Code: unix.BPF_LD | unix.BPF_W | unix.BPF_ABS, K: 4},
		{Code: unix.BPF_JMP | unix.BPF_JEQ | unix.BPF_K, K: native, Jf: 4},
		{Code: unix.BPF_LD | unix.BPF_W | unix.BPF_ABS, K: 0},
		{Code: unix.BPF_ALU | unix.BPF_AND | unix.BPF_K, K: 0xbfffffff}, // x32 syscall bit
		{Code: unix.BPF_JMP | unix.BPF_JEQ | unix.BPF_K, K: ioctl, Jt: 6},
		{Code: unix.BPF_RET | unix.BPF_K, K: unix.SECCOMP_RET_ALLOW},
		{Code: unix.BPF_JMP | unix.BPF_JEQ | unix.BPF_K, K: compat, Jf: 3},
		{Code: unix.BPF_LD | unix.BPF_W | unix.BPF_ABS, K: 0},
		{Code: unix.BPF_JMP | unix.BPF_JEQ | unix.BPF_K, K: 54, Jt: 2},
		{Code: unix.BPF_RET | unix.BPF_K, K: unix.SECCOMP_RET_ALLOW},
		{Code: unix.BPF_RET | unix.BPF_K, K: unix.SECCOMP_RET_ERRNO | uint32(unix.ENOSYS)},
		{Code: unix.BPF_LD | unix.BPF_W | unix.BPF_ABS, K: 24},                 // ioctl request, low 32 bits
		{Code: unix.BPF_JMP | unix.BPF_JEQ | unix.BPF_K, K: 0x401c5820, Jt: 3}, // FSSETXATTR
		{Code: unix.BPF_JMP | unix.BPF_JEQ | unix.BPF_K, K: 0x40086602, Jt: 2}, // SETFLAGS, 64-bit
		{Code: unix.BPF_JMP | unix.BPF_JEQ | unix.BPF_K, K: 0x40046602, Jt: 1}, // SETFLAGS, compat
		{Code: unix.BPF_RET | unix.BPF_K, K: unix.SECCOMP_RET_ALLOW},
		{Code: unix.BPF_RET | unix.BPF_K, K: unix.SECCOMP_RET_ERRNO | uint32(unix.EPERM)},
	}
}
