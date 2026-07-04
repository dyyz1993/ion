//! 通用重试机制 — 指数退避 + 封顶 + 固定间隔
//!
//! 策略（你提的需求）:
//!   前面: 指数退避 (1s → 2s → 4s → ...) 自动把控
//!   封顶后: 固定 10 分钟一次
//!   最多 30 次重试
//!   只有"没钱"才永久停止，其余情况都重试
//!
//! 用法:
//!   let result = retry_async(&config, || async {
//!       send_to_worker(...).await
//!   }).await;

use std::time::Duration;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// 重试配置
#[derive(Clone, Debug)]
pub struct RetryConfig {
    /// 最大重试次数（默认 30）
    pub max_retries: u32,
    /// 初始退避时长（默认 1s）
    pub initial_delay: Duration,
    /// 退避封顶（默认 10 分钟 = 600s）
    pub max_delay: Duration,
    /// 封顶后的固定间隔（默认 10 分钟 = 600s）
    pub fixed_delay: Duration,
    /// 指数倍率（默认 2.0）
    pub multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 30,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10 * 60),       // 10 分钟
            fixed_delay: Duration::from_secs(10 * 60),    // 10 分钟
            multiplier: 2.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Retry decision
// ---------------------------------------------------------------------------

/// 重试决策结果
#[derive(Debug, Clone, PartialEq)]
pub enum RetryDecision {
    /// 应该重试（附带建议的退避时长）
    Retry(Duration),
    /// 永久失败，不要再重试（如没钱了）
    AbortPermanent,
    /// 重试用完，但还不是永久失败（只是没次数了）
    TransientExhausted,
    /// 操作成功
    Success,
}

/// 判断一条错误信息是否应该重试，以及采用什么策略
pub fn should_retry(error: &str, attempt: u32, config: &RetryConfig) -> RetryDecision {
    // 达到最大重试次数 → 重试用完，返回 Transient（不是永久放弃）
    if attempt >= config.max_retries {
        return RetryDecision::TransientExhausted;
    }

    // "没钱" → 永久放弃
    let lower = error.to_lowercase();
    if lower.contains("insufficient")
        || lower.contains("insufficient balance")
        || lower.contains("insufficient_quota")
        || lower.contains("payment required")
        || lower.contains("402")
        || lower.contains("quota exceeded")
        || lower.contains("rate limit exceeded")
        || lower.contains("credit")
    {
        // 这些都是资源不足类的错误，不重试
        return RetryDecision::AbortPermanent;
    }

    // 其余情况都重试（超时、5xx、连接失败等）
    let delay = backoff_duration(attempt, config);
    RetryDecision::Retry(delay)
}

/// 计算第 n 次重试的退避时长（公开，供 WorkerRegistry 集成使用）
pub fn backoff_duration(attempt: u32, config: &RetryConfig) -> Duration {
    // 指数退避：initial_delay * multiplier^attempt
    let exp_delay = config.initial_delay.as_secs_f64() * config.multiplier.powi(attempt as i32);

    if exp_delay >= config.max_delay.as_secs_f64() {
        // 封顶了，用固定间隔
        config.fixed_delay
    } else {
        // 指数退避 + 少量抖动（最多 +-10%）
        let jitter = 1.0 + (fast_random(attempt) as i32 % 21 - 10) as f64 / 100.0;
        Duration::from_secs_f64(exp_delay * jitter.max(0.9))
    }
}

/// 一个简单的确定性"随机"函数，用于抖动
fn fast_random(seed: u32) -> u32 {
    // xorshift
    let mut x = seed.wrapping_add(0x9e3779b9);
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

// ---------------------------------------------------------------------------
// RetryError
// ---------------------------------------------------------------------------

/// 重试操作的最终错误
#[derive(Debug)]
pub enum RetryError<E> {
    /// 永久失败（没钱、达最大次数等）
    Permanent {
        reason: String,
        last_error: E,
        attempts: u32,
    },
    /// 最终成功前的最后一次错误（可选）
    Transient {
        error: E,
        attempts: u32,
        /// 如果返回这个，调用方可以选择是否继续
        last_delay: Duration,
    },
}

// ---------------------------------------------------------------------------
// retry_async — 通用重试执行器
// ---------------------------------------------------------------------------

/// 异步重试执行器
///
/// 对 `operation` 进行最多 `max_retries` 次重试。
/// 每次失败后调用 `should_retry` 做决策。
///
/// # 参数
/// - `config`: 重试配置
/// - `operation`: 异步操作，返回 `Result<T, E>`
/// - `should_abort`: 额外判断函数，返回 true 则永久中止（可选）
///
/// # 返回
/// - `Ok(T)`: 操作成功
/// - `Err(RetryError<E>)`: 所有重试都失败后返回最后一次错误
pub async fn retry_async<F, Fut, T, E>(
    config: &RetryConfig,
    operation: F,
) -> Result<T, RetryError<E>>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    retry_async_with_abort(config, operation, |_| false).await
}

