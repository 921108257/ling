// Package linger 实现原生 Go 客户端，无 cgo；所有整数保留协议精度。
// Package linger implements a native Go client without cgo, preserving wire integer precision.
package linger

import (
	"bytes"
	"encoding/binary"
	"errors"
	"math"
	"sort"
	"unicode/utf8"
)

// Kind 是冻结的类型标记；Kind is a frozen wire type tag.
type Kind byte

const (
	// Null 表示成功空值；Null is a successful null value.
	Null Kind = iota
	// Bool 是布尔；Bool is boolean.
	Bool
	// Int 是有符号 64 位整数；Int is signed 64-bit.
	Int
	// UInt 是无符号 64 位整数；UInt is unsigned 64-bit.
	UInt
	// Float 是不含 NaN 的 IEEE 754；Float is IEEE 754 without NaN.
	Float
	// String 是有效 UTF-8；String is valid UTF-8.
	String
	// Bytes 与 String 严格区分；Bytes is distinct from String.
	Bytes
	// List 是有序 Value；List is ordered Values.
	List
	// Map 是非空 UTF-8 字段名映射；Map uses nonempty UTF-8 field names.
	Map
	// Set 只包含标量；Set contains only scalars.
	Set
)

// Value 保留显式标签；Data 必须与 Kind 匹配，编码前完整校验。
// Value retains an explicit tag; Data must match Kind and is validated before encoding.
// Data: nil/bool/int64/uint64/float64/string/[]byte/[]Value/map[string]Value/[]Value.
type Value struct {
	// Kind 明确区分数字和二进制类型；Kind distinguishes numeric and binary types.
	Kind Kind
	// Data 必须匹配文档中的 Go 类型；Data must use the documented Go type.
	Data any
}

// VString 构造 String；VString constructs a String.
func VString(s string) Value { return Value{String, s} }

// VUInt 构造 UInt；VUInt constructs a UInt.
func VUInt(n uint64) Value { return Value{UInt, n} }

// Equal 按类型比较，Bytes 按内容、零归一化、Map/Set 无序。
// Equal is type-sensitive, compares byte contents, normalizes zero, and ignores Map/Set order.
func (v Value) Equal(other Value) bool {
	a, e := EncodeValue(v)
	if e != nil {
		return false
	}
	b, e := EncodeValue(other)
	return e == nil && bytes.Equal(a, b)
}

// DecodeError 区分可界定业务错误与致命结构错误。
// DecodeError distinguishes delimited semantic errors from fatal structural errors.
type DecodeError struct {
	// Code 是稳定错误；Code is the stable error.
	Code string
	// Fatal 为真时结构无法安全继续；Fatal means structural parsing cannot safely continue.
	Fatal bool
}

// Error 不回显不可信输入；Error never echoes untrusted input.
func (e *DecodeError) Error() string { return e.Code }

// 协议诊断不回显值；Protocol diagnostics never echo values.
func invalidValue() error { return &DecodeError{"ERR_INVALID_VALUE", false} }
func malformed() error    { return &DecodeError{"ERR_PROTOCOL", true} }

const maxBytes = 64 * 1024 * 1024

// EncodeValue 在分配前计算总长，规范化 Set 与 Float 零。
// EncodeValue checks total size before allocation and canonicalizes Sets and Float zero.
func EncodeValue(v Value) ([]byte, error) {
	n, e := valueSize(v, 0)
	if e != nil {
		return nil, e
	}
	out := make([]byte, 0, n)
	return appendValue(out, v), nil
}

