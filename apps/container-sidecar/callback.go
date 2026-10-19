package main

import (
	"bufio"
	"context"
	"crypto/subtle"
	"io"
	"net"
	"net/http"
	"sync"
	"time"
)

const callbackChannelTarget = "/open-compute/callback-channel"
const callbackIdleTimeout = 30 * time.Second
const callbackProbePath = "/open-compute/probe-callback"

// Each reverse connection carries one request. A bounded pool needs no stream multiplexer.
type callbackConnection struct {
	net.Conn
	token   string
	done    chan struct{}
	taken   chan struct{}
	ready   chan struct{}
	handoff chan struct{}
	once    sync.Once
	pool    *callbackPool
}

func (connection *callbackConnection) CloseRead() error {
	if socket, ok := connection.Conn.(interface{ CloseRead() error }); ok {
		return socket.CloseRead()
	}
	return credentialError()
}
func (connection *callbackConnection) CloseWrite() error {
	if socket, ok := connection.Conn.(interface{ CloseWrite() error }); ok {
		return socket.CloseWrite()
	}
	return credentialError()
}

func (connection *callbackConnection) Close() error {
	connection.once.Do(func() {
		close(connection.done)
		if connection.pool != nil {
			connection.pool.remove(connection)
		}
	})
	return connection.Conn.Close()
}

type bufferedCallbackConnection struct {
	net.Conn
	reader io.Reader
}

func (connection *bufferedCallbackConnection) CloseRead() error {
	if socket, ok := connection.Conn.(interface{ CloseRead() error }); ok {
		return socket.CloseRead()
	}
	return credentialError()
}
func (connection *bufferedCallbackConnection) CloseWrite() error {
	if socket, ok := connection.Conn.(interface{ CloseWrite() error }); ok {
		return socket.CloseWrite()
	}
	return credentialError()
}

func (connection *bufferedCallbackConnection) Read(bytes []byte) (int, error) {
	return connection.reader.Read(bytes)
}

// The pool owns only unused connections. Taking or closing one releases its slot.
type callbackPool struct {
	mutex   sync.Mutex
	entries []*callbackConnection
	changed chan struct{}
}

func newCallbackPool() *callbackPool { return &callbackPool{changed: make(chan struct{})} }

func (pool *callbackPool) notifyLocked() {
	close(pool.changed)
	pool.changed = make(chan struct{})
}

func (pool *callbackPool) admit(connection *callbackConnection) (bool, <-chan struct{}) {
	pool.mutex.Lock()
	defer pool.mutex.Unlock()
	select {
	case <-connection.done:
		return false, pool.changed
	default:
	}
	if len(pool.entries) == 16 {
		return false, pool.changed
	}
	pool.entries = append(pool.entries, connection)
	pool.notifyLocked()
	return true, pool.changed
}

func (pool *callbackPool) remove(connection *callbackConnection) {
	pool.mutex.Lock()
	defer pool.mutex.Unlock()
	for index, entry := range pool.entries {
		if entry == connection {
			copy(pool.entries[index:], pool.entries[index+1:])
			pool.entries[len(pool.entries)-1] = nil
			pool.entries = pool.entries[:len(pool.entries)-1]
			pool.notifyLocked()
			return
		}
	}
}

func (pool *callbackPool) take() (*callbackConnection, <-chan struct{}) {
	pool.mutex.Lock()
	defer pool.mutex.Unlock()
	if len(pool.entries) == 0 {
		return nil, pool.changed
	}
	connection := pool.entries[0]
	copy(pool.entries, pool.entries[1:])
	pool.entries[len(pool.entries)-1] = nil
	pool.entries = pool.entries[:len(pool.entries)-1]
	pool.notifyLocked()
	return connection, pool.changed
}

var callbackConnections = newCallbackPool()

