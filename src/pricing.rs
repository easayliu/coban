//! 按官方 API 定价估算「这次转发等价多少钱」。
//!
//! **这不是账单**：走订阅（ChatGPT 账号）转发时并不按 token 计费，真正扣的是额度窗口
//! （见上游的 `x-codex-*-used-percent` 头）。这里算的是「同样的 token 数走 API 要花多少」，
//! 用途是横向比较各账号/各设备的消耗强度——用 token 数直接比会让 cached 与非 cached
//! 混作一谈，而两者差一个数量级。
//!
//! 模型认不出来时返回 `None`，调用点记空值而不是记 0：0 会被平均值统计当成真实读数。

/// 每百万 token 的价格（美元）。
#[derive(Debug, Clone, Copy)]
pub struct Price {
    pub input: f64,
    /// 命中缓存的输入 token（`input_tokens_details.cached_tokens`）。
    ///
    /// **官方没给缓存价的档位（各 `*-pro`）在表里填成与 `input` 相同**：那些模型压根不提供
    /// 提示缓存，所以这一列本该用不上；万一上游仍报了 cached_tokens，按未命中价计是唯一
    /// 不会凭空打折的选择——填 0 会让一次昂贵的 pro 调用被算成几乎免费。
    pub cached_input: f64,
    /// 输出 token。**含 reasoning token**：上游把 `reasoning_tokens` 记在
    /// `output_tokens_details` 里，而 `output_tokens` 已经把它算进去了，
    /// 再单独加一次就是重复计费。
    pub output: f64,
}

/// 价目表。键按**前缀**匹配，长者优先（见 [`price_of`]）。
///
/// 取 OpenAI 公布的 API 价格（`developers.openai.com/api/docs/pricing`，2026-08-21 核对）。
/// 新模型没进表时按 `None` 处理——宁可缺一条估算，也不要拿一个猜的价格去乘出一个看着精确
/// 的错数。
///
/// **两处刻意不建模**，故这里的数只是「标准档」下限：
/// - **长上下文加价**：gpt-5.5/5.4 在输入超过约 272K token 时整轮按 2x 输入 / 1.5x 输出计；
/// - **fast 档**：gpt-5.3-codex 之类有一个 2x 的加速档。
///
/// 两者都要额外知道「这次走的是哪一档」，而响应里没有这个信息。反正这张表算的是横向可比的
/// 等价花费，不是账单（见模块头）。
const PRICES: &[(&str, Price)] = &[
    // ---- gpt-5.6 三档 ----
    // **没有裸的 `gpt-5.6`**：官方价目表里不存在这个模型名（实测上游也拒这个名字），
    // 写一条进来只会让 sol/terra/luna 之外的 `gpt-5.6-*` 撞上一个凭空的价格。
    ("gpt-5.6-sol", Price { input: 5.0, cached_input: 0.5, output: 30.0 }),
    ("gpt-5.6-terra", Price { input: 2.0, cached_input: 0.2, output: 12.0 }),
    ("gpt-5.6-luna", Price { input: 0.2, cached_input: 0.02, output: 1.2 }),
    // ---- gpt-5.5 ----
    ("gpt-5.5-pro", Price { input: 30.0, cached_input: 30.0, output: 180.0 }),
    ("gpt-5.5", Price { input: 5.0, cached_input: 0.5, output: 30.0 }),
    // ---- gpt-5.4 ----
    ("gpt-5.4-pro", Price { input: 30.0, cached_input: 30.0, output: 180.0 }),
    ("gpt-5.4-mini", Price { input: 0.75, cached_input: 0.075, output: 4.5 }),
    ("gpt-5.4-nano", Price { input: 0.2, cached_input: 0.02, output: 1.25 }),
    ("gpt-5.4", Price { input: 2.5, cached_input: 0.25, output: 15.0 }),
    // ---- gpt-5.3 / 5.2 ----
    ("gpt-5.3-codex", Price { input: 1.75, cached_input: 0.175, output: 14.0 }),
    ("gpt-5.2-pro", Price { input: 21.0, cached_input: 21.0, output: 168.0 }),
    ("gpt-5.2-codex", Price { input: 1.75, cached_input: 0.175, output: 14.0 }),
    ("gpt-5.2", Price { input: 1.75, cached_input: 0.175, output: 14.0 }),
    // ---- gpt-5.1 / gpt-5 ----
    // `gpt-5.1-codex-max` 不单列：它与 `gpt-5.1-codex` 同价，前缀匹配已经覆盖到。
    ("gpt-5.1-codex-mini", Price { input: 0.25, cached_input: 0.025, output: 2.0 }),
    ("gpt-5.1-codex", Price { input: 1.25, cached_input: 0.125, output: 10.0 }),
    ("gpt-5.1", Price { input: 1.25, cached_input: 0.125, output: 10.0 }),
    ("gpt-5-pro", Price { input: 15.0, cached_input: 15.0, output: 120.0 }),
    ("gpt-5-codex", Price { input: 1.25, cached_input: 0.125, output: 10.0 }),
    ("gpt-5-mini", Price { input: 0.25, cached_input: 0.025, output: 2.0 }),
    ("gpt-5-nano", Price { input: 0.05, cached_input: 0.005, output: 0.4 }),
    ("gpt-5", Price { input: 1.25, cached_input: 0.125, output: 10.0 }),
    // ---- 兜底：o 系列与 4.1，偶尔还会被指定到 ----
    ("o4-mini", Price { input: 1.1, cached_input: 0.275, output: 4.4 }),
    // `o3-mini` 得单列：边界规则允许 `-` 起头的后缀，故不列的话它会被 `o3` 按两倍价吃掉。
    ("o3-mini", Price { input: 1.1, cached_input: 0.55, output: 4.4 }),
    ("o3", Price { input: 2.0, cached_input: 0.5, output: 8.0 }),
    ("gpt-4.1-nano", Price { input: 0.1, cached_input: 0.025, output: 0.4 }),
    ("gpt-4.1-mini", Price { input: 0.4, cached_input: 0.1, output: 1.6 }),
    ("gpt-4.1", Price { input: 2.0, cached_input: 0.5, output: 8.0 }),
];

