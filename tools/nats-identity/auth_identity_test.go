package server

import (
	"bufio"
	"encoding/json"
	"fmt"
	"github.com/nats-io/jwt/v2"
	"github.com/nats-io/nats.go"
	"github.com/nats-io/nkeys"
	"golang.org/x/crypto/bcrypt"
	"net"
	"strings"
	"sync"
	"testing"
	"time"
)

func proofFor(t *testing.T, s *Server, id uint64) *AuthIdentity {
	t.Helper()
	z, err := s.Connz(&ConnzOptions{Username: true})
	if err != nil {
		t.Fatal("monitor failed")
	}
	for _, c := range z.Conns {
		if c.Cid == id {
			return c.AuthIdentity
		}
	}
	t.Fatal("connection missing")
	return nil
}

func TestAuthIdentityCanonicalAndPending(t *testing.T) {
	s := RunServer(&Options{NoLog: true, Host: "127.0.0.1", Port: -1, Users: []*User{{Username: "agent", Password: "fixture-password"}, {Username: "backend", Password: "other-password"}}})
	defer s.Shutdown()
	socket, err := net.Dial("tcp", s.Addr().String())
	if err != nil {
		t.Fatal("connect failed")
	}
	defer socket.Close()
	r := bufio.NewReader(socket)
	line, _ := r.ReadString('\n')
	var info struct {
		ID uint64 `json:"client_id"`
	}
	json.Unmarshal([]byte(strings.TrimSpace(strings.TrimPrefix(line, "INFO "))), &info)
	if a := proofFor(t, s, info.ID); a.State != "pending" || a.PublicUser != "" {
		t.Fatal("pre-CONNECT acquired identity")
	}
	fmt.Fprint(socket, "CONNECT {\"verbose\":false,\"user\":\"agent\",\"pass\":\"fixture-password\",\"nkey\":\"backend\"}\r\nPING\r\n")
	for {
		l, e := r.ReadString('\n')
		if e != nil {
			t.Fatal("handshake failed")
		}
		if l == "PONG\r\n" {
			break
		}
	}
	a := proofFor(t, s, info.ID)
	if a.State != "accepted" || a.PublicUser != "agent" || a.Mechanism != "user_password" || a.Generation == 0 {
		t.Fatal("canonical result missing")
	}
	z, _ := s.Connz(&ConnzOptions{Username: false})
	for _, c := range z.Conns {
		if c.AuthIdentity != nil {
			t.Fatal("proof exposed without auth=1")
		}
	}
	// Monitoring and reauthentication share c.mu. No stale canonical name may
	// escape while a failed second CONNECT replaces the mutable options.
	hash, _ := bcrypt.GenerateFromPassword([]byte("fixture-password"), 8)
	opts := s.getOpts().Clone()
	opts.Users[0].Password = string(hash)
	s.setOpts(opts)
	if proofFor(t, s, info.ID).State != "stale" || proofFor(t, s, info.ID).PublicUser != "" {
		t.Fatal("generation did not revoke proof")
	}
	fmt.Fprint(socket, "CONNECT {\"user\":\"agent\",\"pass\":\"wrong-canary\",\"nkey\":\"secret-canary\"}\r\n")
	for i := 0; i < 20; i++ {
		z, _ := s.Connz(&ConnzOptions{Username: true})
		for _, c := range z.Conns {
			if c.Cid == info.ID && c.AuthIdentity.PublicUser != "" {
				t.Fatal("reauth exposed old identity")
			}
		}
	}
}

func TestAuthIdentitySecretCollisions(t *testing.T) {
	h, _ := bcrypt.GenerateFromPassword([]byte("agent"), 4)
	for _, password := range []string{"agent", string(h)} {
		opts := &Options{Users: []*User{{Username: "agent", Password: "fixture"}, {Username: "other", Password: password}}}
		if publicAuthNames(opts)["agent"] {
			t.Fatal("credential collision exposed")
		}
	}
	opts := &Options{Users: []*User{{Username: "agent", Password: "fixture"}}}
	opts.Websocket.Token = "agent"
	if publicAuthNames(opts)["agent"] {
		t.Fatal("listener token collision exposed")
	}
	h, _ = bcrypt.GenerateFromPassword([]byte("fixture"), 12)
	opts.Websocket.Token = ""
	opts.Username = "second-role"
	opts.Users[0].Password = string(h)
	if publicAuthNames(opts)["agent"] {
		t.Fatal("proof work budget not enforced")
	}
}

