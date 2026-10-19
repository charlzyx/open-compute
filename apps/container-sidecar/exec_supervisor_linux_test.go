//go:build linux

package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

func TestExecSupervisorHarness(t *testing.T) {
	identity := os.Getenv("OC_EXEC_TEST_SUPERVISOR")
	if identity == "" {
		return
	}
	if os.Getenv("OC_EXEC_TEST_CLOSE") == "stdout" {
		unix.Close(1)
	}
	if os.Getenv("OC_EXEC_TEST_CLOSE") == "stderr" {
		unix.Close(2)
	}
	code := runExecSupervisor(identity)

	os.Exit(code)
}

func TestExecSupervisorPayload(t *testing.T) {
	switch os.Getenv("OC_EXEC_TEST_PAYLOAD") {
	case "shellless":
		fmt.Fprint(os.Stdout, "shellless-readonly")
		os.Exit(0)
	case "descendant":
		command := exec.Command(os.Args[0], "-test.run=^TestExecSupervisorPayload$")
		command.Env = append(os.Environ(), "OC_EXEC_TEST_PAYLOAD=grandchild")
		command.Stdout, command.Stderr = os.Stdout, os.Stderr
		if command.Start() != nil {
			os.Exit(90)
		}
		os.Exit(7)
	case "grandchild":
		time.Sleep(700 * time.Millisecond)
		fmt.Fprint(os.Stdout, "descendant-out")
		fmt.Fprint(os.Stderr, "descendant-err")
		os.Exit(0)
	case "raw-pty":
		termios, err := unix.IoctlGetTermios(0, unix.TCGETS)
		if err != nil {
			os.Exit(94)
		}
		termios.Lflag &^= unix.ICANON | unix.ECHO
		termios.Cc[unix.VMIN], termios.Cc[unix.VTIME] = 1, 0
		if unix.IoctlSetTermios(0, unix.TCSETS, termios) != nil {
			os.Exit(95)
		}
		fmt.Fprintln(os.Stdout, "RAW_PTY_READY")
		var body [3]byte
		if _, err := io.ReadFull(os.Stdin, body[:]); err != nil || string(body[:]) != "raw" {
			os.Exit(96)
		}
		count, err := unix.Poll([]unix.PollFd{{Fd: 0, Events: unix.POLLIN}}, 100)
		if err != nil || count != 0 {
			os.Exit(97)
		}
		fmt.Fprintln(os.Stdout, "RAW_EOF_PRESERVES_MASTER")
		os.Exit(0)
	case "pty":
		if _, err := unix.IoctlGetTermios(0, unix.TCGETS); err != nil {
			os.Exit(91)
		}
		fmt.Fprintln(os.Stdout, "PTY_READY")
		body, err := io.ReadAll(os.Stdin)
		if err != nil || string(body) != "without-newline" {
			os.Exit(92)
		}
		size, err := unix.IoctlGetWinsize(1, unix.TIOCGWINSZ)
		if err != nil || size.Row != 40 || size.Col != 100 {
			os.Exit(93)
		}
		fmt.Fprintln(os.Stdout, "PTY_EOF_RESIZED")
		os.Exit(0)
	}
}

type execSupervisorTestProcess struct {
	command    *exec.Cmd
	stdin      io.WriteCloser
	stdout     *bufio.Reader
	outputPipe io.ReadCloser
	stderr     io.ReadCloser
	identity   string
	key        [32]byte
	pidfd      int
	done       chan error
	owner      *execSupervisorConnection
	started    *execControlResponse
}

func execSupervisorTestStart(t *testing.T, start execSupervisorStart, extraEnvironment ...string) *execSupervisorTestProcess {
	t.Helper()
	var random [32]byte
	if _, err := rand.Read(random[:]); err != nil {
		t.Fatal(err)
	}
	process := &execSupervisorTestProcess{identity: hex.EncodeToString(random[:]), pidfd: -1, done: make(chan error, 1)}
	if _, err := rand.Read(process.key[:]); err != nil {
		t.Fatal(err)
	}
	command := exec.Command(os.Args[0], "-test.run=^TestExecSupervisorHarness$")
	command.Env = append(os.Environ(), append(extraEnvironment, "OC_EXEC_TEST_SUPERVISOR="+process.identity)...)
	command.SysProcAttr = &syscall.SysProcAttr{PidFD: &process.pidfd}
	input, err := command.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	output, err := command.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	stderr, err := command.StderrPipe()
	if err != nil {
		t.Fatal(err)
	}
	process.command, process.stdin, process.stdout, process.stderr = command, input, bufio.NewReader(output), stderr
	process.outputPipe = output
	if err := command.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		ctx, cancel := context.WithTimeout(context.Background(), time.Second)
		_, _ = requestExecSupervisor(ctx, process.identity, process.key[:], execControlRequest{Op: "signal", Signal: 9})
		cancel()
		if process.owner != nil {
			process.owner.Close()
		}
		input.Close()
		if process.pidfd >= 0 {
			_ = unix.PidfdSendSignal(process.pidfd, unix.SIGKILL, nil, 0)
			_ = unix.Close(process.pidfd)
			process.pidfd = -1
		}
		if command.ProcessState == nil {
			_ = command.Wait()
		}
		output.Close()
		stderr.Close()
	})
	ready := make(chan string, 1)
	go func() { line, _ := process.stdout.ReadString('\n'); ready <- line }()
	select {
	case line := <-ready:
		if line != execSupervisorReady {
			t.Fatalf("missing READY: %q", line)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("READY deadline")
	}
	if err := execSupervisorWriteAll(input, process.key[:]); err != nil {
		t.Fatal(err)
	}
	if err := execSupervisorFrameWrite(input, start); err != nil {
		t.Fatal(err)
	}
	return process
}

