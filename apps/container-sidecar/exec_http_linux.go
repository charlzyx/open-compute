//go:build linux

package main

import (
	"bytes"
	"context"
	"encoding/hex"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"strings"
	"time"
)

const execControlPath = "/open-compute/exec"
const execControlUpgrade = "open-compute-exec-v1"

type execHTTPRequest struct {
	Identity string `json:"identity"`
	Key      string `json:"key"`
	Op       string `json:"op"`
	Signal   int    `json:"signal,omitempty"`
	Rows     uint16 `json:"rows,omitempty"`
	Cols     uint16 `json:"cols,omitempty"`
}

// The ingress authenticates the provider credential before reading this private body.
func handleExecControl(ctx context.Context, connection net.Conn, request *http.Request) {
	if connection.SetReadDeadline(time.Now().Add(5*time.Second)) != nil {
		return
	}
	serveExecControl(ctx, connection, request, requestExecSupervisor, func(ctx context.Context, identity string, key []byte) (net.Conn, error) {
		return openExecSupervisor(ctx, identity, key, execControlRequest{Op: "run"})
	})
}
func execHTTPInput(request *http.Request) (execHTTPRequest, []byte, bool) {
	var input execHTTPRequest
	if request.Method != http.MethodPost || request.URL.Path != execControlPath || request.URL.RawQuery != "" || request.ContentLength > 4096 {
		return input, nil, false
	}
	limited := &io.LimitedReader{R: request.Body, N: 4097}
	body, err := io.ReadAll(limited)
	if err != nil || limited.N == 0 || execSupervisorDecodeJSON(body, &input) != nil {
		return input, nil, false
	}
	fields, err := execSupervisorObjectFields(body)
	if err != nil {
		return input, nil, false
	}
	key, err := hex.DecodeString(input.Key)
	valid := input.Op == "run" && execHTTPExactKeys(fields, "identity", "key", "op") ||
		input.Op == "signal" && execHTTPExactKeys(fields, "identity", "key", "op", "signal") && input.Signal >= 1 && input.Signal <= 64 ||
		input.Op == "resize" && execHTTPExactKeys(fields, "identity", "key", "op", "rows", "cols")
	if input.Op == "run" {
		valid = valid && strings.EqualFold(request.Header.Get("Upgrade"), execControlUpgrade) && execHTTPHasUpgrade(request.Header.Get("Connection"))
	}
	if err != nil || len(key) != 32 || hex.EncodeToString(key) != input.Key || !execSupervisorIdentity.MatchString(input.Identity) || !valid {
		clear(key)
		return input, nil, false
	}
	return input, key, true
}
func execHTTPHasUpgrade(value string) bool {
	for _, token := range strings.Split(value, ",") {
		if strings.EqualFold(strings.TrimSpace(token), "upgrade") {
			return true
		}
	}
	return false
}
func serveExecControl(ctx context.Context, writer io.Writer, request *http.Request,
	call func(context.Context, string, []byte, execControlRequest) (execControlResponse, error),
	run func(context.Context, string, []byte) (net.Conn, error)) {
	input, key, valid := execHTTPInput(request)
	defer clear(key)
	if !valid {
		execHTTPError(writer, http.StatusBadRequest, "INVALID_EXEC_CONTROL")
		return
	}
	if input.Op == "run" {
		peer, ok := writer.(net.Conn)
		if !ok {
			execHTTPError(writer, http.StatusBadRequest, "INVALID_EXEC_CONTROL")
			return
		}
		_ = peer.SetReadDeadline(time.Time{})
		owner, err := run(ctx, input.Identity, key)
		if err != nil {
			execHTTPError(writer, http.StatusServiceUnavailable, "EXEC_CONTROL_UNAVAILABLE")
			return
		}
		defer owner.Close()
		// Peer EOF actively closes the Unix owner even if no terminal has arrived.
		go func() { var extra [1]byte; _, _ = peer.Read(extra[:]); owner.Close() }()
		_ = peer.SetWriteDeadline(time.Now().Add(execSupervisorHandshakeTimeout))
		if execSupervisorWriteAll(peer, []byte("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: "+execControlUpgrade+"\r\n\r\n")) != nil {
			return
		}
		execHTTPRunFrames(owner, peer)
		return
	}
	response, err := call(ctx, input.Identity, key, execControlRequest{Op: input.Op, Signal: input.Signal, Rows: input.Rows, Cols: input.Cols})
	if err != nil {
		execHTTPError(writer, http.StatusServiceUnavailable, "EXEC_CONTROL_UNAVAILABLE")
		return
	}
	execHTTPResponse(writer, http.StatusOK, response)
}
func execHTTPRunFrames(owner, peer net.Conn) {
	started, fields, err := execHTTPReadFrame(owner)
	if err != nil {
		return
	}
	spawnFailed := execHTTPExactKeys(fields, "ok", "code", "state") && !started.OK && started.Code == "SPAWN_FAILED" && started.State == "spawn-failed" && started.PID == 0 && started.StartTicks == 0 && started.ExitCode == nil
	if !spawnFailed && (!execHTTPExactKeys(fields, "ok", "state", "pid", "startTicks") || !started.OK || started.Code != "" || started.State != "running" || started.PID <= 0 || started.StartTicks == 0 || started.ExitCode != nil) {
		return
	}
	_ = peer.SetWriteDeadline(time.Now().Add(execSupervisorHandshakeTimeout))
	if execSupervisorFrameWrite(peer, started) != nil {
		return
	}
	if !spawnFailed {
		terminal, fields, err := execHTTPReadFrame(owner)
		if err != nil {
			return
		}
		exited := execHTTPExactKeys(fields, "ok", "state", "pid", "startTicks", "exitCode") && terminal.OK && terminal.Code == "" && terminal.State == "exited" && terminal.ExitCode != nil && *terminal.ExitCode >= 0 && *terminal.ExitCode <= 255
		unknown := execHTTPExactKeys(fields, "ok", "code", "state", "pid", "startTicks") && !terminal.OK && terminal.Code == "CHILD_STATUS_UNKNOWN" && terminal.State == "unknown" && terminal.ExitCode == nil
		if (!exited && !unknown) || terminal.PID != started.PID || terminal.StartTicks != started.StartTicks {
			return
		}
		_ = peer.SetWriteDeadline(time.Now().Add(execSupervisorHandshakeTimeout))
		if execSupervisorFrameWrite(peer, terminal) != nil {
			return
		}
	}
	if spawnFailed {
		return
	}
	drained, fields, err := execHTTPReadFrame(owner)
	if err != nil {
		return
	}
	if drained.State != "drained" || drained.PID != 0 || drained.StartTicks != 0 || drained.ExitCode != nil ||
		!(drained.OK && drained.Code == "" && execHTTPExactKeys(fields, "ok", "state") || !drained.OK && drained.Code == "OUTPUT_DRAIN_FAILED" && execHTTPExactKeys(fields, "ok", "code", "state")) {
		return
	}
	// A complete drain receipt and EOF are both required. Extra bytes never become success.
	var extra [1]byte
	count, err := owner.Read(extra[:])
	if count != 0 || err != io.EOF {
		return
	}
	_ = peer.SetWriteDeadline(time.Now().Add(execSupervisorHandshakeTimeout))
	_ = execSupervisorFrameWrite(peer, drained)
}
func execHTTPReadFrame(reader io.Reader) (execControlResponse, map[string]json.RawMessage, error) {
	var response execControlResponse
	body, err := execSupervisorReadBody(reader, 4096)
	if err != nil {
		return response, nil, err
	}
	fields, err := execSupervisorObjectFields(body)
	if err != nil {
		return response, nil, err
	}
	if err := execSupervisorDecodeJSON(body, &response); err != nil {
		return response, nil, err
	}
	return response, fields, nil
}
func execHTTPExactKeys(fields map[string]json.RawMessage, names ...string) bool {
	if len(fields) != len(names) {
		return false
	}
	for _, name := range names {
		if _, exists := fields[name]; !exists {
			return false
		}
	}
	return true
}
func execHTTPError(writer io.Writer, status int, code string) {
	execHTTPResponse(writer, status, execControlResponse{Code: code})
}
func execHTTPResponse(writer io.Writer, status int, response execControlResponse) {
	body, err := json.Marshal(response)
	if err != nil {
		return
	}
	_ = (&http.Response{ProtoMajor: 1, ProtoMinor: 1, StatusCode: status, Close: true, Header: http.Header{"Content-Type": {"application/json"}}, ContentLength: int64(len(body)), Body: io.NopCloser(bytes.NewReader(body))}).Write(writer)
}
