//! 赞助商中转线路目录。
//!
//! 这里维护 OcHub 赞助商提供的 API 中转端点，供**模型供应商**编辑器渲染成一键填表
//! 的品牌胶囊。它与 `db/dao/providers_seed.rs` 的官方种子是两回事：官方种子会写进
//! `providers` 表成为用户的一条连接，而这里只是**填表模板**——用户点一下把 Base URL
//! 填进去，API Key 仍需自行向对方购买，不点就完全不产生任何副作用。
//!
//! `SponsorId` 与 `RouteKind` 刻意用枚举而非字符串：应用层的「id → 文案 key」映射
//! 因此是穷尽匹配，将来新增一家赞助商会变成**编译错误**，而不是界面上一块空白。
//!
//! Logo 不走 `Provider.icon` 那条路。那两列（`model.rs` 的 `icon` / `icon_color`）
//! 虽然会被写入，但全仓没有任何地方渲染它们，而且它们最终指向 `icons.rs` 的单色
//! `svg()` 通道——彩色第三方品牌标过不了那条路。所以这里直接存资源路径，由
//! `gateway_view` 用 `img()` 加载。

use crate::gateway::types::Dialect;

/// 赞助商标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SponsorId {
    Krill,
    HezuBus,
}

impl SponsorId {
    /// 稳定的字符串形式，用于元素 id 与状态键（对齐 `AppType::as_str` 的写法）。
    pub fn as_str(self) -> &'static str {
        match self {
            SponsorId::Krill => "krill",
            SponsorId::HezuBus => "hezubus",
        }
    }
}

/// 线路种类。同一家赞助商的多条接入地址靠它区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    /// 主线路。
    Primary,
    /// CDN 加速线路。
    Cdn,
    /// 负载均衡线路。
    LoadBalanced,
}

/// 赞助商的一条 API 接入线路。
#[derive(Debug, Clone, Copy)]
pub struct SponsorRoute {
    pub kind: RouteKind,
    /// scheme + host，**不带路径、不带尾部斜杠**：模型供应商的端点行直接填这个值，
    /// 具体方言的路径由网关按接口自行拼接。
    pub origin: &'static str,
}

/// 一家赞助商。
#[derive(Debug, Clone, Copy)]
pub struct Sponsor {
    pub id: SponsorId,
    /// 品牌名，不翻译（与 codex/grokbuild 现有预设名一样是硬编码字面量）。
    pub brand: &'static str,
    pub website: &'static str,
    /// 相对 `crates/app/assets/` 的资源路径，供 GPUI `img()` 加载。
    pub logo: &'static str,
    /// 全部接入线路。对模型供应商（网关）而言这几条是**同一个账号下的等价端点**，
    /// 正好对应编辑器里的故障转移地址列表。
    pub routes: &'static [SponsorRoute],
    /// 该中转已提供的接口方言，用于模型供应商编辑器预勾选「支持的接口」。
    /// 勾错了用户会拿到 404，所以这里只写目录维护者确认过的，其余留给「检测」按钮。
    pub dialects: &'static [Dialect],
}

/// 两家中转都同时提供 Anthropic Messages 与 OpenAI 兼容（Chat 与 Responses）接口。
const SPONSORED_DIALECTS: &[Dialect] = &[Dialect::Messages, Dialect::Responses, Dialect::Chat];

/// 赞助商目录。
pub const SPONSORS: &[Sponsor] = &[
    Sponsor {
        id: SponsorId::Krill,
        brand: "Krill",
        website: "https://www.krill-ai.net",
        logo: "sponsors/krill.png",
        routes: &[
            SponsorRoute {
                kind: RouteKind::Primary,
                origin: "https://api.krill-ai.net",
            },
            SponsorRoute {
                kind: RouteKind::Cdn,
                origin: "https://api.cdn-krill-ai.com",
            },
            SponsorRoute {
                kind: RouteKind::LoadBalanced,
                origin: "https://api-slb.krill-ai.net",
            },
        ],
        dialects: SPONSORED_DIALECTS,
    },
    Sponsor {
        id: SponsorId::HezuBus,
        brand: "合租巴士",
        // API 域名与官网同域，没有单独的 api 子域。
        website: "https://hezubus.cc",
        logo: "sponsors/hezubus.png",
        routes: &[SponsorRoute {
            kind: RouteKind::Primary,
            origin: "https://hezubus.cc",
        }],
        dialects: SPONSORED_DIALECTS,
    },
];

