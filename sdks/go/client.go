// 原生 TCP/TLS、请求匹配与专用订阅；不自动重放任何请求。
// Native TCP/TLS, request matching, and dedicated subscriptions without automatic replay.
package linger

import (
	"context"
	"crypto/tls"
	"encoding/binary"
	"errors"
	"io"
	"net"
	"strings"
	"sync"
	"time"
)

// Error 保留服务端稳定码以及传输结果未知状态。
// Error preserves server codes and unknown transport outcomes.
type Error struct {
	// Code 是稳定服务端码或客户端分类；Code is a stable server code or client category.
	Code string
	// Unknown 表示发送后无法确定结果；Unknown indicates uncertainty after sending.
	Unknown bool
}

// Error 只显示稳定码及不确定性，不携带请求内容。
// Error prints only the stable code and uncertainty without request contents.
func (e *Error) Error() string {
	if e.Unknown {
		return e.Code + "; outcome unknown"
	}
	return e.Code
}

// OutcomeUnknown 明确表示写入可能已执行；OutcomeUnknown means a write may have executed.
func (e *Error) OutcomeUnknown() bool { return e.Unknown || e.Code == "ERR_COMMIT_UNKNOWN" }

// Options 固定连接上下文；TLS 必须验证证书，不接受 InsecureSkipVerify。
// Options fixes connection context; TLS verifies certificates and rejects InsecureSkipVerify.
type Options struct {
	// Address 是 host:port；Address is host:port.
	Address string
	// Password 为空则不发送 AUTH；An empty Password omits AUTH.
	Password string
	// Database 是初始化选择的数据库；Database is selected during initialization.
	Database uint16
	// ConnectTimeout 默认五秒；ConnectTimeout defaults to five seconds.
	ConnectTimeout time.Duration
	// RequestTimeout 默认三十秒；RequestTimeout defaults to thirty seconds.
	RequestTimeout time.Duration
	// TLS 为空使用明文，提供时必须验证证书；Nil TLS uses TCP; configured TLS must verify certificates.
	TLS *tls.Config
}

// Client 串行独占连接，后台只预读一帧；Close 可并发中止等待。
// Client serializes socket ownership with one prefetched frame; Close concurrently interrupts waits.
type Client struct {
	db         uint16
	conn       net.Conn
	options    Options
	gate       chan struct{}
	incoming   chan frameResult
	closed     chan struct{}
	once       sync.Once
	last       uint64
	stream     [16]byte
	events     bool
	subscribed bool
	queue      []Value
	queueBytes int
}

// 帧在分配前检查长度；Frame lengths are checked before allocation.
type frame struct {
	kind    byte
	flags   uint16
	id      uint64
	payload []byte
}
type frameResult struct {
	frame frame
	err   error
}

// Connect 完成 TLS/HELLO/AUTH/SELECT 后才返回；失败释放全部资源。
// Connect returns after TLS/HELLO/AUTH/SELECT and releases all resources on failure.
func Connect(ctx context.Context, options Options) (*Client, error) {
	if options.ConnectTimeout == 0 {
		options.ConnectTimeout = 5 * time.Second
	}
	if options.RequestTimeout == 0 {
		options.RequestTimeout = 30 * time.Second
	}
	if options.ConnectTimeout < 0 || options.RequestTimeout < 0 {
		return nil, &Error{Code: "ERR_CLIENT_CONFIG"}
	}
	dialer := net.Dialer{Timeout: options.ConnectTimeout}
	var conn net.Conn
	var err error
	connecting, cancel := context.WithTimeout(ctx, options.ConnectTimeout)
	defer cancel()
	if options.TLS != nil {
		if options.TLS.InsecureSkipVerify {
			return nil, &Error{Code: "ERR_CLIENT_CONFIG"}
		}
		config := options.TLS.Clone()
		if config.MinVersion < tls.VersionTLS12 {
			config.MinVersion = tls.VersionTLS12
		}
		conn, err = (&tls.Dialer{NetDialer: &dialer, Config: config}).DialContext(connecting, "tcp", options.Address)
	} else {
		conn, err = dialer.DialContext(connecting, "tcp", options.Address)
	}
	if err != nil {
		return nil, &Error{Code: "ERR_CLIENT_CONNECT"}
	}
	c := &Client{db: options.Database, conn: conn, options: options, gate: make(chan struct{}, 1), incoming: make(chan frameResult, 1), closed: make(chan struct{})}
	c.gate <- struct{}{}
	go c.readLoop()
	fail := func(e error) (*Client, error) { c.Close(); return nil, e }
	hello, err := c.Execute(ctx, "HELLO", VUInt(1))
	if err != nil {
		return fail(err)
	}
	if hello.Kind != Map {
		return fail(malformed())
	}
	m := hello.Data.(map[string]Value)
	if !m["protocol"].Equal(VUInt(1)) || m["server_version"].Kind != String || m["events"].Kind != Bool || m["reliable_events"].Kind != Bool {
		return fail(malformed())
	}
	c.events = m["events"].Data.(bool)
	if c.events {
		stream := m["stream_id"]
		if stream.Kind != Bytes || len(stream.Data.([]byte)) != 16 {
			return fail(malformed())
		}
		copy(c.stream[:], stream.Data.([]byte))
	} else if m["stream_id"].Kind != Null || m["reliable_events"].Data.(bool) {
		return fail(malformed())
	}
	if options.Password != "" {
		v, e := c.Execute(ctx, "AUTH", VString(options.Password))
		if e != nil {
			return fail(e)
		}
		if !v.Equal(Value{Bool, true}) {
			return fail(malformed())
		}
	}
	selected, e := c.Execute(ctx, "SELECT", VUInt(uint64(options.Database)))
	if e != nil {
		return fail(e)
	}
	if !selected.Equal(Value{Bool, true}) {
		return fail(malformed())
	}
	return c, nil
}

