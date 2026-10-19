//go:build linux

package main

import (
	"bytes"
	"errors"
	"io"
	"net"
	"os"
	"os/exec"
	"strconv"
	"sync"
	"syscall"
	"time"

	"golang.org/x/sys/unix"
)

type execSupervisor struct {
	mutex          sync.Mutex
	key            [32]byte
	identity       string
	listener       *net.UnixListener
	process        *os.Process
	pidfd          int
	startTicks     uint64
	state          string
	exitCode       int
	claimed        bool
	inputClosed    bool
	input          *io.PipeReader
	exited         chan struct{}
	drained        chan struct{}
	finished       chan struct{}
	finishOnce     sync.Once
	drainError     bool
	pty            *execSupervisorPTY
	output, stderr *execSupervisorOutput
	start          execSupervisorStart
}

// The duplicate is pollable; cancellation interrupts an already blocked write.
// All child bytes are still drained after the Engine attach or owner disappears.
type execSupervisorOutput struct {
	file      *os.File
	discarded bool
	failed    bool
	mutex     sync.Mutex
}

func execSupervisorOutputFile(original *os.File) (*execSupervisorOutput, error) {
	fd, err := unix.FcntlInt(original.Fd(), unix.F_DUPFD_CLOEXEC, 3)
	if err != nil {
		return nil, errors.New("OUTPUT_SETUP_FAILED")
	}
	if unix.SetNonblock(fd, true) != nil {
		unix.Close(fd)
		return nil, errors.New("OUTPUT_SETUP_FAILED")
	}
	file := os.NewFile(uintptr(fd), "exec-output")
	if file.SetWriteDeadline(time.Time{}) != nil {
		file.Close()
		return nil, errors.New("OUTPUT_SETUP_FAILED")
	}
	return &execSupervisorOutput{file: file}, nil
}
func (output *execSupervisorOutput) Write(body []byte) (int, error) {
	output.mutex.Lock()
	discarded := output.discarded
	output.mutex.Unlock()
	if discarded {
		return len(body), nil
	}
	_, err := output.file.Write(body)
	if err != nil {
		output.mutex.Lock()
		if !output.discarded {
			output.failed = true
		}
		output.mutex.Unlock()
		output.discard()
	}
	return len(body), nil
}
func (output *execSupervisorOutput) discard() {
	output.mutex.Lock()
	output.discarded = true
	output.mutex.Unlock()
	_ = output.file.SetWriteDeadline(time.Now())
}

// runExecSupervisor is selected by the sidecar's fixed, non-secret helper argv.
func runExecSupervisor(identity string) int {
	output, err := execSupervisorOutputFile(os.Stdout)
	if err != nil {
		return 125
	}
	defer output.file.Close()
	stderr, err := execSupervisorOutputFile(os.Stderr)
	if err != nil {
		return 125
	}
	defer stderr.file.Close()
	if execSupervisorRun(identity, os.Stdin, output, stderr) != nil {
		return 125
	}
	return 0
}

func execSupervisorRun(identity string, input io.Reader, output, stderr *execSupervisorOutput) error {
	if !execSupervisorIdentity.MatchString(identity) {
		return errors.New("INVALID_IDENTITY")
	}
	if unix.Prctl(unix.PR_SET_DUMPABLE, 0, 0, 0, 0) != nil {
		return errors.New("PROCESS_HARDENING_FAILED")
	}
	listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: "@oc-exec-v1-" + identity, Net: "unix"})
	if err != nil {
		return errors.New("CONTROL_LISTEN_FAILED")
	}
	defer listener.Close()
	if execSupervisorWriteAll(output.file, []byte(execSupervisorReady)) != nil {
		return errors.New("READY_FAILED")
	}
	supervisor := &execSupervisor{identity: identity, listener: listener, pidfd: -1, state: "starting", exited: make(chan struct{}), drained: make(chan struct{}), finished: make(chan struct{}), output: output, stderr: stderr}
	if _, err := io.ReadFull(input, supervisor.key[:]); err != nil {
		return errors.New("BOOTSTRAP_FAILED")
	}
	defer func() { supervisor.mutex.Lock(); clear(supervisor.key[:]); supervisor.mutex.Unlock() }()
	if err := execSupervisorFrameRead(input, &supervisor.start); err != nil {
		return err
	}
	reader, writer := io.Pipe()
	supervisor.input = reader
	defer reader.Close()
	defer writer.Close()
	defer supervisor.close()
	go supervisor.forwardInput(input, writer)
	go supervisor.accept()
	<-supervisor.finished
	if supervisor.state == "spawn-failed" {
		return errors.New("SPAWN_FAILED")
	}
	return nil
}