// 业务深度上限八，封套另用更宽上限；Business depth is eight; envelopes use a separate allowance.
func valueSize(v Value, depth int) (int, error) { return size(v, depth, 8) }
func size(v Value, depth, limit int) (int, error) {
	if depth > limit {
		return 0, invalidValue()
	}
	n := 1
	switch v.Kind {
	case Null:
		if v.Data != nil {
			return 0, invalidValue()
		}
	case Bool:
		if _, ok := v.Data.(bool); !ok {
			return 0, invalidValue()
		}
		n++
	case Int:
		if _, ok := v.Data.(int64); !ok {
			return 0, invalidValue()
		}
		n += 8
	case UInt:
		if _, ok := v.Data.(uint64); !ok {
			return 0, invalidValue()
		}
		n += 8
	case Float:
		f, ok := v.Data.(float64)
		if !ok || math.IsNaN(f) {
			return 0, invalidValue()
		}
		n += 8
	case String:
		s, ok := v.Data.(string)
		if !ok || !utf8.ValidString(s) {
			return 0, invalidValue()
		}
		n += 4 + len(s)
	case Bytes:
		b, ok := v.Data.([]byte)
		if !ok {
			return 0, invalidValue()
		}
		n += 4 + len(b)
	case List, Set:
		a, ok := v.Data.([]Value)
		if !ok || depth == limit {
			return 0, invalidValue()
		}
		n += 4
		for _, x := range a {
			if v.Kind == Set && x.Kind > Bytes {
				return 0, invalidValue()
			}
			m, e := size(x, depth+1, limit)
			if e != nil {
				return 0, e
			}
			n += m
			if n > maxBytes {
				return 0, invalidValue()
			}
		}
	case Map:
		m, ok := v.Data.(map[string]Value)
		if !ok || depth == limit {
			return 0, invalidValue()
		}
		n += 4
		for k, x := range m {
			if k == "" || !utf8.ValidString(k) {
				return 0, invalidValue()
			}
			m, e := size(x, depth+1, limit)
			if e != nil {
				return 0, e
			}
			n += 5 + len(k) + m
			if n > maxBytes {
				return 0, invalidValue()
			}
		}
	default:
		return 0, invalidValue()
	}
	if n > maxBytes {
		return 0, invalidValue()
	}
	return n, nil
}