/// 查一个模型名的价格。
///
/// **按最长前缀匹配**，因为上游回的模型名常带日期/变体后缀（`gpt-5.1-codex-2026-01-01`）。
/// 表里的顺序不能决定结果——`gpt-5` 排在 `gpt-5-mini` 前面时，先到先得会让 mini 被按
/// 完整版计价（贵 5 倍）。故这里显式取最长命中，与表序无关。
///
/// 命中还要求**落在边界上**（见 [`matches_at_boundary`]），否则 `gpt-5` 这条会变成整个
/// `gpt-5.*` 家族的兜底：官方一上 `gpt-5.7`，它就被静默按 `gpt-5` 的价乘出一个看着精确的
/// 错数——而这正是模块头说要避免的事。有了边界，认不出的新模型如实返回 `None`。
pub fn price_of(model: &str) -> Option<Price> {
    let m = model.trim().to_ascii_lowercase();
    PRICES
        .iter()
        .filter(|(prefix, _)| matches_at_boundary(&m, prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, p)| *p)
}

/// 模型名是不是「这个前缀」本身，或「这个前缀 + 一个变体后缀」。
///
/// 判据是**后面紧跟的那个字符必须是 `-`**（或压根没有下一个字符）。OpenAI 的命名里，同一
/// 个模型的所有派生都以 `-` 起头——变体（`-codex`/`-mini`/`-pro`）与日期版本
/// （`-2026-01-01`）都是；而**版本号是直接续在后面的**（`gpt-5` → `gpt-5.1`、`gpt-5.6`）。
/// 于是这一条规则正好把「同一模型的派生」与「下一个版本的模型」分开。
///
/// 反过来放宽（只要 `starts_with` 就算命中）的代价不是估偏一点，而是**每个新版本都会静默
/// 借用上一代的价格**：`gpt-5.4` 曾被按 `gpt-5` 算（少一半）、`gpt-5.6-sol` 被按 `gpt-5` 算
/// （少到四分之一），而界面上照样是一个精确到小数点后四位的数，没有任何症状。
fn matches_at_boundary(model: &str, prefix: &str) -> bool {
    let Some(rest) = model.strip_prefix(prefix) else { return false };
    rest.is_empty() || rest.starts_with('-')
}