func registerCallbackConnection(ctx context.Context, connection net.Conn, token string, idleTimeout time.Duration) {
	lease := &callbackConnection{Conn: connection, token: token, done: make(chan struct{}), taken: make(chan struct{}), ready: make(chan struct{}), handoff: make(chan struct{}), pool: callbackConnections}
	deadline := time.NewTimer(idleTimeout)
	defer deadline.Stop()
	defer lease.Close()
	stopCancellation := context.AfterFunc(ctx, func() { _ = lease.Close() })
	defer stopCancellation()
	for {
		select {
		case <-lease.done:
			return
		case <-ctx.Done():
			return
		case <-deadline.C:
			return
		default:
		}
		admitted, changed := lease.pool.admit(lease)
		if admitted {
			break
		}
		select {
		case <-changed:
		case <-lease.done:
			return
		case <-ctx.Done():
			return
		case <-deadline.C:
			return
		}
	}
	if connection.SetWriteDeadline(time.Now().Add(idleTimeout)) != nil ||
		(&http.Response{ProtoMajor: 1, ProtoMinor: 1, StatusCode: http.StatusOK}).Write(connection) != nil ||
		connection.SetWriteDeadline(time.Time{}) != nil {
		return
	}
	// No protocol response is valid before a callback has been taken and written.
	// Join this idle reader before transferring exclusive read ownership to the caller.
	idleResult := make(chan error, 1)
	idleFinished := make(chan struct{})
	go func() {
		defer close(idleFinished)
		var unexpected [1]byte
		count, err := connection.Read(unexpected[:])
		if count != 0 || err == nil {
			err = credentialError()
		}
		idleResult <- err
	}()
	defer func() { _ = lease.Close(); <-idleFinished }()
	close(lease.ready)
	select {
	case <-lease.taken:
		if connection.SetReadDeadline(time.Now()) != nil {
			return
		}
		failure := <-idleResult
		if timeout, ok := failure.(net.Error); !ok || !timeout.Timeout() {
			return
		}
		if connection.SetReadDeadline(time.Time{}) != nil {
			return
		}
		close(lease.handoff)
		deadline.Stop()
		select {
		case <-lease.done:
		case <-ctx.Done():
		}
	case <-idleResult:
	case <-lease.done:
	case <-ctx.Done():
	case <-deadline.C:
	}
}

func takeCallbackConnection(ctx context.Context, token string) (net.Conn, error) {
	deadline := time.NewTimer(5 * time.Second)
	defer deadline.Stop()
	pool := callbackConnections
	for {
		select {
		case <-ctx.Done():
			return nil, credentialError()
		case <-deadline.C:
			return nil, credentialError()
		default:
		}
		connection, changed := pool.take()
		if connection != nil {
			select {
			case <-connection.done:
				continue
			default:
			}
			if subtle.ConstantTimeCompare([]byte(connection.token), []byte(token)) != 1 {
				_ = connection.Close()
				continue
			}
			select {
			case <-connection.ready:
			case <-connection.done:
				continue
			case <-ctx.Done():
				_ = connection.Close()
				return nil, credentialError()
			case <-deadline.C:
				_ = connection.Close()
				return nil, credentialError()
			}
			close(connection.taken)
			select {
			case <-connection.handoff:
				if ctx.Err() != nil {
					_ = connection.Close()
					return nil, credentialError()
				}
				return connection, nil
			case <-connection.done:
				continue
			case <-ctx.Done():
				_ = connection.Close()
				return nil, credentialError()
			case <-deadline.C:
				_ = connection.Close()
				return nil, credentialError()
			}
		} else {
			select {
			case <-changed:
			case <-ctx.Done():
				return nil, credentialError()
			case <-deadline.C:
				return nil, credentialError()
			}
		}
	}
}

func probeCallback(ctx context.Context, writer io.Writer) {
	credentials, err := loadControlCredentials()
	if err != nil {
		_ = (&http.Response{ProtoMajor: 1, ProtoMinor: 1, StatusCode: http.StatusServiceUnavailable, Close: true}).Write(writer)
		return
	}
	writeCallbackProbe(ctx, writer, credentials)
}

func writeCallbackProbe(ctx context.Context, writer io.Writer, credentials controlCredentials) {
	status := http.StatusServiceUnavailable
	{
		connection, err := takeCallbackConnection(ctx, credentials.ControlToken)
		if err == nil {
			defer connection.Close()
			_ = connection.SetDeadline(time.Now().Add(5 * time.Second))
			request, _ := http.NewRequest(http.MethodConnect, "http://open-compute-probe.invalid:1", nil)
			request.Header.Set(callbackHeader, credentials.CallbackToken)
			request.Header.Set("X-Proto", "tcp")
			if request.Write(connection) == nil {
				response, err := http.ReadResponse(bufio.NewReader(connection), request)
				if err == nil {
					_ = response.Body.Close()
					if response.StatusCode == http.StatusAccepted {
						status = http.StatusNoContent
					}
				}
			}
		}
	}
	_ = (&http.Response{ProtoMajor: 1, ProtoMinor: 1, StatusCode: status, Close: true}).Write(writer)
}