func (process *execSupervisorTestProcess) call(t *testing.T, request execControlRequest) execControlResponse {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	response, err := requestExecSupervisor(ctx, process.identity, process.key[:], request)
	if err != nil {
		t.Fatalf("control failed: %v", err)
	}
	return response
}
func (process *execSupervisorTestProcess) receipt(t *testing.T) execControlResponse {
	t.Helper()
	if process.started != nil {
		return *process.started
	}
	owner, err := openExecSupervisor(context.Background(), process.identity, process.key[:], execControlRequest{Op: "run"})
	if err != nil {
		t.Fatal(err)
	}
	process.owner = owner
	_ = owner.SetReadDeadline(time.Now().Add(5 * time.Second))
	var response execControlResponse
	if execSupervisorControlRead(owner, &response) != nil {
		t.Fatal("missing birth response")
	}
	process.started = &response
	return response
}
func (process *execSupervisorTestProcess) terminal(t *testing.T) execControlResponse {
	t.Helper()
	if process.owner == nil {
		t.Fatal("run not claimed")
	}
	_ = process.owner.SetReadDeadline(time.Now().Add(5 * time.Second))
	var response execControlResponse
	if err := execSupervisorControlRead(process.owner, &response); err != nil {
		t.Fatal(err)
	}
	return response
}

func (process *execSupervisorTestProcess) waitWrapper(t *testing.T) {
	t.Helper()
	if process.owner != nil {
		var drained execControlResponse
		if execSupervisorControlRead(process.owner, &drained) != nil || !drained.OK || drained.State != "drained" || drained.Code != "" {
			t.Fatal("missing positive drain proof")
		}
		var extra [1]byte
		if count, err := process.owner.Read(extra[:]); count != 0 || err != io.EOF {
			t.Fatal("unexpected trailing protocol bytes")
		}
	}
	go func() { process.done <- process.command.Wait() }()
	select {
	case err := <-process.done:
		if err != nil {
			t.Fatalf("wrapper failed: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("wrapper completion deadline")
	}
}

func TestExecSupervisorFastExitPublishesBirthTerminalAndCompleteOutput(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/sh", "-c", "printf payload; printf error >&2; exit 7"}})
	info := process.receipt(t)
	process.stdin.Close()
	time.Sleep(40 * time.Millisecond)
	if info.State != "running" || info.ExitCode != nil || info.PID <= 0 || info.StartTicks == 0 {
		t.Fatal("invalid birth")
	}
	wait := process.terminal(t)
	if !wait.OK || wait.PID != info.PID || wait.StartTicks != info.StartTicks || wait.ExitCode == nil || *wait.ExitCode != 7 {
		t.Fatalf("terminal lost child identity: %+v", wait)
	}
	stdout, err := io.ReadAll(process.stdout)
	if err != nil {
		t.Fatal(err)
	}
	stderr, err := io.ReadAll(process.stderr)
	if err != nil {
		t.Fatal(err)
	}
	if string(stdout) != "payload" || string(stderr) != "error" {
		t.Fatal("output truncated")
	}
	process.waitWrapper(t)
}

func TestExecSupervisorRawSignalsAndLateSignal(t *testing.T) {
	for _, signal := range []int{15, 32, 33, 34, 64} {
		t.Run(fmt.Sprint(signal), func(t *testing.T) {
			process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/sleep", "30"}})
			info := process.receipt(t)
			stat, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", info.PID))
			if err != nil {
				t.Fatal(err)
			}
			fields := bytes.Fields(stat[bytes.LastIndexByte(stat, ')')+1:])
			if string(fields[1]) != fmt.Sprint(process.command.Process.Pid) {
				t.Fatal("control targets wrapper rather than direct child")
			}
			for _, invalid := range []int{0, 65} {
				if process.call(t, execControlRequest{Op: "signal", Signal: invalid}).Code != "INVALID_SIGNAL" {
					t.Fatal("invalid signal accepted")
				}
			}
			if !process.call(t, execControlRequest{Op: "signal", Signal: signal}).OK {
				t.Fatal("signal rejected")
			}
			wait := process.terminal(t)
			if wait.ExitCode == nil || *wait.ExitCode != 128+signal || wait.PID != info.PID {
				t.Fatalf("signal substituted: %+v", wait)
			}
			process.stdin.Close()
			_, _ = io.ReadAll(process.stdout)
			_, _ = io.ReadAll(process.stderr)
			process.waitWrapper(t)
			ctx, cancel := context.WithTimeout(context.Background(), time.Second)
			defer cancel()
			if _, err := requestExecSupervisor(ctx, process.identity, process.key[:], execControlRequest{Op: "signal", Signal: 9}); err == nil {
				t.Fatal("late signal reached a replacement")
			}
		})
	}
}

func TestExecSupervisorExitDoesNotWaitForDescendantOutput(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{os.Args[0], "-test.run=^TestExecSupervisorPayload$"}}, "OC_EXEC_TEST_PAYLOAD=descendant")
	info := process.receipt(t)
	start := time.Now()
	wait := process.terminal(t)
	if time.Since(start) > 500*time.Millisecond || wait.ExitCode == nil || *wait.ExitCode != 7 || wait.PID != info.PID {
		t.Fatalf("exit waited for EOF: %+v", wait)
	}
	if err := unix.PidfdSendSignal(process.pidfd, 0, nil, 0); err != nil {
		t.Fatal("wrapper exited before descendant EOF")
	}
	stdout, err := io.ReadAll(process.stdout)
	if err != nil {
		t.Fatal(err)
	}
	stderr, err := io.ReadAll(process.stderr)
	if err != nil {
		t.Fatal(err)
	}
	if string(stdout) != "descendant-out" || string(stderr) != "descendant-err" {
		t.Fatal("descendant output lost")
	}
	process.stdin.Close()
	process.waitWrapper(t)
}