// 经校验后追加，无无界中间字符串；Appends validated values without unbounded intermediate strings.
func appendValue(out []byte, v Value) []byte {
	out = append(out, byte(v.Kind))
	switch v.Kind {
	case Null:
	case Bool:
		if v.Data.(bool) {
			out = append(out, 1)
		} else {
			out = append(out, 0)
		}
	case Int:
		out = binary.BigEndian.AppendUint64(out, uint64(v.Data.(int64)))
	case UInt:
		out = binary.BigEndian.AppendUint64(out, v.Data.(uint64))
	case Float:
		f := v.Data.(float64)
		if f == 0 {
			f = 0
		}
		out = binary.BigEndian.AppendUint64(out, math.Float64bits(f))
	case String:
		s := v.Data.(string)
		out = binary.BigEndian.AppendUint32(out, uint32(len(s)))
		out = append(out, s...)
	case Bytes:
		b := v.Data.([]byte)
		out = binary.BigEndian.AppendUint32(out, uint32(len(b)))
		out = append(out, b...)
	case List:
		a := v.Data.([]Value)
		out = binary.BigEndian.AppendUint32(out, uint32(len(a)))
		for _, x := range a {
			out = appendValue(out, x)
		}
	case Map:
		m := v.Data.(map[string]Value)
		keys := make([]string, 0, len(m))
		for k := range m {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		out = binary.BigEndian.AppendUint32(out, uint32(len(keys)))
		for _, k := range keys {
			out = appendValue(out, VString(k))
			out = appendValue(out, m[k])
		}
	case Set:
		a := v.Data.([]Value)
		encoded := make([][]byte, 0, len(a))
		for _, x := range a {
			encoded = append(encoded, appendValue(nil, x))
		}
		sort.Slice(encoded, func(i, j int) bool { return bytes.Compare(encoded[i], encoded[j]) < 0 })
		unique := encoded[:0]
		for _, x := range encoded {
			if len(unique) == 0 || !bytes.Equal(unique[len(unique)-1], x) {
				unique = append(unique, x)
			}
		}
		out = binary.BigEndian.AppendUint32(out, uint32(len(unique)))
		for _, x := range unique {
			out = append(out, x...)
		}
	}
	return out
}

// DecodeValue 校验独立 Value，不接受尾随字节；DecodeValue rejects trailing bytes.
func DecodeValue(data []byte) (Value, error) {
	d := decoder{data: data, budget: 128 * 1024 * 1024}
	v, e := d.value(0)
	if e == nil && len(d.data) != 0 {
		e = malformed()
	}
	if e == nil {
		e = d.semantic
	}
	return v, e
}

// 解码预算先于容器分配扣除；Decoding budgets are charged before container allocation.
type decoder struct {
	data     []byte
	budget   int
	semantic error
}

func (d *decoder) take(n int) ([]byte, error) {
	if n < 0 || n > len(d.data) {
		return nil, malformed()
	}
	b := d.data[:n]
	d.data = d.data[n:]
	return b, nil
}
func (d *decoder) count() (int, error) {
	b, e := d.take(4)
	if e != nil {
		return 0, e
	}
	n := binary.BigEndian.Uint32(b)
	if n > maxBytes {
		return 0, malformed()
	}
	return int(n), nil
}
func (d *decoder) charge(n int) error {
	if n > d.budget || n < 0 {
		return malformed()
	}
	d.budget -= n
	return nil
}

// 结构错误优先；业务错误仍消费完整结构；Structural errors take priority; semantic errors still consume structure.
func (d *decoder) value(depth int) (Value, error) {
	b, e := d.take(1)
	if e != nil {
		return Value{}, e
	}
	k := Kind(b[0])
	v := Value{Kind: k}
	if depth > 12 {
		return v, malformed()
	}
	switch k {
	case Null:
	case Bool:
		b, e = d.take(1)
		if e != nil {
			return v, e
		}
		if b[0] > 1 {
			return v, malformed()
		}
		v.Data = b[0] == 1
	case Int, UInt, Float:
		b, e = d.take(8)
		if e != nil {
			return v, e
		}
		n := binary.BigEndian.Uint64(b)
		if k == Int {
			v.Data = int64(n)
		} else if k == UInt {
			v.Data = n
		} else {
			f := math.Float64frombits(n)
			if math.IsNaN(f) {
				d.semantic = invalidValue()
			}
			if f == 0 {
				f = 0
			}
			v.Data = f
		}
	case String, Bytes:
		n, e := d.count()
		if e != nil {
			return v, e
		}
		if e = d.charge(n); e != nil {
			return v, e
		}
		b, e = d.take(n)
		if e != nil {
			return v, e
		}
		if k == String {
			if !utf8.Valid(b) {
				return v, malformed()
			}
			v.Data = string(b)
		} else {
			v.Data = bytes.Clone(b)
		}
	case List, Set, Map:
		n, e := d.count()
		if e != nil {
			return v, e
		}
		minimum := 1
		if k == Map {
			minimum = 6
		}
		if n > len(d.data)/minimum || n > d.budget/64 {
			return v, malformed()
		}
		if e = d.charge(n * 64); e != nil {
			return v, e
		}
		if k == Map {
			m := make(map[string]Value, n)
			for i := 0; i < n; i++ {
				key, e := d.value(depth + 1)
				if e != nil {
					return v, e
				}
				if key.Kind != String {
					return v, malformed()
				}
				s := key.Data.(string)
				x, e := d.value(depth + 1)
				if e != nil {
					return v, e
				}
				if _, ok := m[s]; ok || s == "" {
					d.semantic = invalidValue()
				}
				m[s] = x
			}
			v.Data = m
		} else {
			a := make([]Value, 0, n)
			seen := map[string]bool{}
			for i := 0; i < n; i++ {
				x, e := d.value(depth + 1)
				if e != nil {
					return v, e
				}
				if k == Set && x.Kind > Bytes {
					d.semantic = invalidValue()
				}
				if k == Set && x.Kind <= Bytes && d.semantic == nil {
					encoded := string(appendValue(nil, x))
					if seen[encoded] {
						continue
					}
					seen[encoded] = true
				}
				a = append(a, x)
			}
			v.Data = a
		}
	default:
		return v, malformed()
	}
	return v, nil
}

// ErrClosed 表示连接或池已关闭；ErrClosed indicates a closed connection or pool.
var ErrClosed = errors.New("linger client closed")
