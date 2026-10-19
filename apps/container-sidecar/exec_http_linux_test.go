//go:build linux

package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"strings"
	"testing"
	"time"
)

func TestExecHTTPValidationAndSecretFreeErrors(t *testing.T) {
	identity, secret := strings.Repeat("1", 64), strings.Repeat("2", 64)
	base := `{"identity":"` + identity + `","key":"` + secret + `","op":"run"}`
	for _, body := range []string{
		"", "null", "[]", "{}", base + " {}", strings.Repeat(" ", 4096) + base,
		strings.Replace(base, `"run"`, `"unknown"`, 1),
		strings.Replace(base, identity, strings.Repeat("A", 64), 1),
		strings.Replace(base, secret, strings.Repeat("Z", 64), 1),
		strings.TrimSuffix(base, "}") + `,"unknown":1}`,
		strings.TrimSuffix(base, "}") + `,"signal":1}`,
	} {
		request, err := http.NewRequest(http.MethodPost, "http://control"+execControlPath, strings.NewReader(body))
		if err != nil {
			t.Fatal(err)
		}
		var wire bytes.Buffer
		serveExecControl(context.Background(), &wire, request, func(context.Context, string, []byte, execControlRequest) (execControlResponse, error) {
			t.Fatal("invalid request reached helper")
			return execControlResponse{}, nil
		}, nil)
		response, err := http.ReadResponse(bufio.NewReader(&wire), request)
		if err != nil {
			t.Fatal(err)
		}
		payload, err := io.ReadAll(response.Body)
		response.Body.Close()
		if err != nil || response.StatusCode != http.StatusBadRequest || strings.Contains(string(payload), secret) || strings.Contains(string(payload), identity) {
			t.Fatalf("invalid response: status=%d err=%v", response.StatusCode, err)
		}
	}
	for _, operation := range []execControlRequest{{Op: "signal", Signal: 64}, {Op: "resize", Rows: 40, Cols: 100}} {
		input := execHTTPRequest{Identity: identity, Key: secret, Op: operation.Op, Signal: operation.Signal, Rows: operation.Rows, Cols: operation.Cols}
		body, err := json.Marshal(input)
		if err != nil {
			t.Fatal(err)
		}
		for _, failed := range []bool{false, true} {
			request, err := http.NewRequest(http.MethodPost, "http://control"+execControlPath, bytes.NewReader(body))
			if err != nil {
				t.Fatal(err)
			}
			var wire bytes.Buffer
			serveExecControl(context.Background(), &wire, request, func(_ context.Context, actual string, key []byte, control execControlRequest) (execControlResponse, error) {
				if actual != identity || hex.EncodeToString(key) != secret || control != operation {
					t.Fatal("request changed")
				}
				if failed {
					return execControlResponse{}, errors.New(secret)
				}
				code := 137
				_ = code
				return execControlResponse{OK: true}, nil
			}, nil)
			response, err := http.ReadResponse(bufio.NewReader(&wire), request)
			if err != nil {
				t.Fatal(err)
			}
			payload, err := io.ReadAll(response.Body)
			response.Body.Close()
			if err != nil || strings.Contains(string(payload), secret) {
				t.Fatal("secret leaked")
			}
			if failed && response.StatusCode != http.StatusServiceUnavailable || !failed && response.StatusCode != http.StatusOK {
				t.Fatal("incorrect HTTP status")
			}
		}
	}
}

