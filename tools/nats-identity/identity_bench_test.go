package server

import (
	"encoding/json"
	"github.com/nats-io/nats.go"
	"golang.org/x/crypto/bcrypt"
	"sync"
	"testing"
	"time"
	"unsafe"
)

func benchmarkIdentityServer(b *testing.B) (*Server, func()) {
	b.Helper()
	s := RunServer(&Options{NoLog: true, Host: "127.0.0.1", Port: -1, Users: []*User{{Username: "agent", Password: "fixture-password"}}})
	return s, s.Shutdown
}
func BenchmarkIdentityChurn(b *testing.B) {
	s, close := benchmarkIdentityServer(b)
	defer close()
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		n, e := nats.Connect(s.ClientURL(), nats.UserInfo("agent", "fixture-password"))
		if e != nil {
			b.Fatal("handshake failed")
		}
		n.Flush()
		n.Close()
	}
}
func BenchmarkIdentityConnz(b *testing.B) {
	s, close := benchmarkIdentityServer(b)
	defer close()
	clients := []*nats.Conn{}
	for i := 0; i < 128; i++ {
		n, e := nats.Connect(s.ClientURL(), nats.UserInfo("agent", "fixture-password"))
		if e != nil {
			b.Fatal("connect failed")
		}
		clients = append(clients, n)
	}
	defer func() {
		for _, n := range clients {
			n.Close()
		}
	}()
	b.ReportAllocs()
	b.ReportMetric(float64(unsafe.Sizeof(client{})), "client_bytes")
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		z, _ := s.Connz(&ConnzOptions{Username: true})
		body, e := json.Marshal(z)
		if e != nil {
			b.Fatal("marshal failed")
		}
		b.ReportMetric(float64(len(body)), "response_bytes")
	}
}
func BenchmarkIdentityMessagingMonitored(b *testing.B) {
	s, close := benchmarkIdentityServer(b)
	defer close()
	nc, e := nats.Connect(s.ClientURL(), nats.UserInfo("agent", "fixture-password"))
	if e != nil {
		b.Fatal("connect failed")
	}
	defer nc.Close()
	nc.Subscribe("bench.request", func(m *nats.Msg) { m.Respond(m.Data) })
	nc.Flush()
	stop := make(chan struct{})
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		tick := time.NewTicker(10 * time.Millisecond)
		defer tick.Stop()
		for {
			select {
			case <-stop:
				return
			case <-tick.C:
				z, _ := s.Connz(&ConnzOptions{Username: true})
				json.Marshal(z)
			}
		}
	}()
	defer func() { closeChannel(stop); wg.Wait() }()
	payload := make([]byte, 64)
	b.SetBytes(64)
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		if _, err := nc.Request("bench.request", payload, time.Second); err != nil {
			b.Fatal("request failed")
		}
	}
}
func closeChannel(c chan struct{}) { close(c) }
func BenchmarkIdentityConfigBcrypt(b *testing.B) {
	h, _ := bcrypt.GenerateFromPassword([]byte("fixture-password"), 8)
	opts := &Options{NoLog: true, Host: "127.0.0.1", Port: -1, Users: []*User{{Username: "agent", Password: string(h)}, {Username: "backend", Password: string(h)}, {Username: "breakglass", Password: string(h)}}}
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		s, e := NewServer(opts)
		if e != nil {
			b.Fatal("configuration failed")
		}
		s.Shutdown()
	}
}