// Close 幂等关闭连接，唤醒读写；Close idempotently closes the socket and wakes readers/writers.
func (c *Client) Close() error {
	var err error
	c.once.Do(func() { close(c.closed); err = c.conn.Close() })
	return err
}

// StreamID 提供恢复身份，不能自动接受变化；StreamID exposes recovery identity without silently accepting changes.
func (c *Client) StreamID() [16]byte { return c.stream }

// RequireEventStream 必须在恢复订阅之前调用；RequireEventStream must precede resubscription.
func (c *Client) RequireEventStream(expected [16]byte) error {
	if !c.events || expected != c.stream {
		c.Close()
		return &Error{Code: "ERR_STREAM_MISMATCH"}
	}
	return nil
}

// Execute 发送强类型参数；串行化包含 AUTH/SELECT/QUIT 屏障。
// Execute sends tagged arguments and serializes AUTH/SELECT/QUIT barriers.
func (c *Client) Execute(ctx context.Context, command string, args ...Value) (Value, error) {
	result, e := c.request(ctx, 1, 0, requestPayload(command, args), 0)
	if e != nil {
		return Value{}, e
	}
	if strings.EqualFold(command, "QUIT") {
		c.Close()
	}
	return result[0], nil
}

// Request 是普通 Batch 子项；Request is an ordinary Batch item.
type Request struct {
	// Command 为 ASCII 命令名；Command is an ASCII command name.
	Command string
	// Arguments 保留协议类型；Arguments retain wire tags.
	Arguments []Value
}

// Result 保留逐项错误；Result retains per-item errors.
type Result struct {
	// Value 在 Err 为空时有效，Null 是成功；Value is valid when Err is nil, including Null.
	Value Value
	// Err 保存逐项稳定错误；Err retains the per-item stable error.
	Err error
}

// Batch 不提供事务性，拒绝自动重放；Batch is nontransactional and never automatically replays.
func (c *Client) Batch(ctx context.Context, requests []Request) ([]Result, error) {
	if len(requests) == 0 || len(requests) > 1000 {
		return nil, invalidValue()
	}
	data := binary.BigEndian.AppendUint32(nil, uint32(len(requests)))
	for _, r := range requests {
		part := requestPayload(r.Command, r.Arguments)
		if part.err != nil {
			return nil, part.err
		}
		if len(data)+len(part.data) > maxBytes {
			return nil, invalidValue()
		}
		data = append(data, part.data...)
	}
	_, e := c.request(ctx, 1, 1, payload(data), len(requests))
	if e != nil {
		var batch *batchResults
		if errors.As(e, &batch) {
			return batch.items, nil
		}
		return nil, e
	}
	return nil, malformed()
}

// 已解析批量通过内部载体返回，不丢失逐项成功；Internal carrier retains every parsed batch result.
type batchResults struct{ items []Result }

func (*batchResults) Error() string { return "batch results" }

// 延迟返回编码错误，避免非法参数消耗请求编号；Delayed encoding errors do not consume request IDs.
type payloadResult struct {
	data []byte
	err  error
}

