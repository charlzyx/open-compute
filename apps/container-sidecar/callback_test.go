package main

import (
	"bufio"
	"context"
	"io"
	"net"
	"net/http"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestReverseCallbackPool(t *testing.T) {
	callbackConnections = newCallbackPool()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	registered, native := net.Pipe()
	defer native.Close()
	done := make(chan struct{})
	go func() {
		registerCallbackConnection(ctx, registered, strings.Repeat("a", 64), callbackIdleTimeout)
		close(done)
	}()
	_ = native.SetDeadline(time.Now().Add(time.Second))
	response, err := http.ReadResponse(bufio.NewReader(native), nil)
	if err != nil || response.StatusCode != 200 || response.Close {
		t.Fatalf("reverse channel admission failed: %v", err)
	}
	connection, err := takeCallbackConnection(ctx, strings.Repeat("a", 64))
	if err != nil {
		t.Fatal(err)
	}
	go func() { _, _ = connection.Write([]byte("callback")) }()
	bytes := make([]byte, 8)
	if _, err := native.Read(bytes); err != nil || string(bytes) != "callback" {
		t.Fatal("reverse callback data lost")
	}
	_ = connection.Close()
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("reverse callback connection leaked")
	}
}

func TestReverseCallbackRejectsRotatedAndClosedConnections(t *testing.T) {
	callbackConnections = newCallbackPool()
	stale, peer := net.Pipe()
	defer peer.Close()
	closed, closedPeer := net.Pipe()
	defer closedPeer.Close()
	closedLease := &callbackConnection{Conn: closed, token: strings.Repeat("b", 64), done: make(chan struct{}), taken: make(chan struct{}), ready: readyCallback(), handoff: readyCallback()}
	_ = closedLease.Close()
	if admitted, _ := callbackConnections.admit(closedLease); admitted {
		t.Fatal("closed callback consumed admission capacity")
	}
	staleLease := &callbackConnection{Conn: stale, token: strings.Repeat("a", 64), done: make(chan struct{}), taken: make(chan struct{}), ready: readyCallback(), handoff: readyCallback()}
	enqueueCallback(t, staleLease)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()
	if _, err := takeCallbackConnection(ctx, strings.Repeat("b", 64)); err == nil {
		t.Fatal("stale callback accepted")
	}
	select {
	case <-staleLease.done:
	default:
		t.Fatal("rotated callback connection leaked")
	}
}

func TestReverseCallbackProbe(t *testing.T) {
	for _, status := range []int{202, 401} {
		callbackConnections = newCallbackPool()
		provider, native := net.Pipe()
		credentials := controlCredentials{ControlToken: strings.Repeat("a", 64), CallbackToken: strings.Repeat("b", 64)}
		enqueueCallback(t, &callbackConnection{Conn: provider, token: credentials.ControlToken, done: make(chan struct{}), taken: make(chan struct{}), ready: readyCallback(), handoff: readyCallback()})
		go func() {
			defer native.Close()
			request, err := http.ReadRequest(bufio.NewReader(native))
			if err != nil {
				t.Error(err)
				return
			}
			if request.Method != "CONNECT" || request.Host != "open-compute-probe.invalid:1" || request.Header.Get(callbackHeader) != credentials.CallbackToken || request.Header.Get("X-Proto") != "tcp" {
				t.Error("callback probe escaped its closed contract")
			}
			_ = (&http.Response{ProtoMajor: 1, ProtoMinor: 1, StatusCode: status, ContentLength: 0}).Write(native)
		}()
		var output strings.Builder
		writeCallbackProbe(context.Background(), &output, credentials)
		response, err := http.ReadResponse(bufio.NewReader(strings.NewReader(output.String())), nil)
		if err != nil {
			t.Fatal(err)
		}
		expected := 503
		if status == 202 {
			expected = 204
		}
		if response.StatusCode != expected || !response.Close {
			t.Fatalf("callback proof result: %d", response.StatusCode)
		}
	}
}

