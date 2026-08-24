//! 按官方 API 定价估算「这次转发等价多少钱」。
//!
//! **这不是账单**：走订阅（ChatGPT 账号）转发时并不按 token 计费，真正扣的是额度窗口
//! （见上游的 `x-codex-*-used-percent` 头）。这里算的是「同样的 token 数走 API 要花多少」，
//! 用途是横向比较各账号/各设备的消耗强度——用 token 数直接比会让 cached 与非 cached
//! 混作一谈，而两者差一个数量级。
//!
//! 模型认不出来时返回 `None`，调用点记空值而不是记 0：0 会被平均值统计当成真实读数。

/// 一档价格：每百万 token 多少美元。
#[derive(Debug, Clone, Copy)]
pub struct Rate {
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

/// 一个模型的价格：标准档，以及（如果官方给了）长上下文档。
#[derive(Debug, Clone, Copy)]
pub struct Price {
    pub standard: Rate,
    /// 长上下文档。本轮输入超过 [`LONG_CONTEXT_THRESHOLD`] 时**整轮**改按这一档计。
    ///
    /// `None` 表示官方价目表里这一行的长上下文几列是空的——不是「没查到」，是这个模型没有
    /// 这一档（codex 全系、mini/nano、o 系列、4.1 都没有）。
    pub long: Option<Rate>,
}

/// 长上下文加价的触发线：**本轮输入 token 超过这个数**（严格大于）就整轮换档。
///
/// 官方原话是「Prompts with >272K input tokens are priced at 2x input and 1.5x output for the
/// full request」——**是整轮重新计价，不是只给超出的那部分加价**。273K 的一轮全按贵档算，
/// 而不是 272K 按标准价 + 1K 按加价。
pub const LONG_CONTEXT_THRESHOLD: i64 = 272_000;

/// 只有标准档的模型：官方价目表里长上下文那几列是空的。
const fn flat(input: f64, cached_input: f64, output: f64) -> Price {
    Price { standard: Rate { input, cached_input, output }, long: None }
}

/// 带长上下文档的模型。
///
/// 后三个参数照抄**官方直接列出的长上下文价**，不从标准档乘出来。倍率（2x 输入、2x 缓存
/// 输入、1.5x 输出）眼下对每一档都成立，但价目表给的是绝对值；照抄才不会在官方哪天单独调
/// 某一档时静默跟错。倍率关系由测试守着，破了会响。
const fn tiered(
    input: f64,
    cached_input: f64,
    output: f64,
    long_input: f64,
    long_cached_input: f64,
    long_output: f64,
) -> Price {
    Price {
        standard: Rate { input, cached_input, output },
        long: Some(Rate {
            input: long_input,
            cached_input: long_cached_input,
            output: long_output,
        }),
    }
}

/// 价目表。键按**前缀**匹配，长者优先（见 [`price_of`]）。
///
/// 取 OpenAI 公布的 API 价格（`developers.openai.com/api/docs/pricing`，2026-08-24 核对）。
/// 新模型没进表时按 `None` 处理——宁可缺一条估算，也不要拿一个猜的价格去乘出一个看着精确
/// 的错数。
///
/// **一处刻意不建模**，故这里的数在加速档下仍是下限：**fast 档**（gpt-5.3-codex 之类有一个
/// 2x 的加速档）要额外知道「这次走的是哪一档」，而响应里没有这个信息。长上下文档不同——它
/// 的判据就是本轮输入 token 数，那个数我们手上有，所以按 [`LONG_CONTEXT_THRESHOLD`] 建模了。
const PRICES: &[(&str, Price)] = &[
    // ---- gpt-5.6 三档 ----
    // **没有裸的 `gpt-5.6`**：官方价目表里不存在这个模型名（实测上游也拒这个名字），
    // 写一条进来只会让 sol/terra/luna 之外的 `gpt-5.6-*` 撞上一个凭空的价格。
    //
    // sol 在 2026-08-22 降过价（$5/$30 → $4/$20），官方称这是促销价、**至少持续到
    // 2026-11-21**。促销一收就得回来核这一行，否则会反过来低估。
    ("gpt-5.6-sol", tiered(4.0, 0.4, 20.0, 8.0, 0.8, 30.0)),
    ("gpt-5.6-terra", tiered(2.0, 0.2, 12.0, 4.0, 0.4, 18.0)),
    ("gpt-5.6-luna", tiered(0.2, 0.02, 1.2, 0.4, 0.04, 1.8)),
    // ---- gpt-5.5 ----
    ("gpt-5.5-pro", tiered(30.0, 30.0, 180.0, 60.0, 60.0, 270.0)),
    ("gpt-5.5", tiered(5.0, 0.5, 30.0, 10.0, 1.0, 45.0)),
    // ---- gpt-5.4 ----
    ("gpt-5.4-pro", tiered(30.0, 30.0, 180.0, 60.0, 60.0, 270.0)),
    ("gpt-5.4-mini", flat(0.75, 0.075, 4.5)),
    ("gpt-5.4-nano", flat(0.2, 0.02, 1.25)),
    ("gpt-5.4", tiered(2.5, 0.25, 15.0, 5.0, 0.5, 22.5)),
    // ---- gpt-5.3 / 5.2 ----
    // codex 全系没有长上下文档（官方价目表那几列是空的），别顺手给它们补一个。
    ("gpt-5.3-codex", flat(1.75, 0.175, 14.0)),
    ("gpt-5.2-pro", flat(21.0, 21.0, 168.0)),
    ("gpt-5.2-codex", flat(1.75, 0.175, 14.0)),
    ("gpt-5.2", flat(1.75, 0.175, 14.0)),
    // ---- gpt-5.1 / gpt-5 ----
    // `gpt-5.1-codex-max` 不单列：它与 `gpt-5.1-codex` 同价，前缀匹配已经覆盖到。
    ("gpt-5.1-codex-mini", flat(0.25, 0.025, 2.0)),
    ("gpt-5.1-codex", flat(1.25, 0.125, 10.0)),
    ("gpt-5.1", flat(1.25, 0.125, 10.0)),
    ("gpt-5-pro", flat(15.0, 15.0, 120.0)),
    ("gpt-5-codex", flat(1.25, 0.125, 10.0)),
    ("gpt-5-mini", flat(0.25, 0.025, 2.0)),
    ("gpt-5-nano", flat(0.05, 0.005, 0.4)),
    ("gpt-5", flat(1.25, 0.125, 10.0)),
    // ---- 兜底：o 系列与 4.1，偶尔还会被指定到 ----
    ("o4-mini", flat(1.1, 0.275, 4.4)),
    // `o3-mini` 得单列：边界规则允许 `-` 起头的后缀，故不列的话它会被 `o3` 按两倍价吃掉。
    ("o3-mini", flat(1.1, 0.55, 4.4)),
    ("o3", flat(2.0, 0.5, 8.0)),
    ("gpt-4.1-nano", flat(0.1, 0.025, 0.4)),
    ("gpt-4.1-mini", flat(0.4, 0.1, 1.6)),
    ("gpt-4.1", flat(2.0, 0.5, 8.0)),
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

/// 这一轮实际适用的那一档价。
///
/// 输入超过 [`LONG_CONTEXT_THRESHOLD`] 且这个模型有长上下文档时换贵档，否则标准档。
/// **没有长上下文档的模型即便输入再长也不换档**——给它们补一个 2x 是凭空加价。
pub fn rate_of(model: &str, input_tokens: i64) -> Option<Rate> {
    let p = price_of(model)?;
    match p.long {
        Some(long) if input_tokens > LONG_CONTEXT_THRESHOLD => Some(long),
        _ => Some(p.standard),
    }
}

/// 估算一次请求的等价费用（美元）。模型未知时返回 `None`。
///
/// `input_tokens` 传上游报的那个原值——**它已经把 cached 部分算在内**，故这里先扣掉
/// cached 再按未命中价计。不扣的话缓存命中率高的会话会被高估好几倍。
///
/// 换档的判据也是这个原值（整个 prompt 的大小，含命中缓存的部分），且**整轮同档**：
/// 一次 300K 输入的调用是三十万 token 全按贵档，不是只有超出 272K 的那 2.8 万。
pub fn estimate_usd(
    model: &str,
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
) -> Option<f64> {
    let r = rate_of(model, input_tokens)?;
    let uncached = (input_tokens - cached_tokens).max(0) as f64;
    let cached = cached_tokens.max(0) as f64;
    let out = output_tokens.max(0) as f64;
    Some((uncached * r.input + cached * r.cached_input + out * r.output) / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 短上下文那一档（绝大多数请求走的那档）。
    fn std_of(model: &str) -> Rate {
        price_of(model).unwrap_or_else(|| panic!("{model} must be priced")).standard
    }

    /// 表序不能决定结果：`gpt-5` 是 `gpt-5-mini` 的前缀，先到先得会把 mini 按 5 倍价算。
    #[test]
    fn longest_prefix_wins_regardless_of_table_order() {
        assert_eq!(std_of("gpt-5-mini-2026-01-01").input, 0.25);
        assert_eq!(std_of("gpt-5-nano").input, 0.05);
        assert_eq!(std_of("gpt-5.1-codex-mini").input, 0.25);
        assert_eq!(std_of("gpt-5.1-codex").input, 1.25);
    }

    /// 官方价目表（2026-08-24 核对）逐项钉住。
    ///
    /// 钉的是**实测上游真会返回的那些名字**：这张表以前把 5.6-sol 按 gpt-5 的价算（少算
    /// 到四分之一）、5.2 与 5.2-codex 也各少算两成，而这类错误没有任何症状——花费栏照样
    /// 显示一个看着精确的数。
    #[test]
    fn official_rates_are_pinned() {
        for (model, input, cached, output) in [
            // sol 2026-08-22 降价到 $4/$20（促销，官方称至少到 2026-11-21）。
            ("gpt-5.6-sol", 4.0, 0.4, 20.0),
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
            let r = std_of(model);
            assert_eq!((r.input, r.cached_input, r.output), (input, cached, output), "{model}");
        }
        // 上游把别名解析成带日期的具体版本（实测 `gpt-5.4-mini` → `…-2026-03-17`），
        // 那个名字也得落在同一档上，否则真实流水里反而是没价的那一份。
        assert_eq!(std_of("gpt-5.4-mini-2026-03-17").input, 0.75);
    }

    /// 长上下文档也逐项钉住（同一张官方表的后半截几列）。
    #[test]
    fn official_long_context_rates_are_pinned() {
        for (model, input, cached, output) in [
            ("gpt-5.6-sol", 8.0, 0.8, 30.0),
            ("gpt-5.6-terra", 4.0, 0.4, 18.0),
            ("gpt-5.6-luna", 0.4, 0.04, 1.8),
            ("gpt-5.5", 10.0, 1.0, 45.0),
            ("gpt-5.5-pro", 60.0, 60.0, 270.0),
            ("gpt-5.4", 5.0, 0.5, 22.5),
            ("gpt-5.4-pro", 60.0, 60.0, 270.0),
        ] {
            let long = price_of(model).unwrap().long.unwrap_or_else(|| panic!("{model} 该有长档"));
            assert_eq!(
                (long.input, long.cached_input, long.output),
                (input, cached, output),
                "{model}"
            );
        }
    }

    /// 长档与标准档的倍率关系：输入与缓存输入 2x、输出 1.5x。
    ///
    /// 表里填的是官方列出的绝对值（官方哪天单独调一档，照抄才不会跟错），这条守的是**抄的时候
    /// 手滑**——少一个 0 或错一位小数，在别处没有任何症状。
    #[test]
    fn long_context_is_2x_input_and_1_5x_output() {
        for (_, p) in PRICES {
            let Some(long) = p.long else { continue };
            let s = p.standard;
            assert!((long.input - s.input * 2.0).abs() < 1e-9, "{s:?} -> {long:?}");
            assert!((long.cached_input - s.cached_input * 2.0).abs() < 1e-9, "{s:?} -> {long:?}");
            assert!((long.output - s.output * 1.5).abs() < 1e-9, "{s:?} -> {long:?}");
        }
    }

    /// 换档卡在 272K 上，且**整轮**换档——不是只给超出的那一截加价。
    #[test]
    fn long_context_reprices_the_whole_request() {
        // 正好 272K 还是标准档（官方是「>272K」，严格大于）。
        assert_eq!(rate_of("gpt-5.5", LONG_CONTEXT_THRESHOLD).unwrap().input, 5.0);
        assert_eq!(rate_of("gpt-5.5", LONG_CONTEXT_THRESHOLD + 1).unwrap().input, 10.0);

        // 300K 输入 + 10K 输出：三十万 token 全按 $10 算，而不是 272K 按 $5 + 2.8K 按 $10。
        let usd = estimate_usd("gpt-5.5", 300_000, 0, 10_000).unwrap();
        let whole = (300_000.0 * 10.0 + 10_000.0 * 45.0) / 1e6;
        let overflow_only = (272_000.0 * 5.0 + 28_000.0 * 10.0 + 10_000.0 * 45.0) / 1e6;
        assert!((usd - whole).abs() < 1e-9, "{usd} vs {whole}");
        assert!((usd - overflow_only).abs() > 1e-6, "只给超出部分加价是错的");

        // 命中缓存的那部分同样翻倍（官方长档的 cached 列就是标准档的 2x）。
        let cached = estimate_usd("gpt-5.6-sol", 400_000, 400_000, 0).unwrap();
        assert!((cached - 400_000.0 * 0.8 / 1e6).abs() < 1e-9, "{cached}");
    }

    /// **没有长档的模型不许被顺手加价**：codex 全系、mini/nano、o 系列、4.1 官方都只有一档，
    /// 给它们乘个 2x 就是凭空多算一倍。
    #[test]
    fn models_without_a_long_tier_never_change_price() {
        for model in [
            "gpt-5.3-codex",
            "gpt-5.2-codex",
            "gpt-5.1-codex-max",
            "gpt-5-codex",
            "gpt-5.4-mini",
            "gpt-5.4-nano",
            "gpt-5.2-pro",
            "gpt-5-pro",
            "gpt-5",
            "o3",
            "gpt-4.1",
        ] {
            assert!(price_of(model).unwrap().long.is_none(), "{model} 官方没有长上下文档");
            let short = estimate_usd(model, 1_000, 0, 1_000).unwrap();
            let long = estimate_usd(model, 1_000_000, 0, 1_000).unwrap();
            let rate = std_of(model);
            assert!((long - (1_000_000.0 * rate.input + 1_000.0 * rate.output) / 1e6).abs() < 1e-9);
            assert!(short > 0.0);
        }
    }

    /// 没有裸的 `gpt-5.6`：terra/luna 各有各的价，落到一条凭空的 `gpt-5.6` 上就全错了。
    /// 同理 `gpt-5.4-mini` 不能被 `gpt-5.4` 抢走（贵 3 倍多）。
    #[test]
    fn sibling_variants_do_not_borrow_each_others_price() {
        assert_eq!(std_of("gpt-5.6-terra").input, 2.0);
        assert_eq!(std_of("gpt-5.6-luna").input, 0.2);
        assert_ne!(std_of("gpt-5.4-mini").input, std_of("gpt-5.4").input);
        assert_ne!(std_of("gpt-5.5-pro").input, std_of("gpt-5.5").input);
        assert_ne!(std_of("gpt-4.1-nano").input, std_of("gpt-4.1").input);
    }

    /// pro 档官方不给缓存价（它们不提供提示缓存）。这一列填 0 会让一次昂贵调用被算成
    /// 几乎免费，故按未命中价计——宁可不打折，也不要凭空打折。**长档同理**。
    #[test]
    fn pro_tiers_never_get_a_phantom_cache_discount() {
        for model in ["gpt-5.5-pro", "gpt-5.4-pro", "gpt-5.2-pro", "gpt-5-pro"] {
            let p = price_of(model).unwrap_or_else(|| panic!("{model} must be priced"));
            assert_eq!(p.standard.cached_input, p.standard.input, "{model}");
            if let Some(long) = p.long {
                assert_eq!(long.cached_input, long.input, "{model} 长档");
            }
        }
        // 全命中缓存也得按满价：1M token 的 pro 调用不可能只值几分钱。
        // （1M 已经过了 272K，走的是长档的 $60。）
        assert!(estimate_usd("gpt-5.5-pro", 1_000_000, 1_000_000, 0).unwrap() >= 60.0);
    }

    #[test]
    fn unknown_model_has_no_price() {
        assert!(price_of("llama-3").is_none());
        assert!(rate_of("llama-3", 1_000_000).is_none());
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
            ("gpt-5.6-sol-2026-05-01", 4.0),
            ("o4-mini-2025-04-16", 1.1),
        ] {
            assert_eq!(std_of(model).input, expect_input, "{model}");
        }
        // 带后缀的名字也得继承长档，否则真实流水里恰恰是没加价的那一份。
        assert!(price_of("gpt-5.6-sol-2026-05-01").unwrap().long.is_some());
        // `o3-mini` 单列了一档，不能被 `o3` 按两倍价吃掉。
        assert_eq!(std_of("o3-mini").input, 1.1);
        assert_eq!(std_of("o3").input, 2.0);
    }

    /// cached 部分要从 input 里扣掉再按缓存价计，否则命中率高的会话被高估。
    #[test]
    fn cached_tokens_are_not_double_charged() {
        // 1M input（其中 800k 命中缓存）+ 0 output。codex 没有长档，1M 也走标准价。
        let usd = estimate_usd("gpt-5.1-codex", 1_000_000, 800_000, 0).unwrap();
        let expect = 200_000.0 * 1.25 / 1e6 + 800_000.0 * 0.125 / 1e6;
        assert!((usd - expect).abs() < 1e-9, "{usd} vs {expect}");
        // 全部命中缓存时不该出现负数的未命中部分
        assert!(estimate_usd("gpt-5.1-codex", 100, 500, 0).unwrap() > 0.0);
    }
}