func payload(data []byte) payloadResult { return payloadResult{data: data} }
func requestPayload(command string, args []Value) payloadResult {
	command = strings.ToUpper(command)
	if command == "" {
		return payloadResult{err: invalidValue()}
	}
	for _, b := range []byte(command) {
		if !((b >= 'A' && b <= 'Z') || (b >= '0' && b <= '9') || b == '_') {
			return payloadResult{err: invalidValue()}
		}
	}
	// 参数封套不消耗业务深度；Argument envelopes do not consume business depth.
	envelope := Value{List, args}
	n, e := size(envelope, 0, 12)
	if e != nil {
		return payloadResult{err: e}
	}
	if n+5+len(command) > maxBytes {
		return payloadResult{err: invalidValue()}
	}
	out := appendValue(nil, VString(command))
	return payload(appendValue(out, envelope))
}

// 专用读任务保留半帧读取进度，任何取消都关闭连接。
// Dedicated reads preserve partial-frame progress; cancellation closes the connection.
func (c *Client) readLoop() {
	for {
		f, e := readFrame(c.conn)
		select {
		case c.incoming <- frameResult{f, e}:
		case <-c.closed:
			return
		}
		if e != nil {
			return
		}
	}
}
func readFrame(r io.Reader) (frame, error) {
	var f frame
	var h [18]byte
	if _, e := io.ReadFull(r, h[:]); e != nil {
		return f, e
	}
	f.kind = h[3]
	f.flags = binary.BigEndian.Uint16(h[4:6])
	f.id = binary.BigEndian.Uint64(h[6:14])
	n := uint64(binary.BigEndian.Uint32(h[14:]))
	if string(h[:2]) != "LG" || h[2] != 1 || f.kind < 1 || f.kind > 6 || f.flags > 1 || (f.flags == 1 && f.kind != 1 && f.kind != 2) || n > maxBytes || (f.kind == 5 && (f.id != 0 || n != 0)) || (f.kind == 3 && f.id != 0) {
		return f, malformed()
	}
	f.payload = make([]byte, int(n))
	_, e := io.ReadFull(r, f.payload)
	return f, e
}
func (c *Client) write(kind byte, flags uint16, id uint64, data []byte) error {
	var h [18]byte
	copy(h[:2], "LG")
	h[2] = 1
	h[3] = kind
	binary.BigEndian.PutUint16(h[4:6], flags)
	binary.BigEndian.PutUint64(h[6:14], id)
	binary.BigEndian.PutUint32(h[14:], uint32(len(data)))
	for _, part := range [][]byte{h[:], data} {
		for len(part) > 0 {
			n, e := c.conn.Write(part)
			if e != nil {
				return e
			}
			if n == 0 {
				return io.ErrShortWrite
			}
			part = part[n:]
		}
	}
	return nil
}

