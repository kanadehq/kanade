package server

import (
	"golang.org/x/crypto/bcrypt"
	"net/url"
	"reflect"
	"strings"
	"sync/atomic"
	"unicode"
)

// AuthIdentity is additive monitoring v2. Broker/start binding is inherited
// from the Connz envelope; cid/start/transport are the enclosing ConnInfo.
// All fields of that connection are copied under the same existing c.mu.
type AuthIdentity struct {
	Version    int    `json:"version"`
	Generation uint64 `json:"generation"`
	Sequence   uint64 `json:"sequence"`
	State      string `json:"state"`
	Mechanism  string `json:"mechanism"`
	Source     string `json:"source"`
	PublicUser string `json:"public_user,omitempty"`
}

type authIdentityState struct {
	generation, sequence                 uint64
	state, mechanism, source, publicUser string
}

type authIdentityPolicy struct {
	opts       *Options // identity comparison only; no copy of secret-bearing options
	generation uint64
	names      map[string]bool // immutable, safe public identities only
}

// Work is bounded at configuration time, never on the message/authentication
// path. If collision safety is too expensive or unsupported, emit no names.
func publicAuthNames(opts *Options) map[string]bool {
	names := make(map[string]bool)
	if opts.CustomClientAuthentication != nil || opts.AuthCallout != nil || len(opts.TrustedKeys) != 0 || len(opts.Users) > 64 {
		return names
	}
	candidates := []string{opts.Username, opts.Websocket.Username, opts.MQTT.Username}
	for _, u := range opts.Users {
		candidates = append(candidates, u.Username)
	}
	// Only this configuration call holds secret references. Deduplicate before
	// bcrypt work; no secrets or hash results survive in the policy.
	secrets := make(map[string]struct{})
	addSecret := func(secret string) {
		if secret != "" {
			secrets[secret] = struct{}{}
		}
	}
	visited := make(map[uintptr]bool)
	var walk func(reflect.Value)
	walk = func(v reflect.Value) {
		if !v.IsValid() {
			return
		}
		if v.Kind() == reflect.Interface {
			if !v.IsNil() {
				walk(v.Elem())
			}
			return
		}
		if v.Kind() == reflect.Pointer {
			if v.IsNil() || visited[v.Pointer()] {
				return
			}
			visited[v.Pointer()] = true
			if u, ok := v.Interface().(*url.URL); ok && u.User != nil {
				p, _ := u.User.Password()
				addSecret(p)
				return
			}
			walk(v.Elem())
			return
		}
		switch v.Kind() {
		case reflect.Struct:
			for i := 0; i < v.NumField(); i++ {
				f := v.Type().Field(i)
				if !f.IsExported() {
					continue
				}
				x := v.Field(i)
				n := strings.ToLower(f.Name)
				if x.Kind() == reflect.String && (strings.Contains(n, "password") || strings.Contains(n, "token") || strings.Contains(n, "secret") || strings.Contains(n, "seed") || strings.Contains(n, "nkey") || (strings.HasPrefix(n, "jetstream") && strings.HasSuffix(n, "key")) || n == "authorization") {
					addSecret(x.String())
				} else {
					walk(x)
				}
			}
		case reflect.Slice, reflect.Array:
			if v.Type().Elem().Kind() != reflect.Uint8 {
				for i := 0; i < v.Len(); i++ {
					walk(v.Index(i))
				}
			}
		case reflect.Map:
			it := v.MapRange()
			for it.Next() {
				walk(it.Value())
			}
		}
	}
	walk(reflect.ValueOf(opts))
	if len(secrets) > 256 {
		return names
	}
	unique := make(map[string]bool)
	for _, name := range candidates {
		if name == "" || len(name) > 64 {
			continue
		}
		valid := true
		for _, r := range name {
			if r > unicode.MaxASCII || !(unicode.IsLetter(r) || unicode.IsDigit(r) || strings.ContainsRune("._-", r)) {
				valid = false
			}
		}
		// Reserved consumer labels must never be public usernames.
		if strings.Contains("|unknown|shared-token|no-auth|auth-pending|auth-unproven|auth-rejected|auth-accepted-unnameable|", "|"+name+"|") {
			valid = false
		}
		if valid {
			unique[name] = true
		}
	}
	// Check literal collisions first and parse each distinct hash once.
	// A plaintext equality needs no expensive hash comparison.
	for name := range unique {
		if _, collides := secrets[name]; collides {
			delete(unique, name)
		}
	}
	hashes := [][]byte{}
	work := 0
	for secret := range secrets {
		if !strings.HasPrefix(secret, "$") {
			continue
		}
		hash := []byte(secret)
		if cost, err := bcrypt.Cost(hash); err == nil {
			if cost > 12 {
				return names
			}
			work += (1 << cost) * len(unique)
			hashes = append(hashes, hash)
		}
	}
	if work > 4096 {
		return names
	}
	for name := range unique {
		safe := true
		candidate := []byte(name)
		for _, hash := range hashes {
			if bcrypt.CompareHashAndPassword(hash, candidate) == nil {
				safe = false
				break
			}
		}
		if safe {
			names[name] = true
		}
	}
	return names
}

func (s *Server) newAuthIdentityPolicy(opts *Options) *authIdentityPolicy {
	return &authIdentityPolicy{opts: opts, generation: atomic.AddUint64(&s.authIdentitySerial, 1), names: publicAuthNames(opts)}
}

// Called only on authentication/revalidation. c.mu also protects monitoring;
// no message delivery path reads or writes these fields.
func (c *client) finishAuthIdentity(p *authIdentityPolicy, attempt uint64, accepted bool, mechanism, canonical string) {
	if c.kind != CLIENT {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.authIdentity.sequence != attempt {
		return
	}
	seq := attempt + 1
	source := c.authIdentity.source
	if source == "" {
		source = "connect"
	}
	state := "rejected"
	if accepted {
		state = "accepted"
		if c.authIdentity.state == "pending" {
			state = "validated"
		}
	}
	name := ""
	if accepted && mechanism == "user_password" && p != nil && c.srv.authIdentityPolicy.Load() == p && p.names[canonical] {
		name = canonical
	}
	generation := uint64(0)
	if p != nil {
		generation = p.generation
	}
	c.authIdentity = authIdentityState{generation, seq, state, mechanism, source, name}
}

// c.mu is held by the existing authorized monitoring walk.
func (c *client) authIdentityLocked() AuthIdentity {
	if c.kind != CLIENT || c.srv == nil {
		return AuthIdentity{}
	}
	a := c.authIdentity
	if a.state == "" {
		a.state = "pending"
	}
	if a.mechanism == "" {
		a.mechanism = "unavailable"
	}
	if a.source == "" {
		a.source = "connect"
	}
	p := c.srv.authIdentityPolicy.Load()
	if a.state == "accepted" && (p == nil || a.generation != p.generation) {
		a.state = "stale"
		a.publicUser = ""
	}
	return AuthIdentity{2, a.generation, a.sequence, a.state, a.mechanism, a.source, a.publicUser}
}