func TestReverseCallbackHalfCloseAndBufferedBytes(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	client, err := net.Dial("tcp", listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close()
	server, err := listener.Accept()
	if err != nil {
		t.Fatal(err)
	}
	defer server.Close()
	_ = client.SetDeadline(time.Now().Add(time.Second))
	buffered := &bufferedCallbackConnection{Conn: server, reader: strings.NewReader("buffered")}
	connection := &callbackConnection{Conn: buffered, done: make(chan struct{}), taken: make(chan struct{}), ready: readyCallback(), handoff: readyCallback()}
	bytes := make([]byte, 8)
	if _, err := connection.Read(bytes); err != nil || string(bytes) != "buffered" {
		t.Fatal("buffered CONNECT bytes lost")
	}
	if connection.CloseWrite() != nil || connection.CloseRead() != nil {
		t.Fatal("TCP half close failed")
	}
	if _, err := client.Read(bytes); err != io.EOF {
		t.Fatalf("peer did not receive write EOF: %v", err)
	}
	pipe, peer := net.Pipe()
	defer peer.Close()
	defer pipe.Close()
	fallback := &bufferedCallbackConnection{Conn: pipe}
	unsupported := &callbackConnection{Conn: pipe}
	if fallback.CloseRead() == nil || fallback.CloseWrite() == nil || unsupported.CloseRead() == nil || unsupported.CloseWrite() == nil {
		t.Fatal("unsupported half close silently succeeded")
	}
}

func TestReverseCallbackRegistrationCancellation(t *testing.T) {
	for _, full := range []bool{false, true} {
		callbackConnections = newCallbackPool()
		if full {
			for i := 0; i < 16; i++ {
				callbackConnections.entries = append(callbackConnections.entries, nil)
			}
		}
		ctx, cancel := context.WithCancel(context.Background())
		provider, native := net.Pipe()
		done := make(chan struct{})
		go func() {
			registerCallbackConnection(ctx, provider, strings.Repeat("a", 64), callbackIdleTimeout)
			close(done)
		}()
		_ = native.SetDeadline(time.Now().Add(time.Second))
		if !full {
			if _, err := http.ReadResponse(bufio.NewReader(native), nil); err != nil {
				t.Fatal(err)
			}
		}
		cancel()
		select {
		case <-done:
		case <-time.After(time.Second):
			t.Fatal("registration leaked after cancellation")
		}
		_ = native.Close()
	}
	callbackConnections = newCallbackPool()
	provider, native := net.Pipe()
	_ = native.Close()
	registerCallbackConnection(context.Background(), provider, strings.Repeat("a", 64), callbackIdleTimeout)
	previous := *controlAuthFile
	*controlAuthFile = "invalid-private-path"
	defer func() { *controlAuthFile = previous }()
	var output strings.Builder
	probeCallback(context.Background(), &output)
	response, err := http.ReadResponse(bufio.NewReader(strings.NewReader(output.String())), nil)
	if err != nil || response.StatusCode != 503 || !response.Close {
		t.Fatal("missing credentials did not fail closed")
	}
}

func readyCallback() chan struct{} {
	ready := make(chan struct{})
	close(ready)
	return ready
}

func TestReverseCallbackIdleExpiryDoesNotLimitActiveStreams(t *testing.T) {
	callbackConnections = newCallbackPool()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	var connections []net.Conn
	var peers []net.Conn
	var registrations []chan struct{}
	token := strings.Repeat("a", 64)
	for i := 0; i < 32; i++ {
		provider, peer := net.Pipe()
		peers = append(peers, peer)
		done := make(chan struct{})
		registrations = append(registrations, done)
		go func() { registerCallbackConnection(ctx, provider, token, 20*time.Millisecond); close(done) }()
		_ = peer.SetDeadline(time.Now().Add(time.Second))
		if response, err := http.ReadResponse(bufio.NewReader(peer), nil); err != nil || response.StatusCode != 200 {
			t.Fatal("reverse channel not ready")
		}
		connection, err := takeCallbackConnection(ctx, token)
		if err != nil {
			t.Fatal(err)
		}
		connections = append(connections, connection)
	}
	time.Sleep(40 * time.Millisecond)
	for i, connection := range connections {
		go func() { _, _ = connection.Write([]byte("x")) }()
		var byte [1]byte
		if _, err := peers[i].Read(byte[:]); err != nil || byte[0] != 'x' {
			t.Fatal("idle deadline canceled an active callback")
		}
		_ = connection.Close()
		_ = peers[i].Close()
		select {
		case <-registrations[i]:
		case <-time.After(time.Second):
			t.Fatal("active callback leaked")
		}
	}
	callbackConnections = newCallbackPool()
	provider, peer := net.Pipe()
	defer peer.Close()
	done := make(chan struct{})
	go func() { registerCallbackConnection(ctx, provider, token, 10*time.Millisecond); close(done) }()
	if _, err := http.ReadResponse(bufio.NewReader(peer), nil); err != nil {
		t.Fatal(err)
	}
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("idle callback did not expire")
	}
	canceled, stop := context.WithCancel(context.Background())
	stop()
	if _, err := takeCallbackConnection(canceled, token); err == nil {
		t.Fatal("expired callback accepted")
	}
}