func TestExecSupervisorPTYResizeAndPartialLineEOF(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{os.Args[0], "-test.run=^TestExecSupervisorPayload$"}, Pty: true, Rows: 24, Cols: 80}, "OC_EXEC_TEST_PAYLOAD=pty")
	process.receipt(t)
	line, err := process.stdout.ReadString('\n')
	if err != nil || strings.TrimSpace(line) != "PTY_READY" {
		t.Fatal("payload did not inherit PTY")
	}
	if process.call(t, execControlRequest{Op: "resize", Rows: 0, Cols: 100}).Code != "INVALID_PTY_SIZE" {
		t.Fatal("invalid resize accepted")
	}
	if !process.call(t, execControlRequest{Op: "resize", Rows: 40, Cols: 100}).OK {
		t.Fatal("resize failed")
	}
	if _, err := io.WriteString(process.stdin, "without-newline"); err != nil {
		t.Fatal(err)
	}
	process.stdin.Close()
	wait := process.terminal(t)
	if wait.ExitCode == nil || *wait.ExitCode != 0 {
		t.Fatalf("PTY failed: %+v", wait)
	}
	stdout, err := io.ReadAll(process.stdout)
	if err != nil || !bytes.Contains(stdout, []byte("PTY_EOF_RESIZED")) {
		t.Fatal("PTY EOF discarded output")
	}
	stderr, _ := io.ReadAll(process.stderr)
	if len(stderr) != 0 {
		t.Fatal("PTY stderr split")
	}
	process.waitWrapper(t)
}

func TestExecSupervisorSpawnFailureHasNoChildTerminal(t *testing.T) {
	malformed := filepath.Join(t.TempDir(), "invalid-executable")
	if err := os.WriteFile(malformed, []byte("not an executable format"), 0700); err != nil {
		t.Fatal(err)
	}
	for _, argv := range [][]string{{"/open-compute-test-no-such-executable"}, {"/"}, {}, {malformed}} {
		process := execSupervisorTestStart(t, execSupervisorStart{Argv: argv})
		inspect := process.receipt(t)
		if inspect.OK || inspect.State != "spawn-failed" || inspect.PID != 0 || inspect.ExitCode != nil {
			t.Fatalf("spawn failure invented child: %+v", inspect)
		}
		process.stdin.Close()
		_, _ = io.ReadAll(process.stdout)
		_, _ = io.ReadAll(process.stderr)
		if err := process.command.Wait(); err == nil {
			t.Fatal("spawn failure wrapper exit was successful")
		} else if failure, ok := err.(*exec.ExitError); !ok || failure.ExitCode() != 125 {
			t.Fatal("incorrect fixed spawn failure exit")
		}
	}
}

func TestExecSupervisorInheritedEnvironmentWorkingDirectoryAndStdin(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/sh", "-c", "printf '%s|' \"$OC_EXEC_TEST_ENV\"; pwd; cat"}}, "OC_EXEC_TEST_ENV=engine-value")
	process.receipt(t)
	if _, err := process.stdin.Write([]byte("only-child-input")); err != nil {
		t.Fatal(err)
	}
	process.stdin.Close()
	wait := process.terminal(t)
	if wait.ExitCode == nil || *wait.ExitCode != 0 {
		t.Fatal("child failed")
	}
	stdout, _ := io.ReadAll(process.stdout)
	cwd, _ := os.Getwd()
	if string(stdout) != "engine-value|"+cwd+"\nonly-child-input" || bytes.Contains(stdout, process.key[:]) {
		t.Fatal("environment/cwd/bootstrap boundary changed")
	}
	_, _ = io.ReadAll(process.stderr)
	process.waitWrapper(t)
}