/// 估算一次请求的等价费用（美元）。模型未知时返回 `None`。
///
/// `input_tokens` 传上游报的那个原值——**它已经把 cached 部分算在内**，故这里先扣掉
/// cached 再按未命中价计。不扣的话缓存命中率高的会话会被高估好几倍。
pub fn estimate_usd(
    model: &str,
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
) -> Option<f64> {
    let p = price_of(model)?;
    let uncached = (input_tokens - cached_tokens).max(0) as f64;
    let cached = cached_tokens.max(0) as f64;
    let out = output_tokens.max(0) as f64;
    Some((uncached * p.input + cached * p.cached_input + out * p.output) / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表序不能决定结果：`gpt-5` 是 `gpt-5-mini` 的前缀，先到先得会把 mini 按 5 倍价算。
    #[test]
    fn longest_prefix_wins_regardless_of_table_order() {
        assert_eq!(price_of("gpt-5-mini-2026-01-01").unwrap().input, 0.25);
        assert_eq!(price_of("gpt-5-nano").unwrap().input, 0.05);
        assert_eq!(price_of("gpt-5.1-codex-mini").unwrap().input, 0.25);
        assert_eq!(price_of("gpt-5.1-codex").unwrap().input, 1.25);
    }

    /// 官方价目表（2026-08-21 核对）逐项钉住。
    ///
    /// 钉的是**实测上游真会返回的那些名字**：这张表以前把 5.6-sol 按 gpt-5 的价算（少算
    /// 到四分之一）、5.2 与 5.2-codex 也各少算两成，而这类错误没有任何症状——花费栏照样
    /// 显示一个看着精确的数。
    #[test]
    fn official_rates_are_pinned() {
        for (model, input, cached, output) in [
            ("gpt-5.6-sol", 5.0, 0.5, 30.0),
            ("gpt-5.6-terra", 2.0, 0.2, 12.0),
            ("gpt-5.6-luna", 0.2, 0.02, 1.2),
            ("gpt-5.5", 5.0, 0.5, 30.0),
            ("gpt-5.4", 2.5, 0.25, 15.0),
            ("gpt-5.4-mini", 0.75, 0.075, 4.5),
            ("gpt-5.4-nano", 0.2, 0.02, 1.25),
            ("gpt-5.3-codex", 1.75, 0.175, 14.0),
            ("gpt-5.2", 1.75, 0.175, 14.0),
            ("gpt-5.2-codex", 1.75, 0.175, 14.0),
            ("gpt-5.1", 1.25, 0.125, 10.0),
            ("gpt-5", 1.25, 0.125, 10.0),
        ] {
            let p = price_of(model).unwrap_or_else(|| panic!("{model} must be priced"));
            assert_eq!((p.input, p.cached_input, p.output), (input, cached, output), "{model}");
        }
        // 上游把别名解析成带日期的具体版本（实测 `gpt-5.4-mini` → `…-2026-03-17`），
        // 那个名字也得落在同一档上，否则真实流水里反而是没价的那一份。
        assert_eq!(price_of("gpt-5.4-mini-2026-03-17").unwrap().input, 0.75);
    }

    /// 没有裸的 `gpt-5.6`：terra/luna 各有各的价，落到一条凭空的 `gpt-5.6` 上就全错了。
    /// 同理 `gpt-5.4-mini` 不能被 `gpt-5.4` 抢走（贵 3 倍多）。
    #[test]
    fn sibling_variants_do_not_borrow_each_others_price() {
        assert_eq!(price_of("gpt-5.6-terra").unwrap().input, 2.0);
        assert_eq!(price_of("gpt-5.6-luna").unwrap().input, 0.2);
        assert_ne!(price_of("gpt-5.4-mini").unwrap().input, price_of("gpt-5.4").unwrap().input);
        assert_ne!(price_of("gpt-5.5-pro").unwrap().input, price_of("gpt-5.5").unwrap().input);
        assert_ne!(price_of("gpt-4.1-nano").unwrap().input, price_of("gpt-4.1").unwrap().input);
    }

    /// pro 档官方不给缓存价（它们不提供提示缓存）。这一列填 0 会让一次昂贵调用被算成
    /// 几乎免费，故按未命中价计——宁可不打折，也不要凭空打折。
    #[test]
    fn pro_tiers_never_get_a_phantom_cache_discount() {
        for model in ["gpt-5.5-pro", "gpt-5.4-pro", "gpt-5.2-pro", "gpt-5-pro"] {
            let p = price_of(model).unwrap_or_else(|| panic!("{model} must be priced"));
            assert_eq!(p.cached_input, p.input, "{model}");
        }
        // 全命中缓存也得按满价：1M token 的 pro 调用不可能只值几分钱。
        assert!(estimate_usd("gpt-5.5-pro", 1_000_000, 1_000_000, 0).unwrap() >= 30.0);
    }

    #[test]
    fn unknown_model_has_no_price() {
        assert!(price_of("llama-3").is_none());
        assert!(estimate_usd("llama-3", 100, 0, 100).is_none());
    }

    /// **下一个版本不许借上一代的价**：这是历史上真出过的那个 bug——`gpt-5.4` / `gpt-5.6-sol`
    /// 都曾被 `gpt-5` 这条前缀静默兜底，各少算一半到四分之三，而界面上照样是一个精确到小数点
    /// 后四位的数。官方下次上新时，宁可这一栏空着。
    #[test]
    fn a_newer_version_never_borrows_the_previous_ones_price() {
        for unknown in [
            "gpt-5.7",       // 下一个小版本
            "gpt-5.8-sol",   // 下一个小版本的变体
            "gpt-5.9-codex", //
            "gpt-6",         // 下一个大版本
            "gpt-51",        // 数字直接续上，不是 gpt-5 的派生
            "gpt-4.2",       // 4.1 的下一版
            "o5",            // o 系列下一代
            "gpt-5.6",       // 官方无此模型名，不能落到 sol/terra/luna 任一档上
            "gpt-5.6-vega",  // 未知的 5.6 变体，三档的价互不相同，猜哪个都是错的
        ] {
            assert!(price_of(unknown).is_none(), "{unknown} must not borrow a price");
            assert!(estimate_usd(unknown, 1_000, 0, 1_000).is_none(), "{unknown}");
        }
    }

    /// 边界只卡「版本号续写」，同一模型的派生照样要认出来——变体与日期版本都以 `-` 起头。
    #[test]
    fn variant_and_dated_suffixes_still_match() {
        for (model, expect_input) in [
            ("gpt-5", 1.25),
            ("gpt-5-2026-01-01", 1.25),
            ("gpt-5.1-codex-max", 1.25),
            ("gpt-5.1-codex-2026-01-01", 1.25),
            ("gpt-5.4-mini-2026-03-17", 0.75),
            ("gpt-5.6-sol-2026-05-01", 5.0),
            ("o4-mini-2025-04-16", 1.1),
        ] {
            let p = price_of(model).unwrap_or_else(|| panic!("{model} must still be priced"));
            assert_eq!(p.input, expect_input, "{model}");
        }
        // `o3-mini` 单列了一档，不能被 `o3` 按两倍价吃掉。
        assert_eq!(price_of("o3-mini").unwrap().input, 1.1);
        assert_eq!(price_of("o3").unwrap().input, 2.0);
    }

    /// cached 部分要从 input 里扣掉再按缓存价计，否则命中率高的会话被高估。
    #[test]
    fn cached_tokens_are_not_double_charged() {
        // 1M input（其中 800k 命中缓存）+ 0 output
        let usd = estimate_usd("gpt-5.1-codex", 1_000_000, 800_000, 0).unwrap();
        let expect = 200_000.0 * 1.25 / 1e6 + 800_000.0 * 0.125 / 1e6;
        assert!((usd - expect).abs() < 1e-9, "{usd} vs {expect}");
        // 全部命中缓存时不该出现负数的未命中部分
        assert!(estimate_usd("gpt-5.1-codex", 100, 500, 0).unwrap() > 0.0);
    }
}
