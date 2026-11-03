package server

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"github.com/nats-io/nats.go"
	"golang.org/x/crypto/bcrypt"
	"math/big"
	"net"
	"reflect"
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
		if err := n.Flush(); err != nil {
			b.Fatal("flush failed")
		}
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
	b.ResetTimer()
	// ResetTimer clears custom metrics: report retained-size data after it.
	b.ReportMetric(float64(unsafe.Sizeof(client{})), "client_bytes")
	field, present := reflect.TypeOf(client{}).FieldByName("authIdentity")
	stateBytes := uintptr(0)
	if present {
		stateBytes = field.Type.Size()
	}
	b.ReportMetric(float64(stateBytes), "identity_state_bytes")
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

// Each NewServer call is a cold construction. Only credential generation is
// outside the timed region; no policy/hash-result cache is shared by calls.
func BenchmarkIdentityConfigBcryptDistinct(b *testing.B) {
	users := []*User{}
	for _, role := range []string{"agent", "backend", "breakglass"} {
		h, err := bcrypt.GenerateFromPassword([]byte("fixture-"+role+"-password"), 8)
		if err != nil {
			b.Fatal("credential generation failed")
		}
		users = append(users, &User{Username: role, Password: string(h)})
	}
	opts := &Options{NoLog: true, Host: "127.0.0.1", Port: -1, Users: users}
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		s, err := NewServer(opts)
		if err != nil {
			b.Fatal("configuration failed")
		}
		s.Shutdown()
	}
}

// A separate synthetic authenticated WSS workload: one cost-8 bcrypt user,
// fresh verified TLS and WebSocket handshake, CONNECT and PING/PONG per op.
// TLS terminates at this disposable broker, not at the deployment's Caddy.
func BenchmarkIdentityWSSBcryptChurn(b *testing.B) {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		b.Fatal("key generation failed")
	}
	template := &x509.Certificate{
		SerialNumber: big.NewInt(1), NotBefore: time.Now().Add(-time.Minute),
		NotAfter: time.Now().Add(time.Hour), IPAddresses: []net.IP{net.ParseIP("127.0.0.1")},
		KeyUsage: x509.KeyUsageDigitalSignature, ExtKeyUsage: []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth},
	}
	der, err := x509.CreateCertificate(rand.Reader, template, template, &key.PublicKey, key)
	if err != nil {
		b.Fatal("certificate generation failed")
	}
	cert, err := x509.ParseCertificate(der)
	if err != nil {
		b.Fatal("certificate parse failed")
	}
	roots := x509.NewCertPool()
	roots.AddCert(cert)
	hash, err := bcrypt.GenerateFromPassword([]byte("fixture-password"), 8)
	if err != nil {
		b.Fatal("credential generation failed")
	}
	s := RunServer(&Options{NoLog: true, Host: "127.0.0.1", Port: -1,
		Users: []*User{{Username: "agent", Password: string(hash)}},
		Websocket: WebsocketOpts{Host: "127.0.0.1", Port: -1,
			TLSConfig: &tls.Config{MinVersion: tls.VersionTLS12,
				Certificates: []tls.Certificate{{Certificate: [][]byte{der}, PrivateKey: key}}}},
	})
	defer s.Shutdown()
	address := "wss://" + s.websocket.listener.Addr().String()
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		c, err := nats.Connect(address, nats.UserInfo("agent", "fixture-password"),
			nats.Secure(&tls.Config{MinVersion: tls.VersionTLS12, RootCAs: roots}))
		if err != nil {
			b.Fatal("WSS authentication failed")
		}
		if err := c.Flush(); err != nil {
			b.Fatal("WSS flush failed")
		}
		c.Close()
	}
}