func TestExecSupervisorWrongKeyDuplicateRunAndCanceledControl(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/sleep", "30"}})
	info := process.receipt(t)
	wrong := process.key
	wrong[0] ^= 255
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	if _, err := requestExecSupervisor(ctx, process.identity, wrong[:], execControlRequest{Op: "signal", Signal: 9}); err == nil {
		t.Fatal("wrong key")
	}
	cancel()
	for index := 0; index < 20; index++ {
		ctx, cancel := context.WithCancel(context.Background())
		cancel()
		if _, err := openExecSupervisor(ctx, process.identity, process.key[:], execControlRequest{Op: "run"}); err == nil {
			t.Fatal("canceled control")
		}
	}
	duplicate := process.call(t, execControlRequest{Op: "run"})
	if duplicate.OK || duplicate.Code != "RUN_UNAVAILABLE" {
		t.Fatal("double run")
	}
	for _, op := range []string{"inspect", "wait", "drain", "unknown"} {
		if process.call(t, execControlRequest{Op: op}).Code != "INVALID_REQUEST" {
			t.Fatal("retired operation accepted")
		}
	}
	if !process.call(t, execControlRequest{Op: "signal", Signal: 9}).OK {
		t.Fatal("signal")
	}
	terminal := process.terminal(t)
	if terminal.PID != info.PID || terminal.ExitCode == nil || *terminal.ExitCode != 137 {
		t.Fatal("wrong terminal")
	}
	process.stdin.Close()
	io.ReadAll(process.stdout)
	io.ReadAll(process.stderr)
	process.waitWrapper(t)
}

func TestExecSupervisorWrapperDeathAndEndpointRebindingCannotForgeTerminal(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/sleep", "30"}})
	info := process.receipt(t)
	childfd, err := unix.PidfdOpen(info.PID, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer unix.Close(childfd)
	if err := unix.PidfdSendSignal(process.pidfd, unix.SIGKILL, nil, 0); err != nil {
		t.Fatal(err)
	}
	if err := process.command.Wait(); err == nil {
		t.Fatal("wrapper unexpectedly succeeded")
	}
	// This fd was acquired while the authenticated direct child still existed.
	if err := unix.PidfdSendSignal(childfd, unix.SIGKILL, nil, 0); err != nil && !errors.Is(err, unix.ESRCH) {
		t.Fatal(err)
	}
	listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: "@oc-exec-v1-" + process.identity, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	observed := make(chan int, 1)
	go func() {
		connection, err := listener.AcceptUnix()
		if err != nil {
			observed <- -1
			return
		}
		defer connection.Close()
		connection.SetDeadline(time.Now().Add(time.Second))
		var hello [40]byte
		if _, err := io.ReadFull(connection, hello[:]); err != nil {
			observed <- -2
			return
		}
		// A rebound endpoint can see the public challenge but has no server MAC.
		forged := make([]byte, 72)
		copy(forged, execSupervisorMagic)
		if execSupervisorWriteAll(connection, forged) != nil {
			observed <- -3
			return
		}
		var proof [32]byte
		count, _ := connection.Read(proof[:])
		observed <- count
	}()
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	response, err := requestExecSupervisor(ctx, process.identity, process.key[:], execControlRequest{Op: "signal", Signal: 9})
	if err == nil || response.ExitCode != nil {
		t.Fatal("wrapper death/rebinding forged terminal")
	}
	if count := <-observed; count != 0 {
		t.Fatal("client disclosed proof to unauthenticated server")
	}
}

func TestExecSupervisorFailedLaunchCleanupReapsUnwaitedChild(t *testing.T) {
	for _, missing := range []bool{false, true} {
		fd := -1
		attributes := &syscall.SysProcAttr{PidFD: &fd}
		process, err := os.StartProcess("/bin/sleep", []string{"/bin/sleep", "30"}, &os.ProcAttr{Files: []*os.File{nil, os.Stdout, os.Stderr}, Sys: attributes})
		if err != nil {
			t.Fatal(err)
		}
		supervisor := &execSupervisor{process: process, pidfd: fd}
		if missing {
			supervisor.pidfd = -1
		}
		supervisor.failSpawn()
		unix.Close(fd)
		if _, err := process.Wait(); !errors.Is(err, syscall.ECHILD) {
			t.Fatalf("failed launch left child unreaped: %v", err)
		}
	}
	fd := -1
	process, err := os.StartProcess("/bin/true", []string{"/bin/true"}, &os.ProcAttr{Sys: &syscall.SysProcAttr{PidFD: &fd}})
	if err != nil {
		t.Fatal(err)
	}
	defer unix.Close(fd)
	var info unix.Siginfo
	if unix.Waitid(unix.P_PIDFD, fd, &info, unix.WEXITED|unix.WNOWAIT, nil) != nil {
		t.Fatal("observe fast exit")
	}
	(&execSupervisor{process: process, pidfd: fd}).failSpawn()
	if _, err := process.Wait(); !errors.Is(err, syscall.ECHILD) {
		t.Fatal("fast failed launch not reaped")
	}
}

func TestExecSupervisorLookPathPreservesErrDotAndSlash(t *testing.T) {
	directory := t.TempDir()
	executable := filepath.Join(directory, "payload")
	if err := os.WriteFile(executable, []byte("#!/bin/sh\nexit 0\n"), 0700); err != nil {
		t.Fatal(err)
	}
	t.Chdir(directory)
	t.Setenv("PATH", ".")
	if _, err := exec.LookPath("payload"); !errors.Is(err, exec.ErrDot) {
		t.Fatal("Go ErrDot boundary changed")
	}
	if path, err := exec.LookPath("./payload"); err != nil || path != "./payload" {
		t.Fatal("explicit relative slash rejected")
	}
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"payload"}})
	if response := process.receipt(t); response.OK || response.Code != "SPAWN_FAILED" {
		t.Fatal("helper weakened ErrDot")
	}
	process.stdin.Close()
	_, _ = io.ReadAll(process.stdout)
	_, _ = io.ReadAll(process.stderr)
	if err := process.command.Wait(); err == nil {
		t.Fatal("ErrDot spawn failure did not exit")
	}
	process = execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"./payload"}})
	process.receipt(t)
	if response := process.terminal(t); response.ExitCode == nil || *response.ExitCode != 0 {
		t.Fatal("relative executable failed")
	}
	process.stdin.Close()
	_, _ = io.ReadAll(process.stdout)
	_, _ = io.ReadAll(process.stderr)
	process.waitWrapper(t)
}

