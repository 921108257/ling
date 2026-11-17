package linger

import (
	"context"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"net"
	"os"
	"strings"
	"testing"
	"time"
)

// 会话记录来自语言无关夹具；Session records come from the language-independent fixture.
func sessionRows(t *testing.T, name string) [][]string {
	t.Helper()
	data, err := os.ReadFile("../../tests/fixtures/protocol/" + name)
	if err != nil {
		t.Fatal(err)
	}
	var rows [][]string
	for _, line := range strings.Split(string(data), "\n") {
		line = strings.TrimSpace(line)
		if line != "" && !strings.HasPrefix(line, "#") {
			rows = append(rows, strings.Split(line, "|"))
		}
	}
	return rows
}

func sessionRequire(ok bool, message string) {
	if !ok {
		panic(message)
	}
}

func sessionRead(conn net.Conn) frame {
	f, err := readFrame(conn)
	if err != nil {
		panic(err)
	}
	return f
}

func sessionWrite(conn net.Conn, kind byte, flags uint16, id uint64, payload []byte) {
	peer := Client{conn: conn}
	if err := peer.write(kind, flags, id, payload); err != nil {
		panic(err)
	}
}

func sessionValue(v Value) []byte {
	b, err := EncodeValue(v)
	if err != nil {
		panic(err)
	}
	return b
}

// EOF 和连接重置均表示已关闭；超时不能算关闭。 EOF and reset mean closure; timeout never does.
func sessionClosed(conn net.Conn) {
	for i := 0; i < 4; i++ {
		frame, err := readFrame(conn)
		if err == nil && frame.kind == 5 {
			continue
		}
		sessionRequire(err != nil, "unexpected repeated request")
		var timeout net.Error
		sessionRequire(!errors.As(err, &timeout) || !timeout.Timeout(), "peer did not close")
		return
	}
	panic("peer did not close")
}

func sessionInitialize(conn net.Conn, db uint64) {
	hello := sessionRead(conn)
	helloPayload := requestPayload("HELLO", []Value{VUInt(1)})
	sessionRequire(hello.id == 1 && hello.kind == 1 && string(hello.payload) == string(helloPayload.data), "HELLO order")
	identity := make([]byte, 16)
	identity[15] = 1
	value := Value{Map, map[string]Value{"protocol": VUInt(1), "server_version": VString("fixture"), "events": {Bool, true}, "reliable_events": {Bool, false}, "stream_id": {Bytes, identity}}}
	sessionWrite(conn, 2, 0, 1, append([]byte{0}, sessionValue(value)...))
	selected := sessionRead(conn)
	want := requestPayload("SELECT", []Value{VUInt(db)})
	sessionRequire(selected.id == 2 && string(selected.payload) == string(want.data), "fixed database")
	sessionWrite(conn, 2, 0, 2, []byte{0, byte(Bool), 1})
}

func sessionSuccess(mode string) (uint16, []byte) {
	if mode == "subscription" {
		return 0, []byte{0, byte(Bool), 1}
	}
	payload := append([]byte{0}, sessionValue(VUInt(1))...)
	if mode == "batch" {
		return 1, append([]byte{0, 0, 0, 1}, payload...)
	}
	return 0, payload
}

