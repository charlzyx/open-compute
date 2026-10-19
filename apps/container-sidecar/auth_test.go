package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func testCredentials() controlCredentials {
	return controlCredentials{ControlToken: strings.Repeat("a", 64), CallbackToken: strings.Repeat("b", 64)}
}

func writeCredentials(t *testing.T, path string, credentials controlCredentials) {
	t.Helper()
	body, err := json.Marshal(credentials)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, body, 0600); err != nil {
		t.Fatal(err)
	}
}

func TestControlCredentialsRotateAndRevoke(t *testing.T) {
	path := filepath.Join(t.TempDir(), "control.json")
	credentials := testCredentials()
	writeCredentials(t, path, credentials)
	loaded, err := readControlCredentials(path)
	if err != nil || loaded != credentials {
		t.Fatalf("credential load failed: %v", err)
	}
	headers := make(http.Header)
	headers.Set(controlHeader, credentials.ControlToken)
	if !validControlRequest(headers, loaded) {
		t.Fatal("valid credential rejected")
	}
	replacement := controlCredentials{ControlToken: strings.Repeat("c", 64), CallbackToken: strings.Repeat("d", 64)}
	writeCredentials(t, path, replacement)
	loaded, err = readControlCredentials(path)
	if err != nil || validControlRequest(headers, loaded) {
		t.Fatal("old generation accepted")
	}
	headers.Set(controlHeader, replacement.ControlToken)
	if !validControlRequest(headers, loaded) {
		t.Fatal("replacement rejected")
	}
	headers.Add(controlHeader, replacement.ControlToken)
	if validControlRequest(headers, loaded) {
		t.Fatal("duplicate credentials accepted")
	}
	headers.Set(controlHeader, replacement.CallbackToken)
	if validControlRequest(headers, loaded) {
		t.Fatal("callback credential delegated control")
	}
	headers.Set(controlHeader, replacement.ControlToken[:63])
	if validControlRequest(headers, loaded) {
		t.Fatal("truncated credential accepted")
	}
	if validControlRequest(make(http.Header), loaded) {
		t.Fatal("missing credential accepted")
	}
}

func TestControlCredentialsFailClosedWithoutLeakingSecrets(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "control.json")
	credential := testCredentials()
	for _, body := range []string{
		"", "{", "[]", "null", "{}", strings.Repeat("x", 2049),
		`{"controlToken":"` + credential.ControlToken + `","callbackToken":"` + credential.CallbackToken + `","unknown":true}`,
		`{"controlToken":"` + credential.ControlToken + `","callbackToken":"` + credential.ControlToken + `"}`,
		`{"controlToken":"` + strings.Repeat("A", 64) + `","callbackToken":"` + credential.CallbackToken + `"}`,
		`{"controlToken":"` + credential.ControlToken + `","callbackToken":"bad"}`,
		`{"controlToken":"` + credential.ControlToken + `","callbackToken":"` + credential.CallbackToken + `"} {}`,
	} {
		if err := os.WriteFile(path, []byte(body), 0600); err != nil {
			t.Fatal(err)
		}
		_, err := readControlCredentials(path)
		if err == nil || err.Error() != "Container sidecar credentials unavailable" {
			t.Fatalf("malformed credentials accepted or exposed: %v", err)
		}
	}
	writeCredentials(t, path, credential)
	if err := os.Chmod(path, 0644); err != nil {
		t.Fatal(err)
	}
	if _, err := readControlCredentials(path); err == nil {
		t.Fatal("world-readable credentials accepted")
	}
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	if _, err := readControlCredentials(path); err == nil {
		t.Fatal("missing credentials accepted")
	}
	if _, err := readControlCredentials(root); err == nil {
		t.Fatal("directory credentials accepted")
	}
	secret := filepath.Join(root, "actual")
	writeCredentials(t, secret, credential)
	if err := os.Symlink(secret, path); err != nil {
		t.Fatal(err)
	}
	if _, err := readControlCredentials(path); err == nil {
		t.Fatal("symlink credentials accepted")
	}
	previous := *controlAuthFile
	*controlAuthFile = path
	defer func() { *controlAuthFile = previous }()
	if _, err := loadControlCredentials(); err == nil {
		t.Fatal("alternate credential path accepted")
	}
	if authorizeControlRequest(make(http.Header)) {
		t.Fatal("invalid credential path authorized")
	}
}

func TestControlProfileIsExactAndSecretFree(t *testing.T) {
	var output bytes.Buffer
	if err := writeControlProfile(&output); err != nil {
		t.Fatal(err)
	}
	response, err := http.ReadResponse(bufio.NewReader(&output), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil || response.StatusCode != 200 || !response.Close || response.Header.Get("Content-Type") != "application/json" || string(body) != controlProfileBody {
		t.Fatal("provider profile mismatch")
	}
}

func TestControlProfileDoesNotReuseAClosedConnection(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	done := make(chan error, 1)
	go func() {
		for i := 0; i < 2; i++ {
			connection, err := listener.Accept()
			if err != nil {
				done <- err
				return
			}
			_ = connection.SetDeadline(time.Now().Add(time.Second))
			_, err = http.ReadRequest(bufio.NewReader(connection))
			if err == nil {
				err = writeControlProfile(connection)
			}
			_ = connection.Close()
			if err != nil {
				done <- err
				return
			}
		}
		done <- nil
	}()
	transport := &http.Transport{}
	defer transport.CloseIdleConnections()
	client := &http.Client{Transport: transport, Timeout: time.Second}
	for i := 0; i < 2; i++ {
		response, err := client.Get("http://" + listener.Addr().String() + controlProfilePath)
		if err != nil {
			t.Fatal(err)
		}
		body, err := io.ReadAll(response.Body)
		_ = response.Body.Close()
		if err != nil || !response.Close || string(body) != controlProfileBody {
			t.Fatal("closed profile connection advertised as reusable")
		}
	}
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestControlRequestParsingIsBoundedAndPreservesTunnelBytes(t *testing.T) {
	for _, input := range []string{
		"CONNECT /open-compute/callback-channel HTTP/1.1\r\nHost: sidecar\r\n\r\npayload",
		"GET / HTTP/1.1\r\nHost: sidecar\r\nX-Large: " + strings.Repeat("x", 9000) + "\r\n\r\n",
		"malformed\r\n\r\n",
	} {
		provider, client := net.Pipe()
		go func() { _, _ = client.Write([]byte(input)) }()
		request, reader, err := readControlRequest(provider)
		_ = client.Close()
		if strings.HasPrefix(input, "CONNECT") {
			if err != nil || request.RequestURI != callbackChannelTarget {
				t.Fatal("valid reverse control request rejected")
			}
			bytes, err := io.ReadAll(reader)
			if err != nil || string(bytes) != "payload" {
				t.Fatal("buffered tunnel bytes lost")
			}
		} else if err == nil {
			t.Fatal("unbounded or malformed control request accepted")
		}
		_ = provider.Close()
	}
}

func TestControlRequestRejectsAnUnusableSocket(t *testing.T) {
	provider, peer := net.Pipe()
	_ = provider.Close()
	_ = peer.Close()
	if _, _, err := readControlRequest(provider); err == nil {
		t.Fatal("unusable control socket accepted")
	}
}