func TestExecSupervisorBootstrapEOFNeverStartsChild(t *testing.T) {
	var identity [32]byte
	if _, err := rand.Read(identity[:]); err != nil {
		t.Fatal(err)
	}
	command := exec.Command(os.Args[0], "-test.run=^TestExecSupervisorHarness$")
	command.Env = append(os.Environ(), "OC_EXEC_TEST_SUPERVISOR="+hex.EncodeToString(identity[:]))
	command.Stdin = strings.NewReader("short-bootstrap")
	var stdout, stderr bytes.Buffer
	command.Stdout, command.Stderr = &stdout, &stderr
	if err := command.Run(); err == nil {
		t.Fatal("bootstrap EOF accepted")
	} else if exit, ok := err.(*exec.ExitError); !ok || exit.ExitCode() != 125 {
		t.Fatal("wrong fixed bootstrap failure")
	}
	if stdout.String() != execSupervisorReady || stderr.Len() != 0 {
		t.Fatal("bootstrap failure exposed payload or key")
	}
	connection, err := net.DialTimeout("unix", "@oc-exec-v1-"+hex.EncodeToString(identity[:]), time.Second)
	if err == nil {
		connection.Close()
		t.Fatal("bootstrap EOF leaked endpoint")
	}
}

func TestExecSupervisorProtocolRejectsMalformedAndUnboundedFrames(t *testing.T) {
	for _, body := range []string{"", "null {}", `{"argv":[],"env":[]}`, `{"op":"inspect","unknown":1}`} {
		var wire bytes.Buffer
		if body == "" {
			wire.Write([]byte{0xff, 0xff, 0xff, 0xff})
		} else {
			size := len(body)
			wire.Write([]byte{byte(size >> 24), byte(size >> 16), byte(size >> 8), byte(size)})
			wire.WriteString(body)
		}
		var value execControlRequest
		if execSupervisorFrameRead(&wire, &value) == nil {
			t.Fatal("invalid frame accepted")
		}
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := requestExecSupervisor(ctx, "bad", make([]byte, 32), execControlRequest{Op: "signal", Signal: 9}); err == nil {
		t.Fatal("invalid identity accepted")
	}
	if _, err := requestExecSupervisor(ctx, strings.Repeat("a", 64), make([]byte, 31), execControlRequest{Op: "signal", Signal: 9}); err == nil {
		t.Fatal("short key accepted")
	}
}

func TestExecSupervisorAuthenticationRejectsReplayedAndCrossIdentityProof(t *testing.T) {
	key := make([]byte, 32)
	if _, err := rand.Read(key); err != nil {
		t.Fatal(err)
	}
	identity := strings.Repeat("a", 64)
	var stale []byte
	for index := 0; index < 3; index++ {
		server, client := net.Pipe()
		done := make(chan error, 1)
		go func() { done <- execSupervisorAuthenticate(server, key, identity); server.Close() }()
		challenge := make([]byte, 32)
		if _, err := rand.Read(challenge); err != nil {
			t.Fatal(err)
		}
		if execSupervisorWriteAll(client, append([]byte(execSupervisorMagic), challenge...)) != nil {
			t.Fatal("send challenge")
		}
		var reply [72]byte
		if _, err := io.ReadFull(client, reply[:]); err != nil {
			t.Fatal(err)
		}
		proof := execSupervisorMAC(key, "client", identity, challenge, reply[8:40])
		if index == 0 {
			stale = append([]byte{}, proof...)
		} else if index == 1 {
			proof = stale
		} else {
			proof = execSupervisorMAC(key, "client", strings.Repeat("b", 64), challenge, reply[8:40])
		}
		if execSupervisorWriteAll(client, proof) != nil {
			t.Fatal("send proof")
		}
		err := <-done
		client.Close()
		if index == 0 && err != nil || index != 0 && err == nil {
			t.Fatal("fresh challenge/identity binding violated")
		}
	}
}

func TestExecSupervisorRunOwnerGoneDrainsBlockedOutputAndReapsChild(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/sh", "-c", "dd if=/dev/zero bs=65536 count=64 2>/dev/null; exit 7"}})
	info := process.receipt(t)
	time.Sleep(30 * time.Millisecond)
	// Neither Engine stdout nor stderr is read. This must interrupt an in-flight write.
	process.owner.Close()
	process.owner = nil
	process.waitWrapper(t)
	if _, err := os.Stat(fmt.Sprintf("/proc/%d", info.PID)); !os.IsNotExist(err) {
		t.Fatal("child not reaped")
	}
}

func TestExecSupervisorPreRunCrashAndPayloadNeverSpawn(t *testing.T) {
	for _, payload := range []bool{false, true} {
		marker := filepath.Join(t.TempDir(), "must-not-exist")
		process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/sh", "-c", "touch " + marker}})
		if payload {
			_, _ = process.stdin.Write([]byte("unexpected"))
		}
		process.stdin.Close()
		process.waitWrapper(t)
		if _, err := os.Stat(marker); !os.IsNotExist(err) {
			t.Fatal("pre-run crash spawned child")
		}
	}
}

func TestExecSupervisorRawPTYInputEOFDoesNotCloseOutput(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{os.Args[0], "-test.run=^TestExecSupervisorPayload$"}, Pty: true, Rows: 24, Cols: 80}, "OC_EXEC_TEST_PAYLOAD=raw-pty")
	process.receipt(t)
	line, err := process.stdout.ReadString('\n')
	if err != nil || strings.TrimSpace(line) != "RAW_PTY_READY" {
		t.Fatal("raw PTY not ready")
	}
	if _, err := io.WriteString(process.stdin, "raw"); err != nil {
		t.Fatal(err)
	}
	process.stdin.Close()
	wait := process.terminal(t)
	if wait.ExitCode == nil || *wait.ExitCode != 0 {
		t.Fatalf("raw PTY EOF invented input/closed master: %+v", wait)
	}
	body, _ := io.ReadAll(process.stdout)
	if !bytes.Contains(body, []byte("RAW_EOF_PRESERVES_MASTER")) {
		t.Fatal("raw PTY output lost")
	}
	_, _ = io.ReadAll(process.stderr)
	process.waitWrapper(t)
}