func TestExecHTTPRunUpgradeKeepsTerminalSeparateFromDrain(t *testing.T) {
	for _, failed := range []bool{false, true} {
		input := execHTTPRequest{Identity: strings.Repeat("1", 64), Key: strings.Repeat("2", 64), Op: "run"}
		body, _ := json.Marshal(input)
		request, _ := http.NewRequest(http.MethodPost, "http://control"+execControlPath, bytes.NewReader(body))
		request.Header.Set("Upgrade", execControlUpgrade)
		request.Header.Set("Connection", "keep-alive, Upgrade")
		peer, client := net.Pipe()
		helper, server := net.Pipe()
		done := make(chan struct{})
		go func() {
			defer peer.Close()
			serveExecControl(context.Background(), peer, request, nil, func(context.Context, string, []byte) (net.Conn, error) { return helper, nil })
			close(done)
		}()
		drained := make(chan struct{})
		go func() {
			defer server.Close()
			if failed {
				_ = execSupervisorFrameWrite(server, execControlResponse{Code: "SPAWN_FAILED", State: "spawn-failed"})
				return
			}
			_ = execSupervisorFrameWrite(server, execControlResponse{OK: true, State: "running", PID: 123, StartTicks: 456})
			code := 7
			_ = execSupervisorFrameWrite(server, execControlResponse{OK: true, State: "exited", PID: 123, StartTicks: 456, ExitCode: &code})
			<-drained
			_ = execSupervisorFrameWrite(server, execControlResponse{OK: true, State: "drained"})
		}()
		reader := bufio.NewReader(client)
		response, err := http.ReadResponse(reader, request)
		if err != nil || response.StatusCode != 101 || response.Header.Get("Upgrade") != execControlUpgrade {
			t.Fatal("upgrade")
		}
		var started execControlResponse
		if execSupervisorControlRead(reader, &started) != nil {
			t.Fatal("missing started")
		}
		if failed {
			if started.Code != "SPAWN_FAILED" {
				t.Fatal("spawn error")
			}
		} else {
			var terminal execControlResponse
			if execSupervisorControlRead(reader, &terminal) != nil || terminal.ExitCode == nil || *terminal.ExitCode != 7 {
				t.Fatal("terminal")
			}
			select {
			case <-done:
				t.Fatal("closed before real drain")
			default:
			}
			close(drained)
			var proof execControlResponse
			if execSupervisorControlRead(reader, &proof) != nil || !proof.OK || proof.State != "drained" {
				t.Fatal("drain proof missing")
			}
		}
		if _, err := reader.ReadByte(); err != io.EOF {
			t.Fatal("missing drained EOF")
		}
		client.Close()
		<-done
	}
}

func TestExecHTTPRunPeerDisconnectClosesUnixOwner(t *testing.T) {
	input := execHTTPRequest{Identity: strings.Repeat("a", 64), Key: strings.Repeat("b", 64), Op: "run"}
	body, _ := json.Marshal(input)
	request, _ := http.NewRequest(http.MethodPost, "http://control"+execControlPath, bytes.NewReader(body))
	request.Header.Set("Upgrade", execControlUpgrade)
	request.Header.Set("Connection", "Upgrade")
	peer, client := net.Pipe()
	helper, server := net.Pipe()
	defer server.Close()
	done := make(chan struct{})
	go func() {
		defer peer.Close()
		serveExecControl(context.Background(), peer, request, nil, func(context.Context, string, []byte) (net.Conn, error) { return helper, nil })
		close(done)
	}()
	reader := bufio.NewReader(client)
	if _, err := http.ReadResponse(reader, request); err != nil {
		t.Fatal(err)
	}
	client.Close()
	_ = server.SetReadDeadline(time.Now().Add(time.Second))
	var byte [1]byte
	if _, err := server.Read(byte[:]); err != io.EOF {
		t.Fatal("HTTP disconnect did not close helper")
	}
	<-done
}

func TestExecHTTPRunRejectsMalformedHelperFrames(t *testing.T) {
	code := 7
	cases := [][]execControlResponse{
		{{OK: true, State: "exited", PID: 1, StartTicks: 2, ExitCode: &code}},
		{{OK: true, State: "running", PID: 1, StartTicks: 2}, {OK: true, State: "exited", PID: 2, StartTicks: 2, ExitCode: &code}},
		{{OK: true, State: "running", PID: 1, StartTicks: 2}, {OK: true, State: "exited", PID: 1, StartTicks: 2}},
	}
	for _, frames := range cases {
		owner, helper := net.Pipe()
		peer, client := net.Pipe()
		done := make(chan struct{})
		go func() { defer owner.Close(); defer peer.Close(); execHTTPRunFrames(owner, peer); close(done) }()
		go func() {
			defer helper.Close()
			for _, frame := range frames {
				if execSupervisorFrameWrite(helper, frame) != nil {
					return
				}
			}
		}()
		body, _ := io.ReadAll(client)
		client.Close()
		<-done
		if len(frames) == 1 && len(body) != 0 {
			t.Fatal("invalid birth forwarded")
		}
		if len(frames) == 2 {
			var first execControlResponse
			if execSupervisorControlRead(bytes.NewReader(body), &first) != nil {
				t.Fatal("birth lost")
			}
			var second execControlResponse
			reader := bytes.NewReader(body)
			_ = execSupervisorControlRead(reader, &first)
			if execSupervisorControlRead(reader, &second) == nil {
				t.Fatal("invalid terminal forwarded")
			}
		}
	}
}