// Before run claims ownership, stdin contains no application payload.
// Rejecting pre-run bytes avoids a pipe write blocking detection of owner EOF.
func (supervisor *execSupervisor) forwardInput(input io.Reader, writer *io.PipeWriter) {
	defer writer.Close()
	body := make([]byte, 16384)
	for {
		count, err := input.Read(body)
		supervisor.mutex.Lock()
		claimed := supervisor.claimed
		if !claimed && (count != 0 || err != nil) {
			supervisor.inputClosed = true
			supervisor.finishOnce.Do(func() { close(supervisor.finished) })
		}
		supervisor.mutex.Unlock()
		if !claimed {
			if count != 0 || err != nil {
				return
			}
			continue
		}
		if count != 0 {
			if _, writeErr := writer.Write(body[:count]); writeErr != nil {
				return
			}
		}
		if err != nil {
			return
		}
	}
}

func (supervisor *execSupervisor) spawn(start execSupervisorStart, input io.Reader, output, stderr io.Writer) error {
	if len(start.Argv) == 0 || start.Argv[0] == "" || (!start.Pty && (start.Rows != 0 || start.Cols != 0)) {
		return errors.New("SPAWN_FAILED")
	}
	executable, err := exec.LookPath(start.Argv[0])
	if err != nil {
		return errors.New("SPAWN_FAILED")
	}
	attributes := &syscall.SysProcAttr{PidFD: &supervisor.pidfd}
	var files []*os.File
	var childInput, childOutput, childError *os.File
	var inputWrite, outputRead, errorRead *os.File
	closeFiles := func() {
		for _, file := range []*os.File{childInput, childOutput, childError, inputWrite, outputRead, errorRead} {
			if file != nil {
				file.Close()
			}
		}
	}
	if start.Pty {
		pty, err := execSupervisorOpenPTY(start.Rows, start.Cols)
		if err != nil {
			return err
		}
		supervisor.pty = pty
		attributes.Setsid, attributes.Setctty, attributes.Ctty = true, true, 0
		files = []*os.File{pty.slave, pty.slave, pty.slave}
	} else {
		childInput, inputWrite, err = os.Pipe()
		if err == nil {
			outputRead, childOutput, err = os.Pipe()
		}
		if err == nil {
			errorRead, childError, err = os.Pipe()
		}
		if err != nil {
			closeFiles()
			return errors.New("SPAWN_FAILED")
		}
		files = []*os.File{childInput, childOutput, childError}
	}
	process, err := os.StartProcess(executable, start.Argv, &os.ProcAttr{Env: os.Environ(), Files: files, Sys: attributes})
	if err != nil {
		closeFiles()
		return errors.New("SPAWN_FAILED")
	}
	supervisor.process = process
	if supervisor.pidfd < 0 {
		closeFiles()
		return errors.New("PIDFD_UNAVAILABLE")
	}
	ticks, err := execSupervisorStartTicks(process.Pid)
	if err != nil {
		closeFiles()
		return errors.New("SPAWN_IDENTITY_FAILED")
	}
	supervisor.startTicks, supervisor.state = ticks, "running"
	if start.Pty {
		supervisor.pty.slave.Close()
		go supervisor.pty.copyInput(input)
		go func() { supervisor.completeDrain(supervisor.pty.copyOutput(output) != nil) }()
	} else {
		childInput.Close()
		childOutput.Close()
		childError.Close()
		go func() { _, _ = io.Copy(inputWrite, input); inputWrite.Close() }()
		go func() {
			results := make(chan error, 2)
			go func() { _, err := io.Copy(output, outputRead); outputRead.Close(); results <- err }()
			go func() { _, err := io.Copy(stderr, errorRead); errorRead.Close(); results <- err }()
			first, second := <-results, <-results
			supervisor.completeDrain(first != nil || second != nil)
		}()
	}
	go supervisor.waitChild()
	return nil
}