func TestAuthIdentityNKeyJWTAndConcurrentMonitoring(t *testing.T) {
	nk, _ := nkeys.CreateUser()
	pub, _ := nk.PublicKey()
	s := RunServer(&Options{NoLog: true, Host: "127.0.0.1", Port: -1, Nkeys: []*NkeyUser{{Nkey: pub}}})
	defer s.Shutdown()
	nc, err := nats.Connect(s.ClientURL(), nats.Nkey(pub, nk.Sign))
	if err != nil {
		t.Fatal("nkey handshake failed")
	}
	defer nc.Close()
	id, _ := nc.GetClientID()
	a := proofFor(t, s, id)
	if a.Mechanism != "nkey" || a.PublicUser != "" || a.State != "accepted" {
		t.Fatal("nkey classified as user")
	}
	op, _ := nkeys.CreateOperator()
	opPub, _ := op.PublicKey()
	acc, _ := nkeys.CreateAccount()
	accPub, _ := acc.PublicKey()
	ac := jwt.NewAccountClaims(accPub)
	accountJWT, _ := ac.Encode(op)
	uk, _ := nkeys.CreateUser()
	up, _ := uk.PublicKey()
	uc := jwt.NewUserClaims(up)
	userJWT, _ := uc.Encode(acc)
	resolver := &MemAccResolver{}
	resolver.Store(accPub, accountJWT)
	js := RunServer(&Options{NoLog: true, Host: "127.0.0.1", Port: -1, TrustedKeys: []string{opPub}, AccountResolver: resolver})
	defer js.Shutdown()
	jc, err := nats.Connect(js.ClientURL(), nats.UserJWT(func() (string, error) { return userJWT, nil }, uk.Sign))
	if err != nil {
		t.Fatal("JWT handshake failed")
	}
	defer jc.Close()
	jid, _ := jc.GetClientID()
	a = proofFor(t, js, jid)
	if a.State != "accepted" || a.PublicUser != "" || a.Mechanism != "jwt" {
		t.Fatal("JWT classified as user")
	}
	out, _ := json.Marshal(a)
	for _, secret := range []string{userJWT, pub, up, "127.0.0.1"} {
		if strings.Contains(string(out), secret) {
			t.Fatal("non-public data in identity metadata")
		}
	}
	var wg sync.WaitGroup
	for i := 0; i < 4; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for j := 0; j < 50; j++ {
				s.Connz(&ConnzOptions{Username: true})
				js.Varz(nil)
			}
		}()
	}
	for i := 0; i < 20; i++ {
		c, e := nats.Connect(s.ClientURL(), nats.Nkey(pub, nk.Sign))
		if e != nil {
			t.Error("churn failed")
			break
		}
		c.Close()
	}
	wg.Wait()
}

func TestAuthIdentityReloadConcurrent(t *testing.T) {
	s := RunServer(&Options{NoLog: true, Host: "127.0.0.1", Port: -1, Users: []*User{{Username: "agent", Password: "fixture"}}})
	defer s.Shutdown()
	var wg sync.WaitGroup
	for i := 0; i < 3; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for j := 0; j < 50; j++ {
				s.Connz(&ConnzOptions{Username: true})
				s.Varz(nil)
				time.Sleep(time.Millisecond)
			}
		}()
	}
	for i := 0; i < 20; i++ {
		opts := s.getOpts().Clone()
		s.setOpts(opts)
		nc, e := nats.Connect(s.ClientURL(), nats.UserInfo("agent", "fixture"))
		if e == nil {
			nc.Close()
		}
	}
	wg.Wait()
}