// 受控对端验证仅一条业务请求，错误后不接受重放。
// The controlled peer permits one business request and rejects any replay after failure.
func sessionPeer(conn net.Conn, mode, action string, event, heartbeat []byte) int {
	idle, subscribed := mode == "idle-subscription", strings.HasSuffix(mode, "subscription")
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(5 * time.Second))
	sessionInitialize(conn, 0)
	prepared := sessionRead(conn)
	sessionRequire(prepared.id == 3, "preparation ID")
	preparedName, preparedArgs := "PING", []Value{}
	if subscribed {
		preparedName, preparedArgs = "SUBSCRIBE", []Value{{List, []Value{VString("*")}}, VString("GROUP"), VString("sessions")}
	}
	preparedPayload := requestPayload(preparedName, preparedArgs)
	sessionRequire(prepared.kind == 1 && prepared.flags == 0 && string(prepared.payload) == string(preparedPayload.data), "preparation command")
	value := VString("PONG")
	if subscribed {
		value = Value{Bool, true}
	}
	sessionWrite(conn, 2, 0, 3, append([]byte{0}, sessionValue(value)...))
	if action == "closed" {
		sessionClosed(conn)
		return 0
	}
	count := 0
	if !idle {
		target := sessionRead(conn)
		sessionRequire(target.id == 4, "target ID")
		sessionRequire((mode == "subscription" && target.kind == 4) || (mode != "subscription" && target.kind == 1), "target kind")
		count = 1
	}
	flags, payload := sessionSuccess(mode)
	switch action {
	case "disconnect":
		return 1
	case "truncated":
		// 写完整头及一个字节后关闭，保留发送后未知。
		// Close after the complete header and one byte, preserving unknown outcome.
		fragment := make([]byte, 19)
		copy(fragment, []byte{'L', 'G', 1, 2})
		binary.BigEndian.PutUint16(fragment[4:6], flags)
		binary.BigEndian.PutUint64(fragment[6:14], 4)
		binary.BigEndian.PutUint32(fragment[14:18], uint32(len(payload)))
		fragment[18] = payload[0]
		_, err := conn.Write(fragment)
		if err != nil {
			panic(err)
		}
		return 1
	case "old-response":
		sessionWrite(conn, 2, 0, 3, []byte{0, byte(Bool), 1})
	case "old-batch":
		sessionWrite(conn, 2, 1, 3, []byte{0, 0, 0, 1, 0, byte(Bool), 1})
	case "heartbeat":
		if _, err := conn.Write(heartbeat); err != nil {
			panic(err)
		}
	case "event":
		if _, err := conn.Write(event); err != nil {
			panic(err)
		}
		if mode != "subscription" {
			sessionClosed(conn)
			return 1
		}
		if _, err := conn.Write(heartbeat); err != nil {
			panic(err)
		}
	case "zero-response", "future-response":
		id := uint64(0)
		if action == "future-response" {
			id = 5
		}
		sessionWrite(conn, 2, 0, id, append([]byte{0}, sessionValue(VUInt(1))...))
		sessionClosed(conn)
		return count
	case "zero-error", "future-error":
		id := uint64(0)
		if action == "future-error" {
			id = 5
		}
		sessionWrite(conn, 6, 0, id, append(sessionValue(VString("ERR_PROTOCOL")), sessionValue(VString(""))...))
		sessionClosed(conn)
		return count
	default:
		panic("unknown peer action")
	}
	if idle {
		if _, err := conn.Write(event); err != nil {
			panic(err)
		}
	} else {
		sessionWrite(conn, 2, flags, 4, payload)
	}
	sessionClosed(conn)
	return count
}

type sessionPeerResult struct {
	count int
	err   any
}

func sessionStart(listener net.Listener, peer func(net.Conn) int) <-chan sessionPeerResult {
	done := make(chan sessionPeerResult, 1)
	go func() {
		result := sessionPeerResult{}
		defer func() { result.err = recover(); done <- result; listener.Close() }()
		conn, err := listener.Accept()
		if err != nil {
			panic(err)
		}
		result.count = peer(conn)
	}()
	return done
}

func sessionVerify(t *testing.T, err error, expected string, unknown bool) {
	t.Helper()
	if strings.HasPrefix(expected, "ok") {
		if err != nil {
			t.Fatal(err)
		}
		return
	}
	if expected == "closed" && errors.Is(err, ErrClosed) && !unknown {
		return
	}
	var failure *Error
	if !errors.As(err, &failure) || failure.OutcomeUnknown() != unknown {
		t.Fatalf("%s: %v", expected, err)
	}
	want := map[string]string{"protocol": "ERR_CLIENT_PROTOCOL", "disconnected": "ERR_CLIENT_DISCONNECTED", "closed": "ERR_CLIENT_CLOSED", "connect": "ERR_CLIENT_CONNECT", "ERR_PROTOCOL": "ERR_PROTOCOL"}[expected]
	if expected == "incomplete" {
		if failure.Code != "ERR_CLIENT_PROTOCOL" && failure.Code != "ERR_CLIENT_DISCONNECTED" {
			t.Fatal(err)
		}
		return
	}
	if expected == "connect" && failure.Code == "ERR_CLIENT_TIMEOUT" {
		return
	}
	if failure.Code != want {
		t.Fatalf("wanted %s: %v", want, err)
	}
}

