package main

import (
	"bufio"
	"context"
	"io"
	"net"
	"net/http"
	"strings"
	"testing"
	"time"
)

func TestIngressClosingResponses(t *testing.T) {
	egress, err := newEgressConfiguration(1)
	if err != nil {
		t.Fatal(err)
	}
	server := &ingressServer{egress: egress}
	for _, test := range []struct {
		method, path, body string
		status             int
	}{
		{"GET", "/unknown", "", 404},
		{"PUT", "/unknown", "", 404},
		{"PUT", "/egress", "invalid", 400},
		{"PUT", "/egress", "{}", 400},
		{"PUT", "/egress", `{"port":0}`, 400},
		{"PUT", "/egress", `{"internet":{"enabled":false}}`, 204},
		{"CONNECT", "/", "", 400},
	} {
		t.Run(test.method+test.path+test.body, func(t *testing.T) {
			provider, client := net.Pipe()
			defer client.Close()
			_ = client.SetDeadline(time.Now().Add(time.Second))
			request, err := http.NewRequest(test.method, "http://sidecar"+test.path, strings.NewReader(test.body))
			if err != nil {
				t.Fatal(err)
			}
			done := make(chan struct{})
			go func() {
				defer close(done)
				defer provider.Close()
				switch test.method {
				case "GET":
					server.handleGet(provider, request)
				case "PUT":
					server.handlePut(provider, request)
				case "CONNECT":
					server.handleConnect(context.Background(), provider, bufio.NewReader(provider), request)
				}
			}()
			response, err := http.ReadResponse(bufio.NewReader(client), request)
			if err != nil {
				t.Fatal(err)
			}
			_, err = io.ReadAll(response.Body)
			_ = response.Body.Close()
			if err != nil || response.StatusCode != test.status || !response.Close {
				t.Fatal("closing ingress response advertised as reusable")
			}
			<-done
		})
	}
}

func TestIngressUnauthorizedResponseClosesConnection(t *testing.T) {
	provider, client := net.Pipe()
	defer client.Close()
	_ = client.SetDeadline(time.Now().Add(time.Second))
	done := make(chan struct{})
	go func() { (&ingressServer{}).handleConn(context.Background(), provider); close(done) }()
	_, err := io.WriteString(client, "GET /open-compute/profile HTTP/1.1\r\nHost: sidecar\r\n\r\n")
	if err != nil {
		t.Fatal(err)
	}
	response, err := http.ReadResponse(bufio.NewReader(client), nil)
	if err != nil || response.StatusCode != 401 || !response.Close {
		t.Fatal("unauthorized ingress response advertised as reusable")
	}
	_ = response.Body.Close()
	<-done
}