func TestExecHTTPRealAdapterRunAndAuthenticatedSignal(t *testing.T) {
	process := execSupervisorTestStart(t, execSupervisorStart{Argv: []string{"/bin/sleep", "30"}})
	input := execHTTPRequest{Identity: process.identity, Key: hex.EncodeToString(process.key[:]), Op: "run"}
	body, _ := json.Marshal(input)
	request, _ := http.NewRequest(http.MethodPost, "http://control"+execControlPath, bytes.NewReader(body))
	request.Header.Set("Upgrade", execControlUpgrade)
	request.Header.Set("Connection", "Upgrade")
	peer, client := net.Pipe()
	done := make(chan struct{})
	go func() { defer peer.Close(); handleExecControl(context.Background(), peer, request); close(done) }()
	reader := bufio.NewReader(client)
	response, err := http.ReadResponse(reader, request)
	if err != nil || response.StatusCode != 101 {
		t.Fatal("real upgrade")
	}
	var started execControlResponse
	if execSupervisorControlRead(reader, &started) != nil || started.PID <= 0 {
		t.Fatal("birth")
	}
	input.Op = "signal"
	input.Signal = 9
	body, _ = json.Marshal(input)
	request, _ = http.NewRequest(http.MethodPost, "http://control"+execControlPath, bytes.NewReader(body))
	control, clientControl := net.Pipe()
	go func() { defer control.Close(); handleExecControl(context.Background(), control, request) }()
	signal, err := http.ReadResponse(bufio.NewReader(clientControl), request)
	if err != nil || signal.StatusCode != 200 {
		t.Fatal("signal HTTP")
	}
	payload, _ := io.ReadAll(signal.Body)
	signal.Body.Close()
	clientControl.Close()
	if string(payload) != `{"ok":true}` {
		t.Fatal("noncanonical signal response")
	}
	var terminal execControlResponse
	if execSupervisorControlRead(reader, &terminal) != nil || terminal.ExitCode == nil || *terminal.ExitCode != 137 {
		t.Fatal("terminal")
	}
	process.stdin.Close()
	io.ReadAll(process.stdout)
	io.ReadAll(process.stderr)
	var proof execControlResponse
	if execSupervisorControlRead(reader, &proof) != nil || !proof.OK || proof.State != "drained" {
		t.Fatal("missing drain proof")
	}
	if _, err := reader.ReadByte(); err != io.EOF {
		t.Fatal("drain EOF")
	}
	client.Close()
	<-done
	process.waitWrapper(t)
}

func TestExecHTTPRunUnreachableAndWrongUpgradeAreFixedErrors(t *testing.T) {
	input := execHTTPRequest{Identity: strings.Repeat("a", 64), Key: strings.Repeat("b", 64), Op: "run"}
	body, _ := json.Marshal(input)
	for _, upgrade := range []string{"", "wrong", execControlUpgrade} {
		request, _ := http.NewRequest(http.MethodPost, "http://control"+execControlPath, bytes.NewReader(body))
		request.Header.Set("Upgrade", upgrade)
		request.Header.Set("Connection", "keep-alive")
		var wire bytes.Buffer
		serveExecControl(context.Background(), &wire, request, nil, nil)
		response, err := http.ReadResponse(bufio.NewReader(&wire), request)
		if err != nil || response.StatusCode != 400 {
			t.Fatal("wrong upgrade accepted")
		}
		response.Body.Close()
	}
	request, _ := http.NewRequest(http.MethodPost, "http://control"+execControlPath, bytes.NewReader(body))
	request.Header.Set("Upgrade", execControlUpgrade)
	request.Header.Set("Connection", "Upgrade")
	var wire bytes.Buffer
	serveExecControl(context.Background(), &wire, request, nil, nil)
	response, err := http.ReadResponse(bufio.NewReader(&wire), request)
	if err != nil || response.StatusCode != 400 {
		t.Fatal("nonduplex run accepted")
	}
	response.Body.Close()
	request, _ = http.NewRequest(http.MethodPost, "http://control"+execControlPath, bytes.NewReader(body))
	request.Header.Set("Upgrade", execControlUpgrade)
	request.Header.Set("Connection", "Upgrade")
	peer, client := net.Pipe()
	done := make(chan struct{})
	go func() { defer peer.Close(); handleExecControl(context.Background(), peer, request); close(done) }()
	response, err = http.ReadResponse(bufio.NewReader(client), request)
	if err != nil || response.StatusCode != 503 {
		t.Fatal("unavailable helper")
	}
	payload, _ := io.ReadAll(response.Body)
	response.Body.Close()
	client.Close()
	<-done
	if bytes.Contains(payload, []byte(input.Key)) || bytes.Contains(payload, []byte(input.Identity)) {
		t.Fatal("identity leaked")
	}
	// A failed read-deadline setup must not dereference the request or make an RPC.
	peer, client = net.Pipe()
	peer.Close()
	client.Close()
	handleExecControl(context.Background(), peer, nil)
}