func TestReverseCallbackPoolAdmissionExpiresAndUnreadyRequestsCancel(t *testing.T) {
	callbackConnections = newCallbackPool()
	for i := 0; i < 16; i++ {
		callbackConnections.entries = append(callbackConnections.entries, nil)
	}
	provider, peer := net.Pipe()
	defer peer.Close()
	registerCallbackConnection(context.Background(), provider, strings.Repeat("a", 64), 5*time.Millisecond)
	var byte [1]byte
	if _, err := peer.Read(byte[:]); err != io.EOF {
		t.Fatal("full idle pool did not close expired admission")
	}
	callbackConnections = newCallbackPool()
	provider, peer2 := net.Pipe()
	defer peer2.Close()
	registerCallbackConnection(context.Background(), provider, strings.Repeat("a", 64), 5*time.Millisecond)
	if _, err := peer2.Read(byte[:]); err != io.EOF {
		t.Fatal("unread readiness response did not expire")
	}
	callbackConnections = newCallbackPool()
	provider, peer3 := net.Pipe()
	defer peer3.Close()
	lease := &callbackConnection{Conn: provider, token: strings.Repeat("a", 64), done: make(chan struct{}), taken: make(chan struct{}), ready: make(chan struct{})}
	enqueueCallback(t, lease)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error)
	go func() { _, err := takeCallbackConnection(ctx, lease.token); done <- err }()
	deadline := time.Now().Add(time.Second)
	for callbackPoolLength(callbackConnections) != 0 && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	cancel()
	if err := <-done; err == nil {
		t.Fatal("unready callback accepted")
	}
	select {
	case <-lease.done:
	default:
		t.Fatal("unready canceled callback leaked")
	}
}

func TestReverseCallbackRetiredIdlePeerIsRemovedBeforeNextEpoch(t *testing.T) {
	callbackConnections = newCallbackPool()
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	token := strings.Repeat("a", 64)
	register := func() (net.Conn, <-chan struct{}) {
		peer, err := net.Dial("tcp", listener.Addr().String())
		if err != nil {
			t.Fatal(err)
		}
		provider, err := listener.Accept()
		if err != nil {
			_ = peer.Close()
			t.Fatal(err)
		}
		done := make(chan struct{})
		go func() {
			registerCallbackConnection(ctx, provider, token, callbackIdleTimeout)
			close(done)
		}()
		t.Cleanup(func() {
			_ = peer.Close()
			cancel()
			select {
			case <-done:
			case <-time.After(time.Second):
				t.Error("owned callback registration did not stop")
			}
		})
		_ = peer.SetDeadline(time.Now().Add(time.Second))
		response, err := http.ReadResponse(bufio.NewReader(peer), nil)
		if err != nil || response.StatusCode != http.StatusOK || response.Close {
			t.Fatal("owned reverse channel readiness failed")
		}
		return peer, done
	}
	oldPeer, oldDone := register()
	_ = oldPeer.Close()
	select {
	case <-oldDone:
	case <-time.After(time.Second):
		t.Fatal("retired idle native peer remained eligible until the 30-second timeout")
	}
	newPeer, _ := register()
	connection, err := takeCallbackConnection(ctx, token)
	if err != nil {
		t.Fatal("replacement native epoch could not obtain its own channel")
	}
	defer connection.Close()
	writeResult := make(chan error, 1)
	go func() {
		_, err := connection.Write([]byte("x"))
		writeResult <- err
	}()
	var payload [1]byte
	if _, err := newPeer.Read(payload[:]); err != nil || payload[0] != 'x' {
		t.Fatal("first callback after retirement selected a stale peer")
	}
	if <-writeResult != nil {
		t.Fatal("replacement channel write failed")
	}
}

