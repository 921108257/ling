// 独立共享黄金和真实服务端验收，不使用自身编码器生成期望字节。
// Independent shared fixtures and real-server contracts never derive expected bytes from the encoder.
package linger

import (
	"bytes"
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"errors"
	"net"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// 所有语言读取同一份冻结用例；Every language reads the same frozen fixtures.
func fixtures(t *testing.T, name string) [][]string {
	t.Helper()
	data, e := os.ReadFile(filepath.Join("../../tests/fixtures/protocol", name))
	if e != nil {
		t.Fatal(e)
	}
	var lines [][]string
	for _, line := range strings.Split(string(data), "\n") {
		if strings.TrimSpace(line) == "" || strings.HasPrefix(line, "#") {
			continue
		}
		parts := strings.Split(line, "|")
		for i := range parts {
			parts[i] = strings.TrimSpace(parts[i])
		}
		lines = append(lines, parts)
	}
	return lines
}
func unhex(t *testing.T, s string) []byte {
	t.Helper()
	b, e := hex.DecodeString(strings.ReplaceAll(s, " ", ""))
	if e != nil {
		t.Fatal(e)
	}
	return b
}

// 黄金覆盖十类型、畸形长度和所有帧类别；Goldens cover ten tags, malformed lengths, and every frame kind.
func TestSharedProtocol(t *testing.T) {
	for _, line := range fixtures(t, "values.tsv") {
		t.Run(line[0], func(t *testing.T) {
			want := unhex(t, line[1])
			v, e := DecodeValue(want)
			if e != nil {
				t.Fatal(e)
			}
			got, e := EncodeValue(v)
			if e != nil || !bytes.Equal(got, want) {
				t.Fatalf("golden %s", line[0])
			}
		})
	}
	for _, line := range fixtures(t, "invalid-values.tsv") {
		_, e := DecodeValue(unhex(t, line[3]))
		var decode *DecodeError
		if !errors.As(e, &decode) || decode.Code != line[2] || decode.Fatal != (line[1] == "true") {
			t.Fatalf("invalid fixture %s: %v", line[0], e)
		}
	}
	for _, line := range fixtures(t, "frames.tsv") {
		f, e := readFrame(bytes.NewReader(unhex(t, line[1])))
		if e != nil {
			t.Fatal(e)
		}
		d := decoder{data: f.payload, budget: 128 * 1024 * 1024}
		switch f.kind {
		case 1:
			if f.flags == 1 {
				_, e = d.count()
			}
			if e == nil {
				_, e = d.value(0)
			}
			if e == nil {
				_, e = d.value(0)
			}
		case 2:
			if f.flags == 1 {
				_, e = d.count()
			}
			if e == nil {
				_, e = decodeResponse(&d)
			}
		case 3, 4:
			_, e = d.value(0)
		case 6:
			var server *Error
			e = decodeServerError(&d)
			if errors.As(e, &server) {
				e = nil
			}
		}
		if e != nil || len(d.data) != 0 {
			t.Fatalf("frame %s: %v", line[0], e)
		}
	}
}

// 可选服务地址由仓库验收驱动器提供；The repository harness supplies an optional live address.
func options(t *testing.T) Options {
	t.Helper()
	address := os.Getenv("LINGER_TEST_ADDR")
	if address == "" {
		t.Skip("live server provided by tests/sdk_contracts.py")
	}
	o := Options{Address: address, RequestTimeout: time.Second}
	if ca := os.Getenv("LINGER_TEST_CA"); ca != "" {
		data, e := os.ReadFile(ca)
		if e != nil {
			t.Fatal(e)
		}
		roots := x509.NewCertPool()
		if !roots.AppendCertsFromPEM(data) {
			t.Fatal("CA")
		}
		o.TLS = &tls.Config{RootCAs: roots, ServerName: "localhost", MinVersion: tls.VersionTLS12}
		if cert := os.Getenv("LINGER_TEST_CERT"); cert != "" {
			identity, e := tls.LoadX509KeyPair(cert, os.Getenv("LINGER_TEST_KEY"))
			if e != nil {
				t.Fatal(e)
			}
			o.TLS.Certificates = []tls.Certificate{identity}
		}
	}
	return o
}

// 每个共享命令直接调用真实 Rust 服务端；Every shared command calls the real Rust server.
func TestLiveContracts(t *testing.T) {
	o := options(t)
	ctx := context.Background()
	c, e := Connect(ctx, o)
	if e != nil {
		t.Fatal(e)
	}
	defer c.Close()
	for _, line := range fixtures(t, "commands.tsv") {
		args, e := DecodeValue(unhex(t, line[2]))
		if e != nil {
			t.Fatal(e)
		}
		v, e := c.Execute(ctx, line[1], args.Data.([]Value)...)
		if strings.HasPrefix(line[3], "ERR_") {
			var server *Error
			if !errors.As(e, &server) || server.Code != line[3] {
				t.Fatalf("%s: %v", line[0], e)
			}
		} else {
			want, e2 := DecodeValue(unhex(t, line[3]))
			if e != nil || e2 != nil || !v.Equal(want) {
				t.Fatalf("%s: %v", line[0], e)
			}
		}
	}
	// 三语言共用 INFO 类型契约，动态计数保持 UInt；Shared INFO types retain dynamic counters as UInt.
	info, e := c.Execute(ctx, "INFO")
	if e != nil || info.Kind != Map {
		t.Fatalf("INFO: %v", e)
	}
	for _, row := range fixtures(t, "info-fields.tsv") {
		section := info.Data.(map[string]Value)[row[0]]
		if section.Kind != Map {
			t.Fatalf("INFO section: %s", row[0])
		}
		value, present := section.Data.(map[string]Value)[row[1]]
		if !present {
			t.Fatalf("missing INFO field: %v", row)
		}
		valid := false
		switch row[2] {
		case "UInt":
			valid = value.Kind == UInt
		case "Bool":
			valid = value.Kind == Bool
		case "Float":
			valid = value.Kind == Float
		case "UIntOrNull":
			valid = value.Kind == UInt || value.Kind == Null
		case "FloatOrNull":
			valid = value.Kind == Float || value.Kind == Null
		case "Histogram":
			if value.Kind == Map {
				histogram := value.Data.(map[string]Value)
				valid = histogram["count"].Kind == UInt && histogram["sum"].Kind == UInt &&
					histogram["bounds"].Kind == List && len(histogram["bounds"].Data.([]Value)) == 6 &&
					histogram["buckets"].Kind == List && len(histogram["buckets"].Data.([]Value)) == 7
			}
		}
		if !valid {
			t.Fatalf("INFO field %v: %v", row, value.Kind)
		}
	}
	for _, line := range fixtures(t, "values.tsv") {
		v, e := DecodeValue(unhex(t, line[1]))
		if e != nil {
			t.Fatal(e)
		}
		key := VString("go:" + line[0])
		if _, e = c.Execute(ctx, "SET", key, v); e != nil {
			t.Fatal(e)
		}
		got, e := c.Execute(ctx, "GET", key)
		if e != nil || !got.Equal(v) {
			t.Fatalf("live value %s: %v", line[0], e)
		}
	}
	key := VString("go:batch")
	results, e := c.Batch(ctx, []Request{{"SET", []Value{key, {Null, nil}}}, {"GET", []Value{key}}, {"GET", []Value{VString("absent")}}})
	if e != nil || len(results) != 3 || results[2].Err == nil || results[1].Value.Kind != Null {
		t.Fatalf("batch %v", e)
	}
	subscriber, e := Connect(ctx, o)
	if e != nil {
		t.Fatal(e)
	}
	stream := subscriber.StreamID()
	// 切库后的订阅验证使用当前数据库；Subscription validation follows the selected database.
	for _, client := range []*Client{c, subscriber} {
		if _, e = client.Execute(ctx, "SELECT", VUInt(1)); e != nil {
			t.Fatal(e)
		}
	}
	s, e := subscriber.Subscribe(ctx, []string{"go:event"}, "go-workers")
	if e != nil {
		t.Fatal(e)
	}
	if _, e = c.Execute(ctx, "SET", VString("go:event"), Value{Bytes, []byte{0, 255}}); e != nil {
		t.Fatal(e)
	}
	event, e := s.Next(ctx)
	if e != nil {
		t.Fatal(e)
	}
	id := event.Data.(map[string]Value)["event_id"]
	if !id.Data.(map[string]Value)["db"].Equal(VUInt(1)) {
		t.Fatal("subscription database")
	}
	s.Close()
	retry, e := Connect(ctx, o)
	if e != nil {
		t.Fatal(e)
	}
	if e = retry.RequireEventStream(stream); e != nil {
		t.Fatal(e)
	}
	if _, e = retry.Execute(ctx, "SELECT", VUInt(1)); e != nil {
		t.Fatal(e)
	}
	s, e = retry.Subscribe(ctx, []string{"go:event"}, "go-workers")
	if e != nil {
		t.Fatal(e)
	}
	event, e = s.Next(ctx)
	if e != nil || !event.Data.(map[string]Value)["event_id"].Equal(id) {
		t.Fatalf("redelivery: %v", e)
	}
	if e = s.Ack(ctx, id); e != nil {
		t.Fatal(e)
	}
	waiting, cancel := context.WithTimeout(ctx, 20*time.Millisecond)
	defer cancel()
	if _, e = s.Next(waiting); e == nil {
		t.Fatal("cancel")
	}
	s.Close()
	pool, e := NewPool(o, 2)
	if e != nil {
		t.Fatal(e)
	}
	if _, e = pool.Execute(ctx, "GET", key); e != nil {
		t.Fatal(e)
	}
	if _, e = pool.Execute(ctx, "SELECT", VUInt(1)); e == nil {
		t.Fatal("pooled select")
	}
	lease, e := pool.Acquire(ctx)
	if e != nil {
		t.Fatal(e)
	}
	if _, e = lease.Execute(ctx, "SCAN", VUInt(0)); e != nil {
		t.Fatal(e)
	}
	pool.Close()
	if _, e = lease.Execute(ctx, "GET", key); e == nil {
		t.Fatal("closed pool lease")
	}
	lease.Close()
}

// 发送后断线与取消不能伪装成未执行；Disconnect/cancel after send must never claim nonexecution.
func TestUnknownOutcome(t *testing.T) {
	listener, e := net.Listen("tcp", "127.0.0.1:0")
	if e != nil {
		t.Fatal(e)
	}
	defer listener.Close()
	done := make(chan struct{})
	go func() {
		defer close(done)
		socket, e := listener.Accept()
		if e != nil {
			return
		}
		defer socket.Close()
		c := Client{conn: socket}
		for i := 0; i < 2; i++ {
			f, e := readFrame(socket)
			if e != nil {
				return
			}
			v := Value{Bool, true}
			if i == 0 {
				v = Value{Map, map[string]Value{"protocol": VUInt(1), "server_version": VString("test"), "events": {Bool, false}, "reliable_events": {Bool, false}, "stream_id": {Null, nil}}}
			}
			data, _ := EncodeValue(v)
			if c.write(2, 0, f.id, append([]byte{0}, data...)) != nil {
				return
			}
		}
		readFrame(socket)
	}()
	c, e := Connect(context.Background(), Options{Address: listener.Addr().String()})
	if e != nil {
		t.Fatal(e)
	}
	defer c.Close()
	_, e = c.Execute(context.Background(), "INCR", VString("counter"))
	var failure *Error
	if !errors.As(e, &failure) || !failure.OutcomeUnknown() {
		t.Fatalf("outcome: %v", e)
	}
	<-done
}

// 畸形批量响应必须关闭连接并保留结果未知，不能当作逐项业务错误。
// Malformed batch responses close the connection with unknown outcomes, not per-item business errors.
func TestMalformedBatchResponse(t *testing.T) {
	for _, response := range []struct {
		flags uint16
		data  []byte
	}{{1, []byte{0, 0, 0, 1, 2}}, {0, []byte{0, byte(Null)}}} {
		local, remote := net.Pipe()
		client := &Client{conn: local, options: Options{RequestTimeout: time.Second}, gate: make(chan struct{}, 1), incoming: make(chan frameResult, 1), closed: make(chan struct{})}
		client.gate <- struct{}{}
		go client.readLoop()
		done := make(chan struct{})
		go func() {
			defer close(done)
			defer remote.Close()
			request, err := readFrame(remote)
			if err == nil {
				peer := Client{conn: remote}
				_ = peer.write(2, response.flags, request.id, response.data)
			}
		}()
		_, err := client.Batch(context.Background(), []Request{{"GET", []Value{VString("key")}}})
		var failure *Error
		if !errors.As(err, &failure) || failure.Code != "ERR_CLIENT_PROTOCOL" || !failure.OutcomeUnknown() {
			t.Fatalf("malformed batch: %v", err)
		}
		client.Close()
		<-done
	}
}