func TestExecHTTPUnknownChildCannotBecomeWrapperExitCode(t *testing.T) {
	owner, helper := net.Pipe()
	peer, client := net.Pipe()
	done := make(chan struct{})
	go func() { defer owner.Close(); defer peer.Close(); execHTTPRunFrames(owner, peer); close(done) }()
	go func() {
		defer helper.Close()
		execSupervisorFrameWrite(helper, execControlResponse{OK: true, State: "running", PID: 1, StartTicks: 2})
		execSupervisorFrameWrite(helper, execControlResponse{Code: "CHILD_STATUS_UNKNOWN", State: "unknown", PID: 1, StartTicks: 2})
	}()
	var receipt, terminal execControlResponse
	if execSupervisorControlRead(client, &receipt) != nil || execSupervisorControlRead(client, &terminal) != nil {
		t.Fatal("failure frame lost")
	}
	if terminal.OK || terminal.Code != "CHILD_STATUS_UNKNOWN" || terminal.ExitCode != nil {
		t.Fatal("unknown wrapper projected as child")
	}
	var byte [1]byte
	if _, err := client.Read(byte[:]); err != io.EOF {
		t.Fatal("missing EOF")
	}
	client.Close()
	<-done
}

func TestExecHTTPDrainRequiresExactReceiptAndEOF(t *testing.T) {
	for _, mode := range []string{"success", "failure", "missing", "wrong", "extra", "truncated"} {
		owner, helper := net.Pipe()
		peer, client := net.Pipe()
		done := make(chan struct{})
		go func() { defer owner.Close(); defer peer.Close(); execHTTPRunFrames(owner, peer); close(done) }()
		go func() {
			defer helper.Close()
			code := 7
			if execSupervisorFrameWrite(helper, execControlResponse{OK: true, State: "running", PID: 1, StartTicks: 2}) != nil {
				return
			}
			if execSupervisorFrameWrite(helper, execControlResponse{OK: true, State: "exited", PID: 1, StartTicks: 2, ExitCode: &code}) != nil {
				return
			}
			switch mode {
			case "success":
				execSupervisorFrameWrite(helper, execControlResponse{OK: true, State: "drained"})
			case "failure":
				execSupervisorFrameWrite(helper, execControlResponse{Code: "OUTPUT_DRAIN_FAILED", State: "drained"})
			case "wrong":
				execSupervisorFrameWrite(helper, execControlResponse{OK: true, State: "drained", PID: 1})
			case "extra":
				execSupervisorFrameWrite(helper, execControlResponse{OK: true, State: "drained"})
				helper.Write([]byte("x"))
			case "truncated":
				helper.Write([]byte{0, 0, 0, 4, '{', '}'})
			}
		}()
		var receipt, terminal, drained execControlResponse
		if execSupervisorControlRead(client, &receipt) != nil || execSupervisorControlRead(client, &terminal) != nil {
			t.Fatal("terminal lost")
		}
		err := execSupervisorControlRead(client, &drained)
		if mode == "success" {
			if err != nil || !drained.OK || drained.State != "drained" {
				t.Fatal("positive drain missing")
			}
		} else if mode == "failure" {
			if err != nil || drained.OK || drained.Code != "OUTPUT_DRAIN_FAILED" {
				t.Fatal("failure proof lost")
			}
		} else if err == nil {
			t.Fatal("malformed drain became success")
		}
		client.Close()
		<-done
	}
}

