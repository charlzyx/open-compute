//go:build linux

package main

import (
	"os"
	"os/exec"
	"runtime"
	"sync"
	"testing"
	"unsafe"

	"golang.org/x/sys/unix"
)

func TestContainerQuotaFilterPayload(t *testing.T) {
	if os.Getenv("OC_QUOTA_FILTER_PAYLOAD") != "1" {
		return
	}
	if containerQuotaFilter() != nil {
		t.Fatal("quota filter installation failed")
	}
	file, err := os.Open("/dev/null")
	if err != nil {
		t.Fatal("control descriptor unavailable")
	}
	defer file.Close()
	var workers sync.WaitGroup
	for range 8 {
		workers.Go(func() {
			runtime.LockOSThread()
			defer runtime.UnlockOSThread()
			for _, request := range []uintptr{0x401c5820, 0x40086602, 0x40046602} {
				if _, _, errno := unix.Syscall(unix.SYS_IOCTL, file.Fd(), request, 0); errno != unix.EPERM {
					t.Error("a quota mutation escaped the thread filter")
				}
			}
			if _, _, errno := unix.Syscall(unix.SYS_IOCTL, file.Fd(), 0, 0); errno != unix.ENOTTY {
				t.Error("an unrelated ioctl was changed")
			}
		})
	}
	workers.Wait()
	if exec.Command("/bin/true").Run() != nil {
		t.Fatal("ordinary subprocess execution was changed")
	}
}

func TestContainerQuotaFilterIsInheritedByAllThreads(t *testing.T) {
	command := exec.Command(os.Args[0], "-test.run=^TestContainerQuotaFilterPayload$")
	command.Env = append(os.Environ(), "OC_QUOTA_FILTER_PAYLOAD=1")
	if directory := os.Getenv("GOCOVERDIR"); directory != "" {
		command.Args = append(command.Args, "-test.gocoverdir="+directory)
	}
	if output, err := command.CombinedOutput(); err != nil {
		t.Fatalf("quota filter subprocess failed: %v, %s", err, output)
	}
}

func TestContainerQuotaMutationPayload(t *testing.T) {
	mode := os.Getenv("OC_QUOTA_MUTATION_PAYLOAD")
	if mode == "" {
		return
	}
	fd, err := unix.Open("/restore", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW, 0)
	if err != nil {
		t.Fatal("owned directory mount unavailable")
	}
	defer unix.Close(fd)
	var attributes [7]uint32
	if _, _, errno := unix.Syscall(unix.SYS_IOCTL, uintptr(fd), 0x801c581f, uintptr(unsafe.Pointer(&attributes))); errno != 0 || attributes[3] == 0 || attributes[0]&0x200 == 0 {
		t.Fatal("owned directory has no inherited project")
	}
	original := attributes
	attributes[3] = 0
	attributes[0] &^= 0x200
	_, _, errno := unix.Syscall(unix.SYS_IOCTL, uintptr(fd), 0x401c5820, uintptr(unsafe.Pointer(&attributes)))
	if mode == "filtered" {
		if errno != unix.EPERM {
			t.Fatal("tenant changed the directory quota")
		}
	} else if mode == "unfiltered-control" {
		if errno != 0 {
			t.Fatal("control did not expose the unfiltered ownership boundary")
		}
		if _, _, errno := unix.Syscall(unix.SYS_IOCTL, uintptr(fd), 0x401c5820, uintptr(unsafe.Pointer(&original))); errno != 0 {
			t.Fatal("owned control quota restoration failed")
		}
	} else {
		t.Fatal("unknown mutation fixture mode")
	}
	attributes = [7]uint32{}
	if _, _, errno := unix.Syscall(unix.SYS_IOCTL, uintptr(fd), 0x801c581f, uintptr(unsafe.Pointer(&attributes))); errno != 0 || attributes != original {
		t.Fatal("directory quota identity changed")
	}
}