func TestExecSupervisorStopContinueTargetsOnlyChild(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/sleep", "30"}})
	info := process.receipt(t)
	if !process.call(t, execControlRequest{Op: "signal", Signal: 19}).OK {
		t.Fatal("stop failed")
	}
	deadline := time.Now().Add(time.Second)
	stopped := false
	for time.Now().Before(deadline) {
		stat, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", info.PID))
		if err != nil {
			t.Fatal(err)
		}
		fields := bytes.Fields(stat[bytes.LastIndexByte(stat, ')')+1:])
		if string(fields[0]) == "T" {
			stopped = true
			break
		}
		time.Sleep(time.Millisecond)
	}
	if !stopped {
		t.Fatal("true child did not stop")
	}
	if check := process.receipt(t); check.PID != info.PID || check.State != "running" {
		t.Fatal("helper stopped or switched child")
	}
	if !process.call(t, execControlRequest{Op: "signal", Signal: 18}).OK {
		t.Fatal("continue failed")
	}
	if !process.call(t, execControlRequest{Op: "signal", Signal: 9}).OK {
		t.Fatal("kill failed")
	}
	wait := process.terminal(t)
	if wait.ExitCode == nil || *wait.ExitCode != 137 {
		t.Fatal("child termination lost")
	}
	process.stdin.Close()
	_, _ = io.ReadAll(process.stdout)
	_, _ = io.ReadAll(process.stderr)
	process.waitWrapper(t)
}

func TestExecSupervisorReadOnlyDirectoryNeedsNoShellOrStateFile(t *testing.T) {
	directory := t.TempDir()
	if os.Chmod(directory, 0500) != nil {
		t.Fatal("chmod")
	}
	t.Cleanup(func() { os.Chmod(directory, 0700) })
	t.Chdir(directory)
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{os.Args[0], "-test.run=^TestExecSupervisorPayload$"}}, "OC_EXEC_TEST_PAYLOAD=shellless")
	process.receipt(t)
	process.stdin.Close()
	terminal := process.terminal(t)
	if terminal.ExitCode == nil || *terminal.ExitCode != 0 {
		t.Fatal("shellless failed")
	}
	body, _ := io.ReadAll(process.stdout)
	io.ReadAll(process.stderr)
	process.waitWrapper(t)
	if string(body) != "shellless-readonly" {
		t.Fatal("output changed")
	}
	entries, err := os.ReadDir(directory)
	if err != nil || len(entries) != 0 {
		t.Fatal("wrote state to readonly cwd")
	}
}