// 一次请求只由一个持有者发送；取消在发送后明确保留未知状态。
// One owner sends each request; cancellation after sending retains unknown outcome.
func (c *Client) request(ctx context.Context, kind byte, flags uint16, p payloadResult, batch int) (values []Value, err error) {
	if p.err != nil {
		return nil, p.err
	}
	defer func() {
		var decode *DecodeError
		if errors.As(err, &decode) {
			c.Close()
			err = &Error{Code: "ERR_CLIENT_PROTOCOL", Unknown: true}
		}
	}()
	ctx, cancel := context.WithTimeout(ctx, c.options.RequestTimeout)
	defer cancel()
	select {
	case <-ctx.Done():
		return nil, &Error{Code: "ERR_CLIENT_CANCELLED"}
	case <-c.closed:
		return nil, ErrClosed
	case <-c.gate:
	}
	defer func() { c.gate <- struct{}{} }()
	if c.subscribed && kind == 1 {
		if flags == 1 {
			return nil, &Error{Code: "ERR_CONNECTION_STATE"}
		}
		d := decoder{data: p.data, budget: 128 * 1024 * 1024}
		name, e := d.value(0)
		if e != nil {
			return nil, e
		}
		if name.Data != "PING" && name.Data != "UNSUBSCRIBE" && name.Data != "QUIT" {
			return nil, &Error{Code: "ERR_CONNECTION_STATE"}
		}
	}
	c.last++
	if c.last == 0 {
		c.Close()
		return nil, ErrClosed
	}
	stop := context.AfterFunc(ctx, func() { c.Close() })
	defer stop()
	deadline, _ := ctx.Deadline()
	c.conn.SetWriteDeadline(deadline)
	if e := c.write(kind, flags, c.last, p.data); e != nil {
		c.Close()
		return nil, &Error{Code: "ERR_CLIENT_DISCONNECTED", Unknown: true}
	}
	for {
		select {
		case <-ctx.Done():
			c.Close()
			return nil, &Error{Code: "ERR_CLIENT_TIMEOUT", Unknown: true}
		case <-c.closed:
			return nil, &Error{Code: "ERR_CLIENT_CLOSED", Unknown: true}
		case received := <-c.incoming:
			if received.err != nil {
				c.Close()
				return nil, &Error{Code: "ERR_CLIENT_DISCONNECTED", Unknown: true}
			}
			f := received.frame
			if f.kind == 5 {
				continue
			}
			if f.kind == 3 && c.subscribed {
				v, e := DecodeValue(f.payload)
				if e != nil {
					c.Close()
					return nil, e
				}
				if e = c.validateEvent(v); e != nil {
					c.Close()
					return nil, e
				}
				if len(c.queue) >= 1024 || c.queueBytes+len(f.payload) > maxBytes/2 {
					c.Close()
					return nil, &Error{Code: "ERR_CLIENT_BUFFER_FULL", Unknown: true}
				}
				c.queue = append(c.queue, v)
				c.queueBytes += len(f.payload)
				continue
			}
			if f.kind == 2 && f.id > 0 && f.id < c.last {
				continue
			}
			if (f.kind != 2 && f.kind != 6) || f.id != c.last {
				c.Close()
				return nil, &Error{Code: "ERR_CLIENT_PROTOCOL", Unknown: true}
			}
			d := decoder{data: f.payload, budget: 128 * 1024 * 1024}
			if f.kind == 6 {
				e := decodeServerError(&d)
				c.Close()
				return nil, e
			}
			if f.flags == 1 && batch > 0 {
				n, e := d.count()
				if e != nil || n != batch {
					c.Close()
					return nil, malformed()
				}
				items := make([]Result, 0, n)
				for i := 0; i < n; i++ {
					v, e := decodeResponse(&d)
					var server *Error
					if e != nil && !errors.As(e, &server) {
						return nil, e
					}
					items = append(items, Result{v, e})
				}
				if len(d.data) != 0 || d.semantic != nil {
					c.Close()
					return nil, malformed()
				}
				return nil, &batchResults{items}
			}
			if f.flags != 0 {
				c.Close()
				return nil, malformed()
			}
			v, e := decodeResponse(&d)
			if len(d.data) != 0 || d.semantic != nil {
				c.Close()
				return nil, malformed()
			}
			if batch > 0 && e == nil {
				return nil, malformed()
			}
			if e == nil && kind == 1 && flags == 0 {
				command := decoder{data: p.data, budget: 128 * 1024 * 1024}
				name, _ := command.value(0)
				arguments, _ := command.value(0)
				if name.Data == "SUBSCRIBE" && v.Equal(Value{Bool, true}) {
					c.subscribed = true
				}
				if name.Data == "UNSUBSCRIBE" && v.Equal(Value{Bool, true}) {
					c.subscribed = false
					c.queue = nil
					c.queueBytes = 0
				}
				if name.Data == "SELECT" && v.Equal(Value{Bool, true}) {
					if a, ok := arguments.Data.([]Value); ok && len(a) == 1 && a[0].Kind == UInt && a[0].Data.(uint64) <= 65535 {
						c.db = uint16(a[0].Data.(uint64))
					}
				}
			}
			return []Value{v}, e
		}
	}
}

// 服务端错误必须是两个 String，不把业务错误混同结构损坏。
// Server errors require two Strings and remain distinct from structural corruption.
func decodeServerError(d *decoder) error {
	code, e := d.value(0)
	if e != nil {
		return e
	}
	message, e := d.value(0)
	if e != nil {
		return e
	}
	if code.Kind != String || message.Kind != String {
		return malformed()
	}
	return &Error{Code: code.Data.(string)}
}

// 成功 Null 仍返回 nil error；A successful Null still returns a nil error.
func decodeResponse(d *decoder) (Value, error) {
	status, e := d.take(1)
	if e != nil {
		return Value{}, e
	}
	if status[0] == 0 {
		return d.value(0)
	}
	if status[0] == 1 {
		return Value{}, decodeServerError(d)
	}
	return Value{}, malformed()
}

// Subscriber 独占连接，必须在业务成功后显式 ACK。
// Subscriber owns a dedicated connection and requires explicit ACK after business success.
type Subscriber struct{ client *Client }

// Subscribe 消耗普通连接的业务状态，命名组从持久进度继续。
// Subscribe dedicates the connection and resumes named groups at durable progress.
func (c *Client) Subscribe(ctx context.Context, patterns []string, group string) (*Subscriber, error) {
	a := make([]Value, 0, len(patterns))
	for _, p := range patterns {
		a = append(a, VString(p))
	}
	args := []Value{{List, a}}
	if group != "" {
		args = append(args, VString("GROUP"), VString(group))
	}
	v, e := c.Execute(ctx, "SUBSCRIBE", args...)
	if e != nil {
		return nil, e
	}
	if !v.Equal(Value{Bool, true}) {
		c.Close()
		return nil, malformed()
	}
	return &Subscriber{c}, nil
}