// Failed launch cleanup owns the direct child until it reaps that child.
func (supervisor *execSupervisor) failSpawn() {
	if supervisor.process != nil && supervisor.pidfd >= 0 {
		err := unix.PidfdSendSignal(supervisor.pidfd, unix.SIGKILL, nil, 0)
		if err == nil || errors.Is(err, unix.ESRCH) {
			_, _ = supervisor.process.Wait()
		}
	}
	if supervisor.process != nil && supervisor.pidfd < 0 {
		// Failed launch only: an unreaped direct child cannot have its PID reused.
		if err := supervisor.process.Kill(); err == nil || errors.Is(err, os.ErrProcessDone) {
			_, _ = supervisor.process.Wait()
		}
	}
	if supervisor.pty != nil {
		supervisor.pty.master.Close()
		supervisor.pty.slave.Close()
	}
}

func execSupervisorStartTicks(pid int) (uint64, error) {
	stat, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
	if err != nil {
		return 0, errors.New("SPAWN_IDENTITY_FAILED")
	}
	separator := bytes.LastIndexByte(stat, ')')
	if separator < 0 {
		return 0, errors.New("SPAWN_IDENTITY_FAILED")
	}
	fields := bytes.Fields(stat[separator+1:])
	if len(fields) < 20 {
		return 0, errors.New("SPAWN_IDENTITY_FAILED")
	}
	ticks, err := strconv.ParseUint(string(fields[19]), 10, 64)
	if err != nil || ticks == 0 {
		return 0, errors.New("SPAWN_IDENTITY_FAILED")
	}
	return ticks, nil
}

