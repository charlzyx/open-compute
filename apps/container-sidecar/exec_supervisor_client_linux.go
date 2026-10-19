//go:build linux

package main

import (
	"context"
	"crypto/hmac"
	"crypto/rand"
	"errors"
	"io"
	"net"
	"time"
)

// requestExecSupervisor never authenticates to a rebound endpoint without its server proof.
func requestExecSupervisor(ctx context.Context, identity string, key []byte, request execControlRequest) (execControlResponse, error) {
	connection, err := openExecSupervisor(ctx, identity, key, request)
	if err != nil {
		return execControlResponse{}, err
	}
	defer connection.Close()
	var response execControlResponse
	if err := execSupervisorControlRead(connection, &response); err != nil {
		return execControlResponse{}, err
	}
	return response, nil
}

type execSupervisorConnection struct {
	net.Conn
	stop func() bool
}

func (connection *execSupervisorConnection) Close() error {
	connection.stop()
	return connection.Conn.Close()
}
func openExecSupervisor(ctx context.Context, identity string, key []byte, request execControlRequest) (*execSupervisorConnection, error) {
	if !execSupervisorIdentity.MatchString(identity) || len(key) != 32 {
		return nil, errors.New("INVALID_CONTROL_IDENTITY")
	}
	var dialer net.Dialer
	raw, err := dialer.DialContext(ctx, "unix", "@oc-exec-v1-"+identity)
	if err != nil {
		return nil, errors.New("CONTROL_UNAVAILABLE")
	}
	connection := &execSupervisorConnection{Conn: raw, stop: context.AfterFunc(ctx, func() { raw.Close() })}
	if err := execSupervisorClientAuthenticate(connection, key, identity); err != nil {
		connection.Close()
		return nil, err
	}
	if connection.SetWriteDeadline(time.Now().Add(execSupervisorHandshakeTimeout)) != nil {
		connection.Close()
		return nil, errors.New("CONTROL_WRITE_FAILED")
	}
	if err := execSupervisorFrameWrite(connection, request); err != nil {
		connection.Close()
		return nil, err
	}
	return connection, nil
}

func execSupervisorClientAuthenticate(connection net.Conn, key []byte, identity string) error {
	if err := connection.SetDeadline(time.Now().Add(execSupervisorHandshakeTimeout)); err != nil {
		return errors.New("AUTH_FAILED")
	}
	var challenge [32]byte
	if _, err := rand.Read(challenge[:]); err != nil {
		return errors.New("AUTH_FAILED")
	}
	if err := execSupervisorWriteAll(connection, append([]byte(execSupervisorMagic), challenge[:]...)); err != nil {
		return errors.New("AUTH_FAILED")
	}
	var reply [72]byte
	if _, err := io.ReadFull(connection, reply[:]); err != nil || string(reply[:8]) != execSupervisorMagic || !hmac.Equal(reply[40:], execSupervisorMAC(key, "server", identity, challenge[:], reply[8:40])) {
		return errors.New("AUTH_FAILED")
	}
	if err := execSupervisorWriteAll(connection, execSupervisorMAC(key, "client", identity, challenge[:], reply[8:40])); err != nil {
		return errors.New("AUTH_FAILED")
	}
	if connection.SetDeadline(time.Time{}) != nil {
		return errors.New("AUTH_FAILED")
	}
	return nil
}
