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

type authIdentityStatus uint8

const (
	authIdentityPending authIdentityStatus = iota
	authIdentityValidated
	authIdentityAccepted
	authIdentityRejected
	authIdentityStale
)

func (s authIdentityStatus) label() string {
	switch s {
	case authIdentityValidated:
		return "validated"
	case authIdentityAccepted:
		return "accepted"
	case authIdentityRejected:
		return "rejected"
	case authIdentityStale:
		return "stale"
	default:
		return "pending"
	}
}

type authIdentityMethod uint8

const (
	authIdentityUnavailable authIdentityMethod = iota
	authIdentityUserPassword
	authIdentityToken
	authIdentityAnonymous
	authIdentityNoAuthUser
	authIdentityNKey
	authIdentityJWT
	authIdentityCustom
	authIdentityDelegated
	authIdentityTLSMapping
)

func (m authIdentityMethod) label() string {
	switch m {
	case authIdentityUserPassword:
		return "user_password"
	case authIdentityToken:
		return "token"
	case authIdentityAnonymous:
		return "anonymous"
	case authIdentityNoAuthUser:
		return "no_auth_user"
	case authIdentityNKey:
		return "nkey"
	case authIdentityJWT:
		return "jwt"
	case authIdentityCustom:
		return "custom"
	case authIdentityDelegated:
		return "delegated"
	case authIdentityTLSMapping:
		return "tls_mapping"
	default:
		return "unavailable"
	}
}

type authIdentitySource uint8

const (
	authIdentityConnect authIdentitySource = iota
	authIdentityCookie
)

func (s authIdentitySource) label() string {
	if s == authIdentityCookie {
		return "cookie"
	}
	return "connect"
}

// The zero state is pending/CONNECT with an unavailable mechanism. Fixed
// labels are expanded only for monitoring. Safe immutable name cells belong
// to configuration, not authentication attempts or secret-bearing User objects.
type authIdentityState struct {
	generation, sequence uint64
	publicUser           *string
	state                authIdentityStatus
	mechanism            authIdentityMethod
	source               authIdentitySource
	// Sticky across CONNECT and revalidation: injected credentials are not proof.
	assigned bool
}

type authIdentityPolicy struct {
	opts       *Options // identity comparison only; no copy of secret-bearing options
	generation uint64
	names      map[string]*string // immutable cells containing safe public names only
}

// Work is bounded at configuration time, never on the message/authentication
// path. If collision safety is too expensive or unsupported, emit no names.
func publicAuthNames(opts *Options) map[string]*string {
	names := make(map[string]*string)
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
			publicName := name
			names[name] = &publicName
		}
	}
	return names
}

func (s *Server) newAuthIdentityPolicy(opts *Options) *authIdentityPolicy {
	return &authIdentityPolicy{opts: opts, generation: atomic.AddUint64(&s.authIdentitySerial, 1), names: publicAuthNames(opts)}
}

// Called only on authentication/revalidation. c.mu also protects monitoring;
// no message delivery path reads or writes these fields.
func (c *client) finishAuthIdentity(p *authIdentityPolicy, attempt uint64, accepted bool, mechanism authIdentityMethod, canonical string) {
	if c.kind != CLIENT {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.authIdentity.sequence != attempt {
		return
	}
	seq := attempt + 1
	state := authIdentityRejected
	if accepted {
		state = authIdentityAccepted
		if c.authIdentity.state == authIdentityPending {
			state = authIdentityValidated
		}
	}
	var name *string
	if accepted && mechanism == authIdentityUserPassword && p != nil && c.srv.authIdentityPolicy.Load() == p {
		name = p.names[canonical]
	}
	generation := uint64(0)
	if p != nil {
		generation = p.generation
	}
	c.authIdentity = authIdentityState{
		generation: generation,
		sequence:   seq,
		publicUser: name,
		state:      state,
		mechanism:  mechanism,
		source:     c.authIdentity.source,
		assigned:   c.authIdentity.assigned,
	}
}

// c.mu is held by the existing authorized monitoring walk.
func (c *client) authIdentityLocked() AuthIdentity {
	if c.kind != CLIENT || c.srv == nil {
		return AuthIdentity{}
	}
	a := c.authIdentity
	p := c.srv.authIdentityPolicy.Load()
	if a.state == authIdentityAccepted && (p == nil || a.generation != p.generation) {
		a.state = authIdentityStale
		a.publicUser = nil
	}
	name := ""
	if a.publicUser != nil {
		name = *a.publicUser
	}
	return AuthIdentity{2, a.generation, a.sequence, a.state.label(), a.mechanism.label(), a.source.label(), name}
}