func TestReverseCallbackTCPHandoffPreservesFirstProtocolBytes(t *testing.T) {
	callbackConnections = newCallbackPool()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	peer, err := net.Dial("tcp", listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer peer.Close()
	provider, err := listener.Accept()
	if err != nil {
		t.Fatal(err)
	}
	registration := make(chan struct{})
	go func() {
		registerCallbackConnection(ctx, provider, strings.Repeat("a", 64), callbackIdleTimeout)
		close(registration)
	}()
	t.Cleanup(func() {
		cancel()
		_ = peer.Close()
		select {
		case <-registration:
		case <-time.After(time.Second):
			t.Error("TCP registration leaked")
		}
	})
	_ = peer.SetDeadline(time.Now().Add(time.Second))
	reader := bufio.NewReader(peer)
	if response, err := http.ReadResponse(reader, nil); err != nil || response.StatusCode != 200 {
		t.Fatal("TCP reverse channel not ready")
	}
	connection, err := takeCallbackConnection(ctx, strings.Repeat("a", 64))
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	nativeDone := make(chan error, 1)
	go func() {
		request, err := http.ReadRequest(reader)
		if err == nil && (request.Method != "CONNECT" || request.Host != "callback.invalid:80") {
			err = credentialError()
		}
		if err == nil {
			_, err = peer.Write([]byte("HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\npayload"))
		}
		nativeDone <- err
	}()
	if _, err = connection.Write([]byte("CONNECT callback.invalid:80 HTTP/1.1\r\nHost: callback.invalid:80\r\n\r\n")); err != nil {
		t.Fatal(err)
	}
	response, err := http.ReadResponse(bufio.NewReader(connection), nil)
	if err != nil || response.StatusCode != 200 {
		t.Fatal("idle monitor consumed the first formal response byte")
	}
	body, err := io.ReadAll(response.Body)
	_ = response.Body.Close()
	if err != nil || string(body) != "payload" {
		t.Fatal("response payload lost during reader handoff")
	}
	if err := <-nativeDone; err != nil {
		t.Fatal(err)
	}
	_ = peer.Close()
	if _, err := connection.Read(make([]byte, 1)); err != io.EOF {
		t.Fatal("taken TCP channel did not preserve peer EOF")
	}
}

func TestReverseCallbackUnsolicitedIdleDataIsRejected(t *testing.T) {
	callbackConnections = newCallbackPool()
	provider, peer := net.Pipe()
	defer peer.Close()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan struct{})
	go func() {
		registerCallbackConnection(ctx, provider, strings.Repeat("a", 64), callbackIdleTimeout)
		close(done)
	}()
	_ = peer.SetDeadline(time.Now().Add(time.Second))
	if _, err := http.ReadResponse(bufio.NewReader(peer), nil); err != nil {
		t.Fatal(err)
	}
	if _, err := peer.Write([]byte("unsolicited")); err != nil && err != io.ErrClosedPipe {
		t.Fatal(err)
	}
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("unsolicited idle data kept a reusable callback")
	}
	bounded, stop := context.WithTimeout(ctx, 20*time.Millisecond)
	defer stop()
	if _, err := takeCallbackConnection(bounded, strings.Repeat("a", 64)); err == nil {
		t.Fatal("protocol-invalid idle channel accepted")
	}
}