func TestContainerQuotaRulesCoverNativeCompatAndX32(t *testing.T) {
	for _, platform := range []struct{ native, compat, ioctl uint32 }{
		{unix.AUDIT_ARCH_X86_64, unix.AUDIT_ARCH_I386, 16},
		{unix.AUDIT_ARCH_AARCH64, unix.AUDIT_ARCH_ARM, 29},
	} {
		instructions := containerQuotaRules(platform.native, platform.compat, platform.ioctl)
		for _, abi := range []struct{ arch, syscall uint32 }{
			{platform.native, platform.ioctl}, {platform.compat, 54},
			{platform.native, platform.ioctl | 0x40000000},
		} {
			for _, request := range []uint32{0x401c5820, 0x40086602, 0x40046602, 0, 0x801c581f} {
				data := map[uint32]uint32{0: abi.syscall, 4: abi.arch, 24: request}
				expected := uint32(unix.SECCOMP_RET_ALLOW)
				if request == 0x401c5820 || request == 0x40086602 || request == 0x40046602 {
					expected = unix.SECCOMP_RET_ERRNO | uint32(unix.EPERM)
				}
				if actual := evaluateQuotaFilter(t, instructions, data); actual != expected {
					t.Fatalf("ABI %x request %x returned %x, want %x", abi.arch, request, actual, expected)
				}
			}
		}
		for _, arch := range []uint32{platform.native, platform.compat} {
			if actual := evaluateQuotaFilter(t, instructions, map[uint32]uint32{0: 1000, 4: arch}); actual != unix.SECCOMP_RET_ALLOW {
				t.Fatal("unrelated syscall was changed")
			}
		}
		if actual := evaluateQuotaFilter(t, instructions, map[uint32]uint32{4: 0}); actual != unix.SECCOMP_RET_ERRNO|uint32(unix.ENOSYS) {
			t.Fatal("unknown syscall ABI was admitted")
		}
	}
}

func evaluateQuotaFilter(t *testing.T, instructions []unix.SockFilter, data map[uint32]uint32) uint32 {
	t.Helper()
	var accumulator uint32
	for index := 0; index < len(instructions); index++ {
		instruction := instructions[index]
		switch instruction.Code {
		case unix.BPF_LD | unix.BPF_W | unix.BPF_ABS:
			accumulator = data[instruction.K]
		case unix.BPF_ALU | unix.BPF_AND | unix.BPF_K:
			accumulator &= instruction.K
		case unix.BPF_JMP | unix.BPF_JEQ | unix.BPF_K:
			if accumulator == instruction.K {
				index += int(instruction.Jt)
			} else {
				index += int(instruction.Jf)
			}
		case unix.BPF_RET | unix.BPF_K:
			return instruction.K
		default:
			t.Fatal("unsupported quota filter instruction")
		}
	}
	t.Fatal("quota filter has no terminal verdict")
	return 0
}

func TestContainerQuotaFilterFailsClosedWhenKernelRejectsInstallation(t *testing.T) {
	for _, mode := range []string{"seccomp", "no-new-privs"} {
		command := exec.Command(os.Args[0], "-test.run=^TestContainerQuotaFilterDeniedPayload$")
		command.Env = append(os.Environ(), "OC_QUOTA_DENIAL_PAYLOAD="+mode)
		if directory := os.Getenv("GOCOVERDIR"); directory != "" {
			command.Args = append(command.Args, "-test.gocoverdir="+directory)
		}
		if output, err := command.CombinedOutput(); err != nil {
			t.Fatalf("denied filter subprocess failed: %v, %s", err, output)
		}
	}
}

func TestContainerQuotaFilterDeniedPayload(t *testing.T) {
	mode := os.Getenv("OC_QUOTA_DENIAL_PAYLOAD")
	if mode == "" {
		return
	}
	denied := uint32(unix.SYS_SECCOMP)
	if mode == "no-new-privs" {
		denied = unix.SYS_PRCTL
	}
	instructions := []unix.SockFilter{
		{Code: unix.BPF_LD | unix.BPF_W | unix.BPF_ABS, K: 0},
		{Code: unix.BPF_JMP | unix.BPF_JEQ | unix.BPF_K, K: denied, Jf: 1},
		{Code: unix.BPF_RET | unix.BPF_K, K: unix.SECCOMP_RET_ERRNO | uint32(unix.EPERM)},
		{Code: unix.BPF_RET | unix.BPF_K, K: unix.SECCOMP_RET_ALLOW},
	}
	program := unix.SockFprog{Len: uint16(len(instructions)), Filter: &instructions[0]}
	runtime.LockOSThread()
	defer runtime.UnlockOSThread()
	if unix.Prctl(unix.PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != nil {
		t.Fatal("denial fixture setup failed")
	}
	thread, _, errno := unix.Syscall(unix.SYS_SECCOMP, unix.SECCOMP_SET_MODE_FILTER, unix.SECCOMP_FILTER_FLAG_TSYNC, uintptr(unsafe.Pointer(&program)))
	runtime.KeepAlive(instructions)
	if thread != 0 || errno != 0 {
		t.Fatal("denial fixture filter failed")
	}
	if containerQuotaFilter() == nil || containerMain("0:0", []string{"/bin/true"}) != 125 || runExecSupervisor("untrusted", "") != 125 {
		t.Fatal("tenant execution continued without the quota filter")
	}
}