/// 带自定义中止判断的异步重试
pub async fn retry_async_with_abort<F, Fut, T, E>(
    config: &RetryConfig,
    operation: F,
    should_abort: impl Fn(&E) -> bool,
) -> Result<T, RetryError<E>>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut last_error: Option<E> = None;

    for attempt in 0..=config.max_retries {
        // 首次尝试不等待，重试才等
        if attempt > 0 {
            let delay = backoff_duration(attempt - 1, config);
            tokio::time::sleep(delay).await;
        }

        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                let error_str = e.to_string();

                // 检查自定义中止条件
                if should_abort(&e) {
                    return Err(RetryError::Permanent {
                        reason: "custom abort condition".into(),
                        last_error: e,
                        attempts: attempt + 1,
                    });
                }

                // 检查通用中止条件
                match should_retry(&error_str, attempt, config) {
                    RetryDecision::AbortPermanent => {
                        return Err(RetryError::Permanent {
                            reason: error_str,
                            last_error: e,
                            attempts: attempt + 1,
                        });
                    }
                    RetryDecision::TransientExhausted => {
                        return Err(RetryError::Transient {
                            error: e,
                            attempts: attempt + 1,
                            last_delay: config.fixed_delay,
                        });
                    }
                    RetryDecision::Retry(delay) => {
                        tracing::warn!(
                            "[retry] attempt {}/{} failed: {:.80} — retrying in {:?}",
                            attempt + 1,
                            config.max_retries + 1,
                            error_str,
                            delay
                        );
                        last_error = Some(e);
                    }
                    RetryDecision::Success => {
                        unreachable!("should_retry should never return Success for an error");
                    }
                }
            }
        }
    }

    // 所有重试都失败
    let err = last_error.expect("retry loop exited without error");
    Err(RetryError::Transient {
        error: err,
        attempts: config.max_retries + 1,
        last_delay: config.fixed_delay,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_config_default() {
        let c = RetryConfig::default();
        assert_eq!(c.max_retries, 30);
        assert_eq!(c.initial_delay, Duration::from_secs(1));
        assert_eq!(c.max_delay, Duration::from_secs(10 * 60));
        assert_eq!(c.fixed_delay, Duration::from_secs(10 * 60));
    }

    #[test]
    fn should_retry_aborts_on_insufficient_balance() {
        let c = RetryConfig::default();
        assert_eq!(should_retry("Insufficient balance", 0, &c), RetryDecision::AbortPermanent);
        assert_eq!(should_retry("insufficient_quota", 0, &c), RetryDecision::AbortPermanent);
        assert_eq!(should_retry("402 Payment Required", 0, &c), RetryDecision::AbortPermanent);
        assert_eq!(should_retry("quota exceeded", 0, &c), RetryDecision::AbortPermanent);
        assert_eq!(should_retry("rate limit exceeded", 0, &c), RetryDecision::AbortPermanent);
    }

    #[test]
    fn should_retry_on_timeout_and_server_errors() {
        let c = RetryConfig::default();
        assert!(matches!(should_retry("timeout", 0, &c), RetryDecision::Retry(_)));
        assert!(matches!(should_retry("500 Internal Server Error", 0, &c), RetryDecision::Retry(_)));
        assert!(matches!(should_retry("connection refused", 0, &c), RetryDecision::Retry(_)));
        assert!(matches!(should_retry("Service Unavailable", 0, &c), RetryDecision::Retry(_)));
    }

    #[test]
    fn should_retry_transient_after_max_retries() {
        let c = RetryConfig::default();
        assert_eq!(should_retry("timeout", 30, &c), RetryDecision::TransientExhausted);
        assert_eq!(should_retry("timeout", 31, &c), RetryDecision::TransientExhausted);
    }

    #[test]
    fn backoff_starts_small_and_grows() {
        let c = RetryConfig::default();
        let d0 = backoff_duration(0, &c);
        let d1 = backoff_duration(1, &c);
        let d2 = backoff_duration(2, &c);
        assert!(d0 < d1, "backoff should grow: {d0:?} < {d1:?}");
        assert!(d1 < d2, "backoff should grow: {d1:?} < {d2:?}");
        // First retry at ~1s, second at ~2s, third at ~4s
        assert!(d0.as_millis() >= 900 && d0.as_millis() <= 1100, "first ~1s: {:?}", d0);
        assert!(d1.as_millis() >= 1800 && d1.as_millis() <= 2200, "second ~2s: {:?}", d1);
    }

    #[test]
    fn backoff_caps_at_max_delay() {
        let c = RetryConfig {
            max_delay: Duration::from_secs(60),  // cap at 1 minute for fast test
            fixed_delay: Duration::from_secs(30), // after cap, 30s
            ..Default::default()
        };
        // After many retries, should be capped at fixed_delay
        let d_big = backoff_duration(50, &c);
        assert_eq!(d_big, Duration::from_secs(30), "should cap at fixed_delay");
    }

    #[test]
    fn should_retry_aborts_on_credit_errors() {
        let c = RetryConfig::default();
        assert_eq!(should_retry("InsufficientBalance", 0, &c), RetryDecision::AbortPermanent);
        assert_eq!(should_retry("insufficient balance in your account", 0, &c), RetryDecision::AbortPermanent);
    }

    #[tokio::test]
    async fn retry_async_success_first_try() {
        let c = RetryConfig::default();
        let result = retry_async(&c, || async { Ok::<_, String>(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn retry_async_success_after_retries() {
        let c = RetryConfig {
            max_retries: 5,
            initial_delay: Duration::from_millis(1),
            ..Default::default()
        };
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = counter.clone();

        let result = retry_async(&c, || {
            let cnt = counter_clone.clone();
            async move {
                let val = cnt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if val < 2 {
                    Err::<i32, String>("timeout".into())
                } else {
                    Ok(42)
                }
            }
        }).await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3,
            "should have failed 2 times then succeed");
    }

    #[tokio::test]
    async fn retry_async_aborts_on_insufficient_balance() {
        let c = RetryConfig {
            max_retries: 5,
            initial_delay: Duration::from_millis(1),
            ..Default::default()
        };
        let result = retry_async(&c, || async {
            Err::<i32, String>("Insufficient balance".into())
        }).await;

        match result {
            Err(RetryError::Permanent { reason, .. }) => {
                assert!(reason.contains("Insufficient"), "should mention insufficient");
            }
            _ => panic!("should have aborted permanently"),
        }
    }

    #[tokio::test]
    async fn retry_async_transient_after_max_retries() {
        let c = RetryConfig {
            max_retries: 3,
            initial_delay: Duration::from_millis(1),
            ..Default::default()
        };
        let result = retry_async(&c, || async {
            Err::<i32, String>("timeout".into())
        }).await;

        match result {
            Err(RetryError::Transient { attempts, .. }) => {
                // 首次尝试 + 3 次重试 = 4 次总尝试
                assert!(attempts >= 3, "should have attempted at least 3 times: {attempts}");
            }
            other => panic!("should have returned transient, got: {other:?}"),
        }
    }

    #[test]
    fn backoff_jitter_does_not_exceed_10_percent() {
        let c = RetryConfig::default();
        // 只在封顶前验证抖动（指数退避阶段）
        // 封顶后返回 fixed_delay，没有抖动
        // max_delay=600 (10min), 2^9=512 < 600, cap at i=10
        for i in 0..9 {  // i=9 gives 512 < 600, still exponential
            let d = backoff_duration(i, &c);
            let expected = c.initial_delay.as_secs_f64() * c.multiplier.powi(i as i32);
            let ratio = d.as_secs_f64() / expected;
            assert!(ratio >= 0.9 && ratio <= 1.1, "jitter out of bounds at {i}: {ratio}");
        }
    }

    // =========================================================================
    // RetryTestHarness — 结构化重试测试框架
    // =========================================================================

    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    #[derive(Debug, Clone)]
    struct AttemptRecord {
        pub attempt: u32,
        pub error: String,
        pub decision: RetryDecision,
        pub elapsed_ms: u64,
    }

    struct RetryTestHarness {
        pub attempts: Vec<AttemptRecord>,
        config: RetryConfig,
        errors: Vec<String>,
        error_idx: Arc<AtomicU32>,
        start: std::time::Instant,
    }

    impl RetryTestHarness {
        fn new(config: RetryConfig, errors: Vec<String>) -> Self {
            Self {
                attempts: Vec::new(),
                config,
                errors,
                error_idx: Arc::new(AtomicU32::new(0)),
                start: std::time::Instant::now(),
            }
        }

        async fn run_with_hook<H>(&mut self, mut _hook: H) -> Result<i32, RetryError<String>>
        where
            H: FnMut(u32, &str, &RetryDecision),
        {
            let cfg = self.config.clone();
            let errors = self.errors.clone();
            let counter = self.error_idx.clone();
            let start = self.start;

            let mut last_error = None;

            for attempt in 0..=cfg.max_retries {
                if attempt > 0 {
                    let delay = backoff_duration(attempt - 1, &cfg);
                    tokio::time::sleep(delay).await;
                }

                let idx = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as usize;
                let elapsed = start.elapsed().as_millis() as u64;

                if idx < errors.len() {
                    let err_msg = format!("err{}:{}", idx, errors[idx]);
                    let decision = should_retry(&err_msg, attempt, &cfg);

                    self.attempts.push(AttemptRecord {
                        attempt,
                        error: err_msg.clone(),
                        decision: decision.clone(),
                        elapsed_ms: elapsed,
                    });

                    match &decision {
                        RetryDecision::AbortPermanent => {
                            return Err(RetryError::Permanent {
                                reason: err_msg.clone(), last_error: err_msg, attempts: attempt + 1,
                            });
                        }
                        RetryDecision::TransientExhausted => {
                            return Err(RetryError::Transient {
                                error: err_msg, attempts: attempt + 1, last_delay: cfg.fixed_delay,
                            });
                        }
                        RetryDecision::Retry(_) => { last_error = Some(err_msg); }
                        RetryDecision::Success => unreachable!(),
                    }
                } else {
                    self.attempts.push(AttemptRecord {
                        attempt, error: "".into(), decision: RetryDecision::Success, elapsed_ms: elapsed,
                    });
                    return Ok(42);
                }
            }

            Err(RetryError::Transient {
                error: last_error.unwrap_or_else(|| "unknown".into()),
                attempts: 0, last_delay: cfg.fixed_delay,
            })
        }
    }

    // =========================================================================
    // Harness 测试用例
    // =========================================================================

    #[tokio::test]
    async fn harness_multiple_error_modes() {
        let cfg = RetryConfig {
            max_retries: 10, initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(50), fixed_delay: Duration::from_millis(30),
            multiplier: 2.0,
        };
        let errs = vec!["timeout".into(), "500".into(), "conn_reset".into(), "timeout".into(), "Insufficient balance".into()];
        let mut h = RetryTestHarness::new(cfg, errs);
        let result = h.run_with_hook(|_, _, _| {}).await;
        match result {
            Err(RetryError::Permanent { reason, attempts, .. }) => {
                assert!(reason.contains("Insufficient"), "abort on insufficient: {reason}");
                assert!(attempts <= 5, "within 5 attempts: {attempts}");
                assert_eq!(h.attempts.len(), 5);
                for i in 0..4 { assert!(matches!(h.attempts[i].decision, RetryDecision::Retry(_)), "{i} retry"); }
                assert!(matches!(h.attempts[4].decision, RetryDecision::AbortPermanent));
            }
            _ => panic!("should abort"),
        }
    }

    #[tokio::test]
    async fn harness_backoff_grows() {
        let cfg = RetryConfig {
            max_retries: 5, initial_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(1000), fixed_delay: Duration::from_millis(500),
            multiplier: 2.0,
        };
        let errs = vec!["timeout".into(); 6];
        let mut h = RetryTestHarness::new(cfg, errs);
        let _ = h.run_with_hook(|_, _, _| {}).await;
        assert!(h.attempts.len() >= 3);
        for i in 1..h.attempts.len().min(6) {
            assert!(h.attempts[i].elapsed_ms > h.attempts[i-1].elapsed_ms,
                "elapsed grows {i}: {} < {}", h.attempts[i-1].elapsed_ms, h.attempts[i].elapsed_ms);
        }
    }

    #[tokio::test]
    async fn harness_success_stops_retrying() {
        let cfg = RetryConfig {
            max_retries: 10, initial_delay: Duration::from_millis(1), ..Default::default()
        };
        let mut h = RetryTestHarness::new(cfg, vec!["timeout".into(), "timeout".into()]);
        assert_eq!(h.run_with_hook(|_, _, _| {}).await.unwrap(), 42);
        assert_eq!(h.attempts.len(), 3, "2 fails + 1 success");
    }

    #[tokio::test]
    async fn harness_no_money_variants() {
        for err in &["insufficient_quota", "InsufficientBalance", "402 Payment Required", "quota exceeded", "rate limit exceeded"] {
            let mut h = RetryTestHarness::new(RetryConfig::default(), vec![err.to_string()]);
            assert!(matches!(h.run_with_hook(|_, _, _| {}).await, Err(RetryError::Permanent { .. })), "abort on: {err}");
        }
    }

    #[tokio::test]
    async fn harness_exhaustion_is_transient() {
        let cfg = RetryConfig { max_retries: 2, initial_delay: Duration::from_millis(1), ..Default::default() };
        let mut h = RetryTestHarness::new(cfg, vec!["timeout".into(); 5]);
        let result = h.run_with_hook(|_, _, _| {}).await;
        match result {
            Err(RetryError::Transient { attempts, .. }) => assert!(attempts > 0, "transient"),
            other => panic!("should be transient: {other:?}"),
        }
    }
}