// Next 返回完整事件 Map，空闲时心跳；取消将关闭连接并保留未确认进度。
// Next returns a full event Map with idle heartbeats; cancellation closes the connection without ACK.
func (s *Subscriber) Next(ctx context.Context) (Value, error) {
	c := s.client
	select {
	case <-ctx.Done():
		return Value{}, ctx.Err()
	case <-c.closed:
		return Value{}, ErrClosed
	case <-c.gate:
	}
	defer func() { c.gate <- struct{}{} }()
	if len(c.queue) > 0 {
		v := c.queue[0]
		c.queue = c.queue[1:]
		encoded, _ := EncodeValue(v)
		c.queueBytes -= len(encoded)
		return v, nil
	}
	tick := time.NewTicker(max(time.Millisecond, min(c.options.RequestTimeout/2, 10*time.Second)))
	defer tick.Stop()
	for {
		select {
		case <-ctx.Done():
			c.Close()
			return Value{}, ctx.Err()
		case <-c.closed:
			return Value{}, ErrClosed
		case <-tick.C:
			c.conn.SetWriteDeadline(time.Now().Add(c.options.RequestTimeout))
			if e := c.write(5, 0, 0, nil); e != nil {
				c.Close()
				return Value{}, e
			}
		case r := <-c.incoming:
			if r.err != nil {
				c.Close()
				return Value{}, r.err
			}
			if r.frame.kind == 5 {
				continue
			}
			if r.frame.kind == 6 {
				d := decoder{data: r.frame.payload, budget: 128 * 1024 * 1024}
				c.Close()
				return Value{}, decodeServerError(&d)
			}
			if r.frame.kind != 3 {
				c.Close()
				return Value{}, malformed()
			}
			v, e := DecodeValue(r.frame.payload)
			if e == nil {
				e = c.validateEvent(v)
			}
			if e != nil {
				c.Close()
			}
			return v, e
		}
	}
}

// Ack 不重放，服务端错误保留原码；Ack never replays and retains server error codes.
func (s *Subscriber) Ack(ctx context.Context, id Value) error {
	data, e := EncodeValue(id)
	if e != nil {
		return e
	}
	v, e := s.client.request(ctx, 4, 0, payload(data), 0)
	if e == nil && !v[0].Equal(Value{Bool, true}) {
		return malformed()
	}
	return e
}

// Close 释放专用连接，不删除命名组；Close releases the socket without deleting its named group.
func (s *Subscriber) Close() error { return s.client.Close() }

// 验证复合身份的宽度与数据库边界，事件不包含缓存 Value。
// Validates compound identity widths and database bounds; events contain no cached Value.
func (c *Client) validateEvent(v Value) error {
	if v.Kind != Map {
		return malformed()
	}
	m := v.Data.(map[string]Value)
	if len(m) != 6 || m["key"].Kind != String || m["key"].Data == "" || m["event_type"].Kind != String || m["event_id"].Kind != Map || m["generation"].Kind != Map {
		return malformed()
	}
	id := m["event_id"].Data.(map[string]Value)
	if len(id) != 4 || id["stream_id"].Kind != Bytes || id["db"].Kind != UInt || id["shard"].Kind != UInt || id["sequence"].Kind != UInt {
		return malformed()
	}
	if !id["stream_id"].Equal(Value{Bytes, c.stream[:]}) || id["db"].Data.(uint64) != uint64(c.db) || id["shard"].Data.(uint64) > mathMaxUint32 || id["sequence"].Data.(uint64) == 0 {
		return malformed()
	}
	g := m["generation"].Data.(map[string]Value)
	if len(g) != 2 || g["epoch"].Kind != Bytes || len(g["epoch"].Data.([]byte)) != 16 || g["counter"].Kind != UInt || g["counter"].Data.(uint64) == 0 || m["revision"].Kind != UInt || m["revision"].Data.(uint64) == 0 || m["timestamp_ms"].Kind != UInt {
		return malformed()
	}
	if !strings.Contains("|KEY_CREATED|KEY_UPDATED|KEY_EXPIRED|KEY_DELETED|KEY_EVICTED|KEY_TTL_CHANGED|", "|"+m["event_type"].Data.(string)+"|") {
		return malformed()
	}
	return nil
}

const mathMaxUint32 = uint64(1<<32 - 1)