func TestReverseCallbackIncompleteHandoffClosesOnCancellationAndDeadline(t *testing.T) {
	for _, cancelEarly := range []bool{true, false} {
		callbackConnections = newCallbackPool()
		provider, peer := net.Pipe()
		defer peer.Close()
		lease := &callbackConnection{Conn: provider, token: strings.Repeat("a", 64), done: make(chan struct{}), taken: make(chan struct{}), ready: readyCallback(), handoff: make(chan struct{})}
		enqueueCallback(t, lease)
		ctx, cancel := context.WithCancel(context.Background())
		result := make(chan error, 1)
		go func() { _, err := takeCallbackConnection(ctx, lease.token); result <- err }()
		select {
		case <-lease.taken:
		case <-time.After(time.Second):
			t.Fatal("callback not taken")
		}
		if cancelEarly {
			cancel()
		}
		select {
		case err := <-result:
			if err == nil {
				t.Fatal("incomplete reader handoff accepted")
			}
		case <-time.After(6 * time.Second):
			t.Fatal("handoff timeout leaked")
		}
		cancel()
		select {
		case <-lease.done:
		default:
			t.Fatal("failed reader handoff left socket owned")
		}
	}
	callbackConnections = newCallbackPool()
	provider, peer := net.Pipe()
	defer peer.Close()
	lease := &callbackConnection{Conn: provider, token: strings.Repeat("a", 64), done: make(chan struct{}), taken: make(chan struct{}), ready: make(chan struct{}), handoff: make(chan struct{})}
	enqueueCallback(t, lease)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	result := make(chan error, 1)
	go func() { _, err := takeCallbackConnection(ctx, lease.token); result <- err }()
	deadline := time.Now().Add(time.Second)
	for callbackPoolLength(callbackConnections) != 0 && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if callbackPoolLength(callbackConnections) != 0 {
		t.Fatal("unready callback was not selected within its bounded window")
	}
	_ = lease.Close()
	cancel()
	if err := <-result; err == nil {
		t.Fatal("closed unready channel accepted")
	}
}

// The stream contract permits data and an error in the same Read result.
type idleByteAndTimeoutConnection struct {
	net.Conn
	released chan struct{}
	once     sync.Once
}

func (connection *idleByteAndTimeoutConnection) Read(bytes []byte) (int, error) {
	<-connection.released
	bytes[0] = 'x'
	return 1, context.DeadlineExceeded
}
func (connection *idleByteAndTimeoutConnection) SetReadDeadline(deadline time.Time) error {
	connection.once.Do(func() { close(connection.released) })
	return connection.Conn.SetReadDeadline(deadline)
}
func (connection *idleByteAndTimeoutConnection) Close() error {
	connection.once.Do(func() { close(connection.released) })
	return connection.Conn.Close()
}

func TestReverseCallbackIdleByteWithTimeoutCannotAuthorizeHandoff(t *testing.T) {
	callbackConnections = newCallbackPool()
	provider, peer := net.Pipe()
	defer peer.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()
	connection := &idleByteAndTimeoutConnection{Conn: provider, released: make(chan struct{})}
	registration := make(chan struct{})
	go func() {
		registerCallbackConnection(ctx, connection, strings.Repeat("a", 64), callbackIdleTimeout)
		close(registration)
	}()
	if _, err := http.ReadResponse(bufio.NewReader(peer), nil); err != nil {
		t.Fatal(err)
	}
	if taken, err := takeCallbackConnection(ctx, strings.Repeat("a", 64)); err == nil {
		taken.Close()
		t.Fatal("idle byte plus timeout was swallowed and authorized")
	}
	select {
	case <-registration:
	case <-time.After(time.Second):
		t.Fatal("rejected handoff reader leaked")
	}
}

func enqueueCallback(t *testing.T, connection *callbackConnection) {
	t.Helper()
	connection.pool = callbackConnections
	if admitted, _ := connection.pool.admit(connection); !admitted {
		t.Fatal("test callback admission failed")
	}
}

func callbackPoolLength(pool *callbackPool) int {
	pool.mutex.Lock()
	defer pool.mutex.Unlock()
	return len(pool.entries)
}