func (supervisor *execSupervisor) waitChild() {
	state, err := supervisor.process.Wait()
	supervisor.mutex.Lock()
	if err != nil {
		supervisor.state = "unknown"
	} else if status, ok := state.Sys().(syscall.WaitStatus); ok {
		supervisor.state = "exited"
		supervisor.exitCode = status.ExitStatus()
		if status.Signaled() {
			supervisor.exitCode = 128 + int(status.Signal())
		}
	} else {
		supervisor.state = "unknown"
	}
	close(supervisor.exited)
	supervisor.mutex.Unlock()
}
func (supervisor *execSupervisor) completeDrain(failed bool) {
	supervisor.mutex.Lock()
	supervisor.drainError = failed
	close(supervisor.drained)
	supervisor.mutex.Unlock()
}
func (supervisor *execSupervisor) close() {
	supervisor.mutex.Lock()
	defer supervisor.mutex.Unlock()
	if supervisor.pidfd >= 0 {
		_ = unix.Close(supervisor.pidfd)
		supervisor.pidfd = -1
	}
	if supervisor.pty != nil {
		supervisor.pty.master.Close()
		supervisor.pty.slave.Close()
	}
}
func (supervisor *execSupervisor) accept() {
	capacity := make(chan struct{}, 16)
	for {
		connection, err := supervisor.listener.AcceptUnix()
		if err != nil {
			return
		}
		select {
		case capacity <- struct{}{}:
			go func() { defer func() { <-capacity }(); defer connection.Close(); supervisor.serve(connection) }()
		default:
			connection.Close()
		}
	}
}
func (supervisor *execSupervisor) serve(connection net.Conn) {
	supervisor.mutex.Lock()
	key := supervisor.key
	supervisor.mutex.Unlock()
	defer clear(key[:])
	if execSupervisorAuthenticate(connection, key[:], supervisor.identity) != nil {
		return
	}
	if connection.SetReadDeadline(time.Now().Add(execSupervisorHandshakeTimeout)) != nil {
		return
	}
	var request execControlRequest
	if execSupervisorControlRead(connection, &request) != nil {
		return
	}
	if connection.SetReadDeadline(time.Time{}) != nil {
		return
	}
	if request.Op == "run" && request.Signal == 0 && request.Rows == 0 && request.Cols == 0 {
		supervisor.run(connection)
		return
	}
	response := supervisor.handle(request)
	_ = connection.SetWriteDeadline(time.Now().Add(execSupervisorHandshakeTimeout))
	_ = execSupervisorFrameWrite(connection, response)
}
func (supervisor *execSupervisor) run(connection net.Conn) {
	supervisor.mutex.Lock()
	if supervisor.claimed || supervisor.inputClosed {
		supervisor.mutex.Unlock()
		_ = execSupervisorFrameWrite(connection, execControlResponse{Code: "RUN_UNAVAILABLE"})
		return
	}
	supervisor.claimed = true
	supervisor.mutex.Unlock()
	go func() {
		var extra [1]byte
		_, _ = connection.Read(extra[:])
		supervisor.output.discard()
		supervisor.stderr.discard()
	}()
	supervisor.mutex.Lock()
	spawnError := supervisor.spawn(supervisor.start, supervisor.input, supervisor.output, supervisor.stderr)
	supervisor.mutex.Unlock()
	if spawnError != nil {
		supervisor.failSpawn()
		supervisor.mutex.Lock()
		supervisor.state = "spawn-failed"
		supervisor.mutex.Unlock()
		_ = connection.SetWriteDeadline(time.Now().Add(execSupervisorHandshakeTimeout))
		_ = execSupervisorFrameWrite(connection, execControlResponse{Code: "SPAWN_FAILED", State: "spawn-failed"})
		supervisor.finishOnce.Do(func() { close(supervisor.finished) })
		return
	}
	// STARTED always describes the successful birth, even if Wait already observed exit.
	supervisor.mutex.Lock()
	started := execControlResponse{OK: true, State: "running", PID: supervisor.process.Pid, StartTicks: supervisor.startTicks}
	supervisor.mutex.Unlock()
	_ = connection.SetWriteDeadline(time.Now().Add(execSupervisorHandshakeTimeout))
	if execSupervisorFrameWrite(connection, started) != nil {
		supervisor.output.discard()
		supervisor.stderr.discard()
	}
	<-supervisor.exited
	supervisor.mutex.Lock()
	terminal := supervisor.responseLocked()
	supervisor.mutex.Unlock()
	_ = connection.SetWriteDeadline(time.Now().Add(execSupervisorHandshakeTimeout))
	if execSupervisorFrameWrite(connection, terminal) != nil {
		supervisor.output.discard()
		supervisor.stderr.discard()
	}
	// Native caches TERMINAL immediately but keeps run open until this drain completes.
	<-supervisor.drained
	supervisor.mutex.Lock()
	failed := supervisor.drainError
	supervisor.mutex.Unlock()
	supervisor.output.mutex.Lock()
	failed = failed || supervisor.output.failed
	supervisor.output.mutex.Unlock()
	supervisor.stderr.mutex.Lock()
	failed = failed || supervisor.stderr.failed
	supervisor.stderr.mutex.Unlock()
	drained := execControlResponse{OK: !failed, State: "drained"}
	if failed {
		drained.Code = "OUTPUT_DRAIN_FAILED"
	}
	_ = connection.SetWriteDeadline(time.Now().Add(execSupervisorHandshakeTimeout))
	_ = execSupervisorFrameWrite(connection, drained)
	supervisor.finishOnce.Do(func() { close(supervisor.finished) })
}
func (supervisor *execSupervisor) responseLocked() execControlResponse {
	response := execControlResponse{OK: true, State: supervisor.state, PID: supervisor.process.Pid, StartTicks: supervisor.startTicks}
	if supervisor.state == "exited" {
		code := supervisor.exitCode
		response.ExitCode = &code
	} else {
		response.OK = false
		response.Code = "CHILD_STATUS_UNKNOWN"
	}
	return response
}
func (supervisor *execSupervisor) handle(request execControlRequest) execControlResponse {
	if request.Op == "signal" && (request.Signal < 1 || request.Signal > 64 || request.Rows != 0 || request.Cols != 0) {
		return execControlResponse{Code: "INVALID_SIGNAL"}
	}
	if request.Op == "resize" && request.Signal != 0 {
		return execControlResponse{Code: "INVALID_REQUEST"}
	}
	supervisor.mutex.Lock()
	defer supervisor.mutex.Unlock()
	switch request.Op {
	case "signal":
		if supervisor.state != "running" {
			return execControlResponse{Code: "CHILD_NOT_RUNNING"}
		}
		if unix.PidfdSendSignal(supervisor.pidfd, unix.Signal(request.Signal), nil, 0) != nil {
			return execControlResponse{Code: "SIGNAL_FAILED"}
		}
	case "resize":
		if supervisor.pty == nil || supervisor.state != "running" {
			return execControlResponse{Code: "PTY_NOT_RUNNING"}
		}
		if err := supervisor.pty.resize(request.Rows, request.Cols); err != nil {
			return execControlResponse{Code: err.Error()}
		}
	default:
		return execControlResponse{Code: "INVALID_REQUEST"}
	}
	return execControlResponse{OK: true}
}
