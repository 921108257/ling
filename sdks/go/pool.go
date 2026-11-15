// 固定数据库与身份的连接池；只重建连接，不重放请求。
// Pool fixes database and credentials, rebuilding connections without replaying requests.
package linger

import (
	"context"
	"sync"
	"sync/atomic"
)

// Pool 有界保存连接，Close 会中止在用连接并唤醒排队者。
// Pool bounds socket ownership; Close interrupts active sockets and wakes waiters.
type Pool struct {
	options Options
	slots   chan struct{}
	mutex   sync.Mutex
	idle    []*Client
	active  map[*Client]bool
	closing context.Context
	cancel  context.CancelFunc
	once    sync.Once
}

// NewPool 延迟初始化连接；容量必须为 1..65536。
// NewPool initializes sockets lazily; capacity must be 1..65536.
func NewPool(options Options, capacity int) (*Pool, error) {
	if capacity < 1 || capacity > 65536 {
		return nil, &Error{Code: "ERR_CLIENT_CONFIG"}
	}
	closing, cancel := context.WithCancel(context.Background())
	return &Pool{options: options, slots: make(chan struct{}, capacity), active: map[*Client]bool{}, closing: closing, cancel: cancel}, nil
}

// Lease 固定上下文的独占租约，可在同连接上继续 SCAN。
// Lease is an exclusive fixed-context handle supporting connection-bound SCAN.
type Lease struct {
	pool     *Pool
	client   *Client
	released atomic.Bool
}

// Acquire 取消建连和排队均归还容量；池关闭也取消建连。
// Acquire returns capacity on cancellation; pool shutdown also cancels connecting sockets.
func (p *Pool) Acquire(ctx context.Context) (*Lease, error) {
	select {
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-p.closing.Done():
		return nil, ErrClosed
	case p.slots <- struct{}{}:
	}
	owned := false
	defer func() {
		if !owned {
			<-p.slots
		}
	}()
	p.mutex.Lock()
	var c *Client
	if len(p.idle) > 0 {
		c = p.idle[len(p.idle)-1]
		p.idle = p.idle[:len(p.idle)-1]
	}
	p.mutex.Unlock()
	if c == nil {
		connecting, cancel := context.WithCancel(ctx)
		stop := context.AfterFunc(p.closing, cancel)
		var err error
		c, err = Connect(connecting, p.options)
		stop()
		cancel()
		if err != nil {
			return nil, err
		}
	}
	p.mutex.Lock()
	defer p.mutex.Unlock()
	select {
	case <-p.closing.Done():
		c.Close()
		return nil, ErrClosed
	default:
	}
	p.active[c] = true
	owned = true
	return &Lease{pool: p, client: c}, nil
}

// Execute 普通命令不会修改连接的身份、数据库或订阅状态。
// Execute never changes connection identity, database, or subscription state.
func (l *Lease) Execute(ctx context.Context, command string, args ...Value) (Value, error) {
	if l.released.Load() {
		return Value{}, ErrClosed
	}
	switch stringsUpper(command) {
	case "HELLO", "AUTH", "SELECT", "QUIT", "SUBSCRIBE", "UNSUBSCRIBE", "ACK":
		return Value{}, &Error{Code: "ERR_CONNECTION_STATE"}
	}
	return l.client.Execute(ctx, command, args...)
}

// Close 归还健康连接；调用方须在所有租约调用结束后归还。
// Close returns healthy sockets; callers return leases after their calls finish.
func (l *Lease) Close() {
	if l.released.Swap(true) {
		return
	}
	p := l.pool
	p.mutex.Lock()
	delete(p.active, l.client)
	select {
	case <-p.closing.Done():
		l.client.Close()
	case <-l.client.closed:
	default:
		p.idle = append(p.idle, l.client)
	}
	p.mutex.Unlock()
	<-p.slots
}

// Execute 单次调用取得独占租约且不重放失败请求。
// Execute acquires an exclusive lease for one call without replaying failures.
func (p *Pool) Execute(ctx context.Context, command string, args ...Value) (Value, error) {
	lease, err := p.Acquire(ctx)
	if err != nil {
		return Value{}, err
	}
	defer lease.Close()
	return lease.Execute(ctx, command, args...)
}

// Close 幂等清理所有连接并取消建连；不会删除消费组。
// Close idempotently closes all sockets and cancels connects without deleting groups.
func (p *Pool) Close() {
	p.once.Do(func() {
		p.cancel()
		p.mutex.Lock()
		defer p.mutex.Unlock()
		for _, c := range p.idle {
			c.Close()
		}
		p.idle = nil
		for c := range p.active {
			c.Close()
		}
	})
}

// ASCII 大写沿用协议命令规则；ASCII uppercase follows protocol command rules.
func stringsUpper(s string) string {
	b := []byte(s)
	for i, x := range b {
		if x >= 'a' && x <= 'z' {
			b[i] = x - 32
		}
	}
	return string(b)
}