func TestRetiredUnusedTCPChannelsReleaseAdmissionCapacity(t *testing.T) {
	callbackConnections = newCallbackPool()
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	token := strings.Repeat("a", 64)
	for i := 0; i < 17; i++ {
		peer, err := net.Dial("tcp", listener.Addr().String())
		if err != nil {
			t.Fatal(err)
		}
		provider, err := listener.Accept()
		if err != nil {
			_ = peer.Close()
			t.Fatal(err)
		}
		done := make(chan struct{})
		go func() { registerCallbackConnection(ctx, provider, token, callbackIdleTimeout); close(done) }()
		t.Cleanup(func() {
			_ = peer.Close()
			cancel()
			select {
			case <-done:
			case <-time.After(time.Second):
				t.Error("owned registration did not stop")
			}
		})
		_ = peer.SetReadDeadline(time.Now().Add(200 * time.Millisecond))
		response, err := http.ReadResponse(bufio.NewReader(peer), nil)
		if err != nil || response.StatusCode != http.StatusOK || response.Close {
			t.Fatalf("channel %d failed readiness despite all previous peers closed and registrations joined; retained slots=%d", i+1, callbackPoolLength(callbackConnections))
		}
		_ = peer.Close()
		select {
		case <-done:
		case <-time.After(time.Second):
			t.Fatal("closed peer registration did not stop")
		}
	}
}

// Each helper owns a real TCP registration and joins it during cleanup.
func callbackTCPPeer(t *testing.T, ctx context.Context, token string) (net.Conn, <-chan struct{}) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	peer, err := net.Dial("tcp", listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	provider, err := listener.Accept()
	if err != nil {
		peer.Close()
		t.Fatal(err)
	}
	owned, cancel := context.WithCancel(ctx)
	done := make(chan struct{})
	go func() { registerCallbackConnection(owned, provider, token, callbackIdleTimeout); close(done) }()
	t.Cleanup(func() {
		cancel()
		peer.Close()
		select {
		case <-done:
		case <-time.After(time.Second):
			t.Error("owned TCP registration leaked")
		}
	})
	return peer, done
}

func callbackTCPReady(t *testing.T, peer net.Conn) {
	t.Helper()
	peer.SetReadDeadline(time.Now().Add(time.Second))
	response, err := http.ReadResponse(bufio.NewReader(peer), nil)
	if err != nil || response.StatusCode != http.StatusOK || response.Close {
		t.Fatal("owned TCP callback did not become ready")
	}
	peer.SetReadDeadline(time.Time{})
}

func TestReverseCallbackLiveTCPPoolBoundsAndReleasesAdmission(t *testing.T) {
	callbackConnections = newCallbackPool()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	token := strings.Repeat("a", 64)
	var peers []net.Conn
	var registrations []<-chan struct{}
	for i := 0; i < 16; i++ {
		peer, done := callbackTCPPeer(t, ctx, token)
		callbackTCPReady(t, peer)
		peers = append(peers, peer)
		registrations = append(registrations, done)
	}
	blockedCtx, stop := context.WithCancel(ctx)
	blocked, blockedDone := callbackTCPPeer(t, blockedCtx, token)
	blocked.SetReadDeadline(time.Now().Add(30 * time.Millisecond))
	if _, err := http.ReadResponse(bufio.NewReader(blocked), nil); err == nil {
		t.Fatal("17th live unused callback exceeded capacity")
	}
	stop()
	select {
	case <-blockedDone:
	case <-time.After(time.Second):
		t.Fatal("blocked admission ignored cancellation")
	}
	if callbackPoolLength(callbackConnections) != 16 {
		t.Fatal("canceling blocked admission altered live pool")
	}
	replacement, replacementDone := callbackTCPPeer(t, ctx, token)
	replacement.SetReadDeadline(time.Now().Add(30 * time.Millisecond))
	if _, err := http.ReadResponse(bufio.NewReader(replacement), nil); err == nil {
		t.Fatal("live pool admitted beyond its limit")
	}
	peers[0].Close()
	select {
	case <-registrations[0]:
	case <-time.After(time.Second):
		t.Fatal("retired live peer did not release admission")
	}
	callbackTCPReady(t, replacement)
	if callbackPoolLength(callbackConnections) != 16 {
		t.Fatal("replacement readiness changed bounded capacity")
	}
	replacement.Close()
	select {
	case <-replacementDone:
	case <-time.After(time.Second):
		t.Fatal("replacement registration leaked")
	}
	for i, peer := range peers {
		peer.Close()
		select {
		case <-registrations[i]:
		case <-time.After(time.Second):
			t.Fatal("live pool cleanup leaked")
		}
	}
	if callbackPoolLength(callbackConnections) != 0 {
		t.Fatal("closed TCP entries retained pool capacity")
	}
}

