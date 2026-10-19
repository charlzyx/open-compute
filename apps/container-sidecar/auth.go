package main

import (
	"bufio"
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"io"
	"net"
	"net/http"
	"os"
	"strings"
	"syscall"
	"time"
)

const controlCredentialsPath = "/run/open-compute/control.json"
const controlHeader = "X-Open-Compute-Container-Control"
const callbackHeader = "X-Open-Compute-Container-Callback"

var controlAuthFile = flag.String("control-auth-file", controlCredentialsPath, "fixed provider credential file")

type controlCredentials struct {
	ControlToken  string `json:"controlToken"`
	CallbackToken string `json:"callbackToken"`
}

func credentialError() error {
	return errors.New("Container sidecar credentials unavailable")
}

// Read on every request so Broker restart and workerd generation changes revoke old credentials.
func readControlCredentials(path string) (controlCredentials, error) {
	var result controlCredentials
	file, err := os.OpenFile(path, os.O_RDONLY|syscall.O_NOFOLLOW, 0)
	if err != nil {
		return result, credentialError()
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil || !info.Mode().IsRegular() || info.Mode().Perm() != 0600 || info.Size() > 2048 {
		return result, credentialError()
	}
	decoder := json.NewDecoder(io.LimitReader(file, 2049))
	decoder.DisallowUnknownFields()
	if decoder.Decode(&result) != nil || decoder.Decode(new(any)) != io.EOF {
		return controlCredentials{}, credentialError()
	}
	for _, token := range []string{result.ControlToken, result.CallbackToken} {
		decoded, err := hex.DecodeString(token)
		if err != nil || len(decoded) != 32 || hex.EncodeToString(decoded) != token {
			return controlCredentials{}, credentialError()
		}
	}
	if subtle.ConstantTimeCompare([]byte(result.ControlToken), []byte(result.CallbackToken)) == 1 {
		return controlCredentials{}, credentialError()
	}
	return result, nil
}

func loadControlCredentials() (controlCredentials, error) {
	if *controlAuthFile != controlCredentialsPath {
		return controlCredentials{}, credentialError()
	}
	return readControlCredentials(controlCredentialsPath)
}

func validControlRequest(headers http.Header, credentials controlCredentials) bool {
	values := headers.Values(controlHeader)
	return len(values) == 1 && len(values[0]) == 64 &&
		subtle.ConstantTimeCompare([]byte(values[0]), []byte(credentials.ControlToken)) == 1
}

func authorizeControlRequest(headers http.Header) bool {
	credentials, err := loadControlCredentials()
	return err == nil && validControlRequest(headers, credentials)
}

const controlProfilePath = "/open-compute/profile"
const controlProfileBody = `{"protocol":"open-compute-container-sidecar","version":2}`

func writeControlProfile(writer io.Writer) error {
	return (&http.Response{
		ProtoMajor: 1, ProtoMinor: 1, StatusCode: http.StatusOK, Close: true,
		Header:        http.Header{"Content-Type": {"application/json"}},
		ContentLength: int64(len(controlProfileBody)),
		Body:          io.NopCloser(strings.NewReader(controlProfileBody)),
	}).Write(writer)
}

// Bound pre-authentication parsing while preserving buffered CONNECT payload bytes.
func readControlRequest(connection net.Conn) (*http.Request, *bufio.Reader, error) {
	if connection.SetReadDeadline(time.Now().Add(5*time.Second)) != nil {
		return nil, nil, credentialError()
	}
	limited := &io.LimitedReader{R: connection, N: 8193}
	reader := bufio.NewReader(limited)
	request, err := http.ReadRequest(reader)
	if err != nil || limited.N == 0 {
		return nil, nil, credentialError()
	}
	limited.N = 1<<63 - 1
	if connection.SetReadDeadline(time.Time{}) != nil {
		return nil, nil, credentialError()
	}
	return request, reader, nil
}
