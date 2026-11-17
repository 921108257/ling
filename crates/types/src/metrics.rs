//! 固定低基数直方图；调用方提供具有明确单位的边界。
//! Fixed low-cardinality histograms with explicitly unit-bearing boundaries.

/// 时延桶上界，单位微秒；最后一个桶接收大于一秒的样本。
/// Latency bucket upper bounds in microseconds; the final bucket holds values above one second.
pub const LATENCY_BUCKET_BOUNDS_US: [u64; 6] = [10, 100, 1_000, 10_000, 100_000, 1_000_000];

/// 完整追加记录的字节桶上界，包括十六字节记录封套。
/// Byte bucket upper bounds for complete appended records, including the sixteen-byte envelope.
pub const JOURNAL_TRANSACTION_BUCKET_BOUNDS_BYTES: [u64; 6] =
    [256, 1_024, 4_096, 16_384, 65_536, 1_048_576];

/// 七个互斥桶的进程内快照；所有累计值饱和而不回绕。
/// Process-local snapshot of seven disjoint buckets; all accumulated values saturate without wrapping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistogramSnapshot {
    /// 前六桶包含各自上界，第七桶为溢出桶；导出 Prometheus 时累加。
    /// The first six buckets include their upper bounds; the seventh is overflow, accumulated on Prometheus export.
    pub buckets: [u64; 7],
    /// 按所属指标单位累计的样本总和；Sum of samples in the owning metric's unit.
    pub sum: u64,
    /// 样本数；Number of samples.
    pub count: u64,
}

impl HistogramSnapshot {
    /// 按递增上界记录样本；样本和边界必须使用同一单位。
    /// Records a sample against increasing upper bounds, using the same unit for the sample and bounds.
    pub fn observe(&mut self, value: u64, bounds: &[u64; 6]) {
        let bucket = bounds.iter().position(|&bound| value <= bound).unwrap_or(6);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.sum = self.sum.saturating_add(value);
        self.count = self.count.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 边界等值属于较小桶，溢出及累计饱和不回绕。
    /// Boundary equality belongs to the lower bucket; overflow and saturated totals never wrap.
    #[test]
    fn histogram_boundaries_and_saturation_are_stable() {
        let mut histogram = HistogramSnapshot::default();
        for value in [0, 10, 11, 100, 101, 1_000_000, 1_000_001] {
            histogram.observe(value, &LATENCY_BUCKET_BOUNDS_US);
        }
        assert_eq!(histogram.buckets, [2, 2, 1, 0, 0, 1, 1]);
        assert_eq!(histogram.count, 7);
        assert_eq!(histogram.sum, 2_000_223);
        histogram.count = u64::MAX;
        histogram.sum = u64::MAX;
        histogram.buckets[6] = u64::MAX;
        histogram.observe(u64::MAX, &LATENCY_BUCKET_BOUNDS_US);
        assert_eq!(histogram.count, u64::MAX);
        assert_eq!(histogram.sum, u64::MAX);
        assert_eq!(histogram.buckets[6], u64::MAX);
    }
}
