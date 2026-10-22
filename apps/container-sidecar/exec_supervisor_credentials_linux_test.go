//go:build linux

package main

import (
	"io"
	"os"
	"os/user"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"golang.org/x/sys/unix"
)

func TestContainerCredentialNumericRange(t *testing.T) {
	for _, value := range []string{"0", "1", "65534", "2147483647", "00001", "+1"} {
		id, numeric, err := containerNumericID(value)
		expected, parseErr := strconv.ParseUint(value, 10, 31)
		if value == "+1" {
			expected, parseErr = 1, nil
		}
		if err != nil || parseErr != nil || !numeric || uint64(id) != expected {
			t.Fatalf("numeric value %q was not preserved", value)
		}
	}
	for _, value := range []string{"-1", "2147483648", "99999999999999999999999999"} {
		if _, _, err := containerNumericID(value); err == nil {
			t.Fatalf("out-of-range value %q was accepted", value)
		}
	}
	if _, numeric, err := containerNumericID("root"); numeric || err != nil {
		t.Fatal("image user names must use the image account lookup")
	}
}

// This payload runs only in the owned scratch container, never on the build host.
func execSupervisorRootCredentialPayload(t *testing.T) {
	if os.Geteuid() != 0 || os.Getpid() != 1 {
		t.Fatal("root credential payload requires the isolated main process")
	}
	header := unix.CapUserHeader{Version: unix.LINUX_CAPABILITY_VERSION_3}
	var before, after [2]unix.CapUserData
	if unix.Capget(&header, &before[0]) != nil || before[0].Permitted == 0 || before[1].Permitted != 0 {
		t.Fatal("root capability bound is unavailable")
	}
	for _, bit := range []uintptr{unix.CAP_NET_RAW, unix.CAP_NET_ADMIN, unix.CAP_SYS_ADMIN} {
		if bound, err := unix.PrctlRetInt(unix.PR_CAPBSET_READ, bit, 0, 0, 0); err != nil || bound != 0 {
			t.Fatal("fixture has an unexpected capability")
		}
	}
	if containerMain("0:0", []string{"/missing-fixture-command"}) != 125 {
		t.Fatal("missing main command was accepted")
	}
	executable, err := os.Executable()
	if err != nil {
		t.Fatal("credential fixture executable is unavailable")
	}
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{executable, "-test.run=^TestExecSupervisorPayload$"}}, "OC_EXEC_TEST_PAYLOAD=credential-process", "OC_EXEC_TEST_USER=65534:65534")
	birth := process.receipt(t)
	process.stdin.Close()
	terminal := process.terminal(t)
	stdout, outErr := io.ReadAll(process.stdout)
	stderr, errErr := io.ReadAll(process.stderr)
	if birth.PID <= 0 || !terminal.OK || terminal.PID != birth.PID || terminal.ExitCode == nil || *terminal.ExitCode != 0 || outErr != nil || errErr != nil || string(stdout) != "credential-process" || len(stderr) != 0 {
		t.Fatal("credential child lost its process identity, output, or exit proof")
	}
	process.waitWrapper(t)
	directory := t.TempDir()
	program := filepath.Join(directory, "missing-interpreter")
	if os.WriteFile(program, []byte("#!/missing-fixture-interpreter\n"), 0o500) != nil {
		t.Fatal("root-owned fixture creation failed")
	}
	if containerMain("65534:65534", []string{program}) != 125 || os.Geteuid() != 65534 || os.Getegid() != 65534 {
		t.Fatal("main credentials were not applied before exec")
	}
	if unix.Capget(&header, &after[0]) != nil || after[0].Effective != before[0].Permitted || after[0].Permitted != before[0].Permitted || after[0].Inheritable != before[0].Permitted || after[1].Effective != 0 {
		t.Fatal("main process exceeded or lost its initial capabilities")
	}
	if privileged, err := unix.PrctlRetInt(unix.PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0); err != nil || privileged != 1 {
		t.Fatal("no-new-privileges changed")
	}
	if _, err := os.ReadFile(program); err != nil {
		t.Fatal("non-root main lost its existing file access capability")
	}
	for bit := uintptr(0); bit < 32; bit++ {
		ambient, err := unix.PrctlRetInt(unix.PR_CAP_AMBIENT, unix.PR_CAP_AMBIENT_IS_SET, bit, 0, 0)
		if err != nil || ambient != int((before[bit/32].Permitted>>(bit%32))&1) {
			t.Fatal("ambient capabilities differ from the initial root bound")
		}
	}
	t.Log("ROOT_CREDENTIAL_BOUND_PASSED")
}

func TestContainerCredentialsPreserveExplicitPair(t *testing.T) {
	for _, value := range []struct {
		user     string
		uid, gid uint32
	}{{"0:0", 0, 0}, {"65534:65534", 65534, 65534}, {"2147483647:2147483647", 2147483647, 2147483647}, {":0", 0, 0}, {"00001:00002", 1, 2}} {
		credential, err := containerCredentials(value.user)
		if err != nil || credential.Uid != value.uid || credential.Gid != value.gid || len(credential.Groups) != 0 || credential.NoSetGroups {
			t.Fatalf("explicit pair %q was not preserved", value.user)
		}
	}
	for _, value := range []string{"bad-user-that-does-not-exist", "0:bad-group-that-does-not-exist", "0:0:0", "0\x00:0", "-1:0", "0:-1", "2147483648:0", "0:2147483648"} {
		if _, err := containerCredentials(value); err == nil {
			t.Fatalf("invalid image user %q was accepted", value)
		}
	}
}

func TestContainerCredentialsUseImageAccount(t *testing.T) {
	current, err := user.Current()
	if err != nil {
		t.Fatal(err)
	}
	for _, value := range []string{current.Username, current.Uid, current.Username + ":"} {
		credential, err := containerCredentials(value)
		if err != nil || strconv.FormatUint(uint64(credential.Uid), 10) != current.Uid || strconv.FormatUint(uint64(credential.Gid), 10) != current.Gid {
			t.Fatal("image account UID or GID changed")
		}
	}
	credential, err := containerCredentials("")
	if err != nil || credential.Uid != 0 {
		t.Fatal("empty image user must preserve root")
	}
}

func TestContainerCapabilitiesRejectUnprivilegedCaller(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Fatal("this owning unit batch requires an ordinary user")
	}
	if _, _, err := containerCapabilities(); err == nil {
		t.Fatal("an unprivileged caller acquired capabilities")
	}
	for _, user := range []string{"0:0", "-1:0"} {
		if execSupervisorRun(strings.Repeat("a", 64), user, nil, nil, nil) == nil {
			t.Fatal("invalid or unprivileged supervisor credential request was accepted")
		}
	}
	for _, value := range []struct {
		user string
		args []string
	}{{"0:0", nil}, {"0:0", []string{"/bin/true"}}, {"-1:0", []string{"/bin/true"}}} {
		if containerMain(value.user, value.args) != 125 {
			t.Fatal("invalid or unprivileged main launch was accepted")
		}
	}
}