func TestReverseCallbackTCPRotationAndOwnerPoolIsolation(t *testing.T) {
	callbackConnections = newCallbackPool()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	stale, staleDone := callbackTCPPeer(t, ctx, strings.Repeat("a", 64))
	callbackTCPReady(t, stale)
	current, currentDone := callbackTCPPeer(t, ctx, strings.Repeat("b", 64))
	callbackTCPReady(t, current)
	connection, err := takeCallbackConnection(ctx, strings.Repeat("b", 64))
	if err != nil {
		t.Fatal("new credentials could not take their own TCP channel")
	}
	select {
	case <-staleDone:
	case <-time.After(time.Second):
		t.Fatal("token rotation retained old TCP registration")
	}
	connection.Close()
	current.Close()
	select {
	case <-currentDone:
	case <-time.After(time.Second):
		t.Fatal("rotated TCP registration leaked")
	}
	oldPool := callbackConnections
	old, oldDone := callbackTCPPeer(t, ctx, strings.Repeat("b", 64))
	callbackTCPReady(t, old)
	callbackConnections = newCallbackPool()
	next, nextDone := callbackTCPPeer(t, ctx, strings.Repeat("b", 64))
	callbackTCPReady(t, next)
	old.Close()
	select {
	case <-oldDone:
	case <-time.After(time.Second):
		t.Fatal("old pool owner did not retire")
	}
	if callbackPoolLength(oldPool) != 0 || callbackPoolLength(callbackConnections) != 1 {
		t.Fatal("old retirement removed a different pool's lease")
	}
	next.Close()
	select {
	case <-nextDone:
	case <-time.After(time.Second):
		t.Fatal("new pool registration leaked")
	}
}

func TestReverseCallbackTCPTakeCloseCancellationRace(t *testing.T) {
	callbackConnections = newCallbackPool()
	token := strings.Repeat("a", 64)
	for i := 0; i < 16; i++ {
		ctx, cancel := context.WithCancel(context.Background())
		peer, registration := callbackTCPPeer(t, ctx, token)
		callbackTCPReady(t, peer)
		begin := make(chan struct{})
		var tasks sync.WaitGroup
		tasks.Add(3)
		go func() {
			defer tasks.Done()
			<-begin
			connection, _ := takeCallbackConnection(ctx, token)
			if connection != nil {
				connection.Close()
			}
		}()
		go func() { defer tasks.Done(); <-begin; peer.Close() }()
		go func() { defer tasks.Done(); <-begin; cancel() }()
		close(begin)
		joined := make(chan struct{})
		go func() { tasks.Wait(); close(joined) }()
		select {
		case <-joined:
		case <-time.After(time.Second):
			t.Fatal("take/close/cancellation race retained operation")
		}
		select {
		case <-registration:
		case <-time.After(time.Second):
			t.Fatal("take/close/cancellation race retained registration")
		}
		if callbackPoolLength(callbackConnections) != 0 {
			t.Fatal("take/close/cancellation left an unused slot")
		}
	}
}

func TestReverseCallbackAlreadyCanceledTakePreservesOtherAdmission(t *testing.T) {
	callbackConnections = newCallbackPool()
	provider, peer := net.Pipe()
	defer peer.Close()
	lease := &callbackConnection{Conn: provider, token: strings.Repeat("a", 64), done: make(chan struct{}), taken: make(chan struct{}), ready: readyCallback(), handoff: readyCallback()}
	enqueueCallback(t, lease)
	defer lease.Close()
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if connection, err := takeCallbackConnection(ctx, lease.token); err == nil {
		connection.Close()
		t.Fatal("already canceled request took a ready channel")
	}
	if callbackPoolLength(callbackConnections) != 1 {
		t.Fatal("already canceled request consumed another unused admission")
	}
}