// 无真实服务环境变量也实际执行受控 TCP 会话。
// Controlled TCP sessions run without a live-service environment variable.
func TestSessions(t *testing.T) {
	var event, heartbeat []byte
	for _, row := range sessionRows(t, "frames.tsv") {
		data, err := hex.DecodeString(strings.ReplaceAll(row[1], " ", ""))
		if err != nil {
			t.Fatal(err)
		}
		if row[0] == "event" {
			event = data
		}
		if row[0] == "heartbeat" {
			heartbeat = data
		}
	}
	for _, row := range sessionRows(t, "sessions.tsv") {
		t.Run(row[0], func(t *testing.T) {
			defer func() {
				status := "passed"
				if t.Failed() {
					status = "failed"
				}
				reportContract("sessions", row[0], status)
			}()
			listener, err := net.Listen("tcp", "127.0.0.1:0")
			if err != nil {
				t.Fatal(err)
			}
			defer listener.Close()
			opts := Options{Address: listener.Addr().String(), ConnectTimeout: 2 * time.Second, RequestTimeout: 2 * time.Second}
			if row[2] == "refused" {
				listener.Close()
				_, err = Connect(context.Background(), opts)
				sessionVerify(t, err, row[3], false)
				return
			}
			if row[1] == "pool" {
				sessionPool(t, listener, opts, row[2], row[6])
				return
			}
			done := sessionStart(listener, func(conn net.Conn) int {
				count := sessionPeer(conn, row[1], row[2], event, heartbeat)
				if row[2] == "disconnect" || row[2] == "truncated" {
					// 保留监听器观察隐式重连；Keep listening to detect implicit reconnects.
					_ = listener.(*net.TCPListener).SetDeadline(time.Now().Add(150 * time.Millisecond))
					repeated, err := listener.Accept()
					if err == nil {
						repeated.Close()
						panic("implicit reconnect after write")
					}
					var timeout net.Error
					sessionRequire(errors.As(err, &timeout) && timeout.Timeout(), "reconnect observation")
				}
				return count
			})
			client, err := Connect(context.Background(), opts)
			if err != nil {
				t.Fatal(err)
			}
			defer client.Close()
			if strings.HasSuffix(row[1], "subscription") {
				s, e := client.Subscribe(context.Background(), []string{"*"}, "sessions")
				if e != nil {
					t.Fatal(e)
				}
				f, e := readFrame(strings.NewReader(string(event)))
				if e != nil {
					t.Fatal(e)
				}
				v, e := DecodeValue(f.payload)
				if e != nil {
					t.Fatal(e)
				}
				if row[1] == "idle-subscription" {
					got, e := s.Next(context.Background())
					err = e
					if e == nil && !got.Equal(v) {
						t.Fatal("idle event differs")
					}
				} else {
					err = s.Ack(context.Background(), v.Data.(map[string]Value)["event_id"])
				}
				if row[3] == "ok-event" && row[1] != "idle-subscription" {
					got, e := s.Next(context.Background())
					if e != nil || !got.Equal(v) {
						t.Fatalf("queued event: %v", e)
					}
				}
			} else {
				if _, err = client.Execute(context.Background(), "PING"); err != nil {
					t.Fatal(err)
				}
				if row[2] == "closed" {
					client.Close()
				}
				if row[1] == "batch" {
					items, e := client.Batch(context.Background(), []Request{{"INCR", []Value{VString("counter")}}})
					err = e
					if e == nil && (len(items) != 1 || items[0].Err != nil || !items[0].Value.Equal(VUInt(1))) {
						t.Fatal("batch value")
					}
				} else {
					v, e := client.Execute(context.Background(), "INCR", VString("counter"))
					err = e
					if e == nil && !v.Equal(VUInt(1)) {
						t.Fatal("value")
					}
				}
			}
			sessionVerify(t, err, row[3], row[4] == "true")
			closed := false
			select {
			case <-client.closed:
				closed = true
			default:
			}
			if closed != (row[5] == "true") {
				t.Fatal("closed state")
			}
			client.Close()
			result := <-done
			if result.err != nil || fmt.Sprint(result.count) != row[6] {
				t.Fatalf("peer: %+v", result)
			}
		})
	}
}

func sessionPool(t *testing.T, listener net.Listener, opts Options, action, want string) {
	t.Helper()
	opts.Database = 7
	done := make(chan sessionPeerResult, 1)
	go func() {
		result := sessionPeerResult{}
		defer func() { result.err = recover(); done <- result }()
		attempts := 1
		if action == "pool-reset" {
			attempts = 2
		}
		for i := 0; i < attempts; i++ {
			conn, err := listener.Accept()
			if err != nil {
				panic(err)
			}
			_ = conn.SetDeadline(time.Now().Add(5 * time.Second))
			sessionInitialize(conn, 7)
			target := sessionRead(conn)
			sessionRequire(target.id == 3, "pool target ID")
			result.count++
			if action == "pool-reset" && i == 0 {
				conn.Close()
				continue
			}
			sessionWrite(conn, 2, 0, 3, append([]byte{0}, sessionValue(VUInt(1))...))
			sessionClosed(conn)
			conn.Close()
		}
	}()
	pool, err := NewPool(opts, 1)
	if err != nil {
		t.Fatal(err)
	}
	defer pool.Close()
	if action == "pool-reset" {
		_, err := pool.Execute(context.Background(), "INCR", VString("counter"))
		sessionVerify(t, err, "disconnected", true)
	} else {
		lease, err := pool.Acquire(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		_, err = lease.Execute(context.Background(), "SELECT", VUInt(0))
		var failure *Error
		if !errors.As(err, &failure) || failure.OutcomeUnknown() {
			t.Fatalf("pool SELECT: %v", err)
		}
		lease.Close()
	}
	v, err := pool.Execute(context.Background(), "INCR", VString("counter"))
	if err != nil || !v.Equal(VUInt(1)) {
		t.Fatalf("pool recovery: %v", err)
	}
	pool.Close()
	result := <-done
	if result.err != nil || fmt.Sprint(result.count) != want {
		t.Fatalf("pool peer: %+v", result)
	}
}