func TestExecSupervisorRejectsMalformedAuthAndControlWithoutSpawning(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/true"}})
	for _, hello := range [][]byte{[]byte("bad"), make([]byte, 40)} {
		connection, err := net.Dial("unix", "@oc-exec-v1-"+process.identity)
		if err != nil {
			t.Fatal(err)
		}
		connection.Write(hello)
		connection.(*net.UnixConn).CloseWrite()
		_ = connection.SetReadDeadline(time.Now().Add(time.Second))
		var buffer [1]byte
		if _, err := connection.Read(buffer[:]); err == nil {
			t.Fatal("bad hello accepted")
		}
		connection.Close()
	}
	connection, err := net.Dial("unix", "@oc-exec-v1-"+process.identity)
	if err != nil {
		t.Fatal(err)
	}
	if execSupervisorClientAuthenticate(connection, process.key[:], process.identity) != nil {
		t.Fatal("auth")
	}
	connection.Write([]byte{0, 0, 0x20, 0})
	connection.(*net.UnixConn).CloseWrite()
	_ = connection.SetReadDeadline(time.Now().Add(time.Second))
	var body [1]byte
	if _, err := connection.Read(body[:]); err == nil {
		t.Fatal("unbounded control accepted")
	}
	connection.Close()
	process.stdin.Close()
	process.waitWrapper(t)
}

func TestExecSupervisorPeerFailureAndPTYValidationAreFailClosed(t *testing.T) {
	for _, start := range []execSupervisorStart{
		{Argv: []string{"/bin/true"}, Rows: 1},
		{Argv: []string{"/bin/true"}, Pty: true},
	} {
		process := execSupervisorTestStart(t, start)
		receipt := process.receipt(t)
		if receipt.Code != "SPAWN_FAILED" || receipt.PID != 0 {
			t.Fatal("invalid PTY spawned")
		}
		process.stdin.Close()
		io.ReadAll(process.stdout)
		io.ReadAll(process.stderr)
		if err := process.command.Wait(); err == nil {
			t.Fatal("bad spawn succeeded")
		}
	}
	closed, err := os.Open("/dev/null")
	if err != nil {
		t.Fatal(err)
	}
	closed.Close()
	if _, err := execSupervisorOutputFile(closed); err == nil {
		t.Fatal("closed output accepted")
	}
	regular, err := os.CreateTemp(t.TempDir(), "output")
	if err != nil {
		t.Fatal(err)
	}
	defer regular.Close()
	if output, err := execSupervisorOutputFile(regular); err == nil {
		output.file.Close()
		t.Fatal("nonpollable output accepted")
	}
	for _, op := range []execControlRequest{{Op: "resize", Signal: 1}, {Op: "resize", Rows: 1, Cols: 1}, {Op: "signal", Signal: 9}} {
		response := (&execSupervisor{}).handle(op)
		if response.OK {
			t.Fatal("no-child control accepted")
		}
	}
	if _, err := execSupervisorStartTicks(-1); err == nil {
		t.Fatal("missing PID accepted")
	}
}

func TestExecSupervisorClientDisconnectDuringAuthNeverClaimsRun(t *testing.T) {
	for _, serverBehavior := range []int{0, 1, 2} {
		server, client := net.Pipe()
		done := make(chan struct{})
		go func() {
			defer server.Close()
			defer close(done)
			if serverBehavior == 0 {
				return
			}
			var hello [40]byte
			_, _ = io.ReadFull(server, hello[:])
			if serverBehavior == 1 {
				server.Write([]byte("partial"))
				return
			}
			reply := append([]byte(execSupervisorMagic), make([]byte, 32)...)
			reply = append(reply, execSupervisorMAC(make([]byte, 32), "server", strings.Repeat("a", 64), hello[8:], reply[8:40])...)
			server.Write(reply)
		}()
		if execSupervisorClientAuthenticate(client, make([]byte, 32), strings.Repeat("a", 64)) == nil {
			t.Fatal("failed auth accepted")
		}
		client.Close()
		<-done
	}
}

func TestExecSupervisorClosedAttachAndEndpointCollisionFailWithoutChild(t *testing.T) {
	for _, closed := range []string{"stdout", "stderr"} {
		command := exec.Command(os.Args[0], "-test.run=^TestExecSupervisorHarness$")
		command.Env = append(os.Environ(), "OC_EXEC_TEST_SUPERVISOR="+strings.Repeat("c", 64), "OC_EXEC_TEST_CLOSE="+closed)
		command.Stdout = io.Discard
		command.Stderr = io.Discard
		if err := command.Run(); err == nil {
			t.Fatal("closed attach accepted")
		} else if failure, ok := err.(*exec.ExitError); !ok || failure.ExitCode() != 125 {
			t.Fatal("wrong failure")
		}
	}
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/true"}})
	command := exec.Command(os.Args[0], "-test.run=^TestExecSupervisorHarness$")
	command.Env = append(os.Environ(), "OC_EXEC_TEST_SUPERVISOR="+process.identity)
	command.Stdout = io.Discard
	command.Stderr = io.Discard
	if err := command.Run(); err == nil {
		t.Fatal("endpoint collision spawned")
	}
	process.stdin.Close()
	process.waitWrapper(t)
}

