//go:build linux

package main

import (
	"bytes"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"net"
	"regexp"
	"time"
)

const execSupervisorReady = "OC_EXEC_READY_V1\n"
const execSupervisorMagic = "OCEXEC01"
const execSupervisorMaxFrame = 1 << 20
const execSupervisorHandshakeTimeout = 5 * time.Second

var execSupervisorIdentity = regexp.MustCompile(`^[0-9a-f]{64}$`)

// Start metadata follows the bootstrap key. Remaining stdin belongs to the child.
type execSupervisorStart struct {
	Argv []string `json:"argv"`
	Pty  bool     `json:"pty"`
	Rows uint16   `json:"rows"`
	Cols uint16   `json:"cols"`
}

type execControlRequest struct {
	Op     string `json:"op"`
	Signal int    `json:"signal,omitempty"`
	Rows   uint16 `json:"rows,omitempty"`
	Cols   uint16 `json:"cols,omitempty"`
}

type execControlResponse struct {
	OK         bool   `json:"ok"`
	Code       string `json:"code,omitempty"`
	State      string `json:"state,omitempty"`
	PID        int    `json:"pid,omitempty"`
	StartTicks uint64 `json:"startTicks,omitempty"`
	ExitCode   *int   `json:"exitCode,omitempty"`
}

func execSupervisorFrameRead(reader io.Reader, value interface{}) error {
	return execSupervisorReadBounded(reader, value, execSupervisorMaxFrame)
}
func execSupervisorControlRead(reader io.Reader, value interface{}) error {
	return execSupervisorReadBounded(reader, value, 4096)
}
func execSupervisorReadBounded(reader io.Reader, value interface{}, maximum uint32) error {
	body, err := execSupervisorReadBody(reader, maximum)
	if err != nil {
		return err
	}
	return execSupervisorDecodeJSON(body, value)
}

func execSupervisorObjectFields(body []byte) (map[string]json.RawMessage, error) {
	decoder := json.NewDecoder(bytes.NewReader(body))
	opening, err := decoder.Token()
	if err != nil || opening != json.Delim('{') {
		return nil, errors.New("INVALID_FRAME")
	}
	fields := make(map[string]json.RawMessage)
	for decoder.More() {
		token, err := decoder.Token()
		if err != nil {
			return nil, errors.New("INVALID_FRAME")
		}
		name, ok := token.(string)
		if !ok {
			return nil, errors.New("INVALID_FRAME")
		}
		if _, exists := fields[name]; exists {
			return nil, errors.New("INVALID_FRAME")
		}
		var value json.RawMessage
		if decoder.Decode(&value) != nil || bytes.Equal(bytes.TrimSpace(value), []byte("null")) {
			return nil, errors.New("INVALID_FRAME")
		}
		fields[name] = value
	}
	if closing, err := decoder.Token(); err != nil || closing != json.Delim('}') {
		return nil, errors.New("INVALID_FRAME")
	}
	var trailing interface{}
	if decoder.Decode(&trailing) != io.EOF {
		return nil, errors.New("INVALID_FRAME")
	}
	return fields, nil
}
func execSupervisorDecodeJSON(body []byte, value interface{}) error {
	if _, err := execSupervisorObjectFields(body); err != nil {
		return err
	}
	decoder := json.NewDecoder(bytes.NewReader(body))
	decoder.DisallowUnknownFields()
	if decoder.Decode(value) != nil {
		return errors.New("INVALID_FRAME")
	}
	return nil
}

func execSupervisorReadBody(reader io.Reader, maximum uint32) ([]byte, error) {
	var prefix [4]byte
	if _, err := io.ReadFull(reader, prefix[:]); err != nil {
		return nil, errors.New("CONTROL_READ_FAILED")
	}
	size := binary.BigEndian.Uint32(prefix[:])
	if size == 0 || size > maximum {
		return nil, errors.New("INVALID_FRAME")
	}
	body := make([]byte, size)
	if _, err := io.ReadFull(reader, body); err != nil {
		return nil, errors.New("CONTROL_READ_FAILED")
	}
	return body, nil
}

func execSupervisorFrameWrite(writer io.Writer, value interface{}) error {
	body, err := json.Marshal(value)
	if err != nil || len(body) == 0 || len(body) > execSupervisorMaxFrame {
		return errors.New("INVALID_FRAME")
	}
	var prefix [4]byte
	binary.BigEndian.PutUint32(prefix[:], uint32(len(body)))
	return execSupervisorWriteAll(writer, append(prefix[:], body...))
}

func execSupervisorWriteAll(writer io.Writer, value []byte) error {
	for len(value) != 0 {
		count, err := writer.Write(value)
		if err != nil || count <= 0 || count > len(value) {
			return errors.New("CONTROL_WRITE_FAILED")
		}
		value = value[count:]
	}
	return nil
}

// The server proves key possession before asking for the client's proof.
func execSupervisorMAC(key []byte, role, identity string, client, server []byte) []byte {
	mac := hmac.New(sha256.New, key)
	mac.Write([]byte("open-compute/exec-supervisor/v1/" + role + "/" + identity))
	mac.Write(client)
	mac.Write(server)
	return mac.Sum(nil)
}

func execSupervisorAuthenticate(connection net.Conn, key []byte, identity string) error {
	if err := connection.SetDeadline(time.Now().Add(execSupervisorHandshakeTimeout)); err != nil {
		return errors.New("AUTH_FAILED")
	}
	var hello [40]byte
	if _, err := io.ReadFull(connection, hello[:]); err != nil || string(hello[:8]) != execSupervisorMagic {
		return errors.New("AUTH_FAILED")
	}
	var challenge [32]byte
	if _, err := rand.Read(challenge[:]); err != nil {
		return errors.New("AUTH_FAILED")
	}
	reply := append([]byte(execSupervisorMagic), challenge[:]...)
	reply = append(reply, execSupervisorMAC(key, "server", identity, hello[8:], challenge[:])...)
	if err := execSupervisorWriteAll(connection, reply); err != nil {
		return errors.New("AUTH_FAILED")
	}
	var proof [32]byte
	if _, err := io.ReadFull(connection, proof[:]); err != nil || !hmac.Equal(proof[:], execSupervisorMAC(key, "client", identity, hello[8:], challenge[:])) {
		return errors.New("AUTH_FAILED")
	}
	if err := connection.SetDeadline(time.Time{}); err != nil {
		return errors.New("AUTH_FAILED")
	}
	return nil
}