func TestExecHTTPRunRejectsMissingExtraAndDuplicateFields(t *testing.T) {
	valid := []string{
		`{"ok":true,"state":"running","pid":1,"startTicks":2}`,
		`{"ok":true,"state":"exited","pid":1,"startTicks":2,"exitCode":7}`,
		`{"ok":true,"state":"drained"}`,
	}
	cases := []struct {
		stage int
		body  string
	}{
		{0, `{"state":"running","pid":1,"startTicks":2}`},
		{0, `{"ok":true,"state":"running","pid":1,"startTicks":2,"code":""}`},
		{0, `{"ok":false,"ok":true,"state":"running","pid":1,"startTicks":2}`},
		{0, `{"ok":true,"state":"running","state":"running","pid":1,"startTicks":2}`},
		{0, `{"ok":true,"state":"running","pid":1,"pid":1,"startTicks":2}`},
		{0, `{"ok":true,"state":"running","pid":1,"startTicks":2,"startTicks":2}`},
		{0, `{"ok":false,"code":"SPAWN_FAILED","state":"spawn-failed","pid":0}`},
		{0, `{"code":"SPAWN_FAILED","state":"spawn-failed"}`},
		{1, `{"ok":true,"state":"exited","pid":1,"startTicks":2,"exitCode":7,"code":""}`},
		{1, `{"ok":true,"state":"exited","pid":1,"startTicks":2,"exitCode":7,"exitCode":7}`},
		{1, `{"code":"CHILD_STATUS_UNKNOWN","state":"unknown","pid":1,"startTicks":2}`},
		{1, `{"ok":false,"code":"CHILD_STATUS_UNKNOWN","state":"unknown","pid":1,"startTicks":2,"exitCode":null}`},
		{2, `{"state":"drained","code":"OUTPUT_DRAIN_FAILED"}`},
		{2, `{"ok":true,"state":"drained","pid":0}`},
		{2, `{"ok":true,"state":"drained","code":""}`},
		{2, `{"ok":true,"ok":true,"state":"drained"}`},
		{2, `{"ok":false,"code":"OUTPUT_DRAIN_FAILED","code":"OUTPUT_DRAIN_FAILED","state":"drained"}`},
		{2, `{"ok":null,"state":"drained"}`},
	}
	for _, scenario := range cases {
		owner, helper := net.Pipe()
		peer, client := net.Pipe()
		done := make(chan struct{})
		go func() { defer owner.Close(); defer peer.Close(); execHTTPRunFrames(owner, peer); close(done) }()
		go func() {
			defer helper.Close()
			for index := 0; index <= scenario.stage; index++ {
				body := valid[index]
				if index == scenario.stage {
					body = scenario.body
				}
				if execSupervisorFrameWrite(helper, json.RawMessage(body)) != nil {
					return
				}
			}
		}()
		for index := 0; index < scenario.stage; index++ {
			var response execControlResponse
			if err := execSupervisorControlRead(client, &response); err != nil {
				t.Fatalf("stage %d rejected earlier valid frame: %v", scenario.stage, err)
			}
		}
		var rejected execControlResponse
		if execSupervisorControlRead(client, &rejected) == nil {
			t.Fatalf("stage %d normalized malformed keys", scenario.stage)
		}
		client.Close()
		<-done
	}
}

func TestExecHTTPInputRejectsDuplicateAndNullFields(t *testing.T) {
	base := `{"identity":"` + strings.Repeat("1", 64) + `","key":"` + strings.Repeat("2", 64) + `","op":"signal","signal":9}`
	for _, body := range []string{
		strings.TrimSuffix(base, "}") + `,"op":"signal"}`,
		strings.TrimSuffix(base, "}") + `,"signal":9}`,
		strings.TrimSuffix(base, "}") + `,"rows":0}`,
		strings.Replace(base, `"identity"`, `"Identity"`, 1),
		strings.Replace(base, `"signal":9`, `"signal":null`, 1),
	} {
		request, _ := http.NewRequest(http.MethodPost, "http://control"+execControlPath, strings.NewReader(body))
		if _, key, valid := execHTTPInput(request); valid || len(key) != 0 {
			t.Fatal("malformed input reached authenticated helper")
		}
	}
}