func TestExecSupervisorControlIOAndKernelFailuresNeverInventSuccess(t *testing.T) {
	var frame bytes.Buffer
	if execSupervisorFrameWrite(&frame, make(chan int)) == nil {
		t.Fatal("non JSON accepted")
	}
	if execSupervisorFrameWrite(&frame, strings.Repeat("x", execSupervisorMaxFrame+1)) == nil {
		t.Fatal("oversized JSON accepted")
	}
	if execSupervisorControlRead(strings.NewReader("\x00\x00\x00\x08{}"), new(execControlRequest)) == nil {
		t.Fatal("truncated JSON accepted")
	}
	code := (&execSupervisor{state: "unknown", process: &os.Process{Pid: 123}, startTicks: 456}).responseLocked()
	if code.OK || code.Code != "CHILD_STATUS_UNKNOWN" || code.ExitCode != nil {
		t.Fatal("unknown child projected terminal")
	}
	if response := (&execSupervisor{state: "running", pidfd: -1}).handle(execControlRequest{Op: "signal", Signal: 9}); response.OK || response.Code != "SIGNAL_FAILED" {
		t.Fatal("invalid pidfd fell back to PID")
	}
	master, err := os.Open("/dev/null")
	if err != nil {
		t.Fatal(err)
	}
	defer master.Close()
	if response := (&execSupervisor{state: "running", pty: &execSupervisorPTY{master: master}}).handle(execControlRequest{Op: "resize", Rows: 1, Cols: 1}); response.OK || response.Code != "PTY_FAILED" {
		t.Fatal("failed resize accepted")
	}
	if err := execSupervisorRun("bad", strings.NewReader(""), nil, nil); err == nil {
		t.Fatal("invalid identity accepted")
	}
}

func TestExecSupervisorServerRejectsTruncatedAndFailedProof(t *testing.T) {
	for _, phase := range []string{"hello", "proof", "reply"} {
		server, client := net.Pipe()
		done := make(chan error, 1)
		go func() {
			done <- execSupervisorAuthenticate(server, make([]byte, 32), strings.Repeat("a", 64))
			server.Close()
		}()
		if phase != "hello" {
			client.Write(append([]byte(execSupervisorMagic), make([]byte, 32)...))
		}
		if phase == "proof" {
			var reply [72]byte
			io.ReadFull(client, reply[:])
			client.Write([]byte("short"))
		}
		client.Close()
		if err := <-done; err == nil {
			t.Fatal("incomplete proof accepted")
		}
	}
}

func TestExecSupervisorLiveOwnerClosedOutputGetsFailureProofAndRealExit(t *testing.T) {
	for _, stream := range []string{"stdout", "stderr"} {
		command := "sleep 0.05; printf data; exit 7"
		if stream == "stderr" {
			command = "sleep 0.05; printf data >&2; exit 7"
		}
		process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/sh", "-c", command}})
		receipt := process.receipt(t)
		if stream == "stdout" {
			process.outputPipe.Close()
		} else {
			process.stderr.Close()
		}
		process.stdin.Close()
		terminal := process.terminal(t)
		if terminal.ExitCode == nil || *terminal.ExitCode != 7 || terminal.PID != receipt.PID {
			t.Fatal("output failure corrupted real terminal")
		}
		var drained execControlResponse
		if execSupervisorControlRead(process.owner, &drained) != nil || drained.OK || drained.Code != "OUTPUT_DRAIN_FAILED" || drained.State != "drained" || drained.PID != 0 || drained.ExitCode != nil {
			t.Fatal("output truncation silently succeeded")
		}
		var extra [1]byte
		if _, err := process.owner.Read(extra[:]); err != io.EOF {
			t.Fatal("failure retirement")
		}
		process.owner.Close()
		process.owner = nil
		process.waitWrapper(t)
	}
}

func TestExecSupervisorWrapperCrashAfterTerminalHasNoDrainProof(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{os.Args[0], "-test.run=^TestExecSupervisorPayload$"}}, "OC_EXEC_TEST_PAYLOAD=descendant")
	process.receipt(t)
	terminal := process.terminal(t)
	if terminal.ExitCode == nil || *terminal.ExitCode != 7 {
		t.Fatal("real terminal")
	}
	if unix.PidfdSendSignal(process.pidfd, unix.SIGKILL, nil, 0) != nil {
		t.Fatal("wrapper kill")
	}
	var drained execControlResponse
	if execSupervisorControlRead(process.owner, &drained) == nil {
		t.Fatal("crash fabricated successful drain")
	}
	if err := process.command.Wait(); err == nil {
		t.Fatal("wrapper crash disappeared")
	}
	process.stdin.Close()
	time.Sleep(750 * time.Millisecond)
}