/// `url` 的 host，小写。缺 scheme 时按 https 补齐——地址栏允许用户只填域名。
fn host_of(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    url::Url::parse(url)
        .or_else(|_| url::Url::parse(&format!("https://{url}")))
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

/// 反查一个 base_url 属于哪家赞助商的哪条线路。
///
/// 只比 host，不比路径：保存下来的地址可能带尾斜杠或 `/v1`，也可能是用户在本功能
/// 出现之前手填的——按 host 认，那些既有连接同样能认回来并显示品牌 Logo。
pub fn route_of_url(url: &str) -> Option<(&'static Sponsor, usize)> {
    let host = host_of(url)?;
    SPONSORS.iter().find_map(|sponsor| {
        sponsor
            .routes
            .iter()
            .position(|route| host_of(route.origin).as_deref() == Some(host.as_str()))
            .map(|index| (sponsor, index))
    })
}

/// [`route_of_url`] 的简化版：只关心是哪一家。
pub fn sponsor_for_url(url: &str) -> Option<&'static Sponsor> {
    route_of_url(url).map(|(sponsor, _)| sponsor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_origins_are_bare_https_origins() {
        for sponsor in SPONSORS {
            for route in sponsor.routes {
                let url = url::Url::parse(route.origin)
                    .unwrap_or_else(|err| panic!("{} 不是合法 URL: {err}", route.origin));
                assert_eq!(url.scheme(), "https", "{} 必须是 https", route.origin);
                assert_eq!(url.path(), "/", "{} 不能带路径", route.origin);
                assert!(url.query().is_none(), "{} 不能带 query", route.origin);
                assert!(url.fragment().is_none(), "{} 不能带 fragment", route.origin);
                // 网关在这个 origin 后面直接拼方言路径，尾斜杠会拼出 `//v1`。
                assert!(
                    !route.origin.ends_with('/'),
                    "{} 不能以斜杠结尾",
                    route.origin
                );
            }
        }
    }

    #[test]
    fn ids_and_route_kinds_are_unique() {
        let mut ids: Vec<&str> = SPONSORS.iter().map(|s| s.id.as_str()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "赞助商 id 重复");

        for sponsor in SPONSORS {
            let mut kinds: Vec<RouteKind> = sponsor.routes.iter().map(|r| r.kind).collect();
            let count = kinds.len();
            kinds.dedup_by(|a, b| a == b);
            // 同种线路出现两次，界面上就是两条分不出差别的端点。
            assert_eq!(kinds.len(), count, "{} 的线路种类重复", sponsor.brand);
            assert!(!sponsor.routes.is_empty(), "{} 没有线路", sponsor.brand);
        }
    }

    /// `img()` 找不到资源时是静默渲染成空白，没有更廉价的守卫，所以在这里跨 crate
    /// 检查一次文件是否真的存在。
    #[test]
    fn logo_assets_exist() {
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../app/assets");
        for sponsor in SPONSORS {
            let path = assets.join(sponsor.logo);
            assert!(path.is_file(), "缺少 Logo 资源: {}", path.display());
        }
    }

    /// 列表卡片靠这个反查决定画品牌 Logo 还是通用图标，所以既有连接常见的几种写法
    /// （尾斜杠、`/v1` 后缀、大写 host、无 scheme）都必须认得出来。
    #[test]
    fn route_of_url_matches_on_host_only() {
        for (url, expected) in [
            ("https://api-slb.krill-ai.net", 2),
            ("https://api-slb.krill-ai.net/", 2),
            ("https://api.krill-ai.net/v1", 0),
            ("https://API.CDN-Krill-AI.com", 1),
            ("api.krill-ai.net", 0),
        ] {
            let (sponsor, route) = route_of_url(url).unwrap_or_else(|| panic!("{url} 应命中"));
            assert_eq!(sponsor.id, SponsorId::Krill, "{url}");
            assert_eq!(route, expected, "{url}");
        }
        assert_eq!(
            sponsor_for_url("https://hezubus.cc/openai").map(|s| s.id),
            Some(SponsorId::HezuBus)
        );
    }

    /// 官网域名不是接口域名：Krill 的官网是 `www.` 子域，认成线路会让一条指向官网的
    /// 连接冒充赞助商线路。
    #[test]
    fn route_of_url_ignores_unrelated_hosts() {
        assert!(route_of_url("https://api.openai.com").is_none());
        assert!(route_of_url("").is_none());
        assert!(route_of_url("http://127.0.0.1:4180").is_none());
        assert!(route_of_url("https://www.krill-ai.net").is_none());
    }
}
