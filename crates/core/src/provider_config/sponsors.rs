//! 赞助商中转线路目录。
//!
//! 这里维护 OcHub 赞助商提供的 API 中转端点，供供应商编辑器渲染成一键预设卡片。
//! 它与 `db/dao/providers_seed.rs` 的官方种子是两回事：官方种子会写进 `providers`
//! 表成为用户的一条连接，而这里只是**填表模板**——用户点一下把 Base URL 填进去，
//! API Key 仍需自行向对方购买，不点就完全不产生任何副作用。
//!
//! `SponsorId` 与 `RouteKind` 刻意用枚举而非字符串：应用层的「id → 文案 key」映射
//! 因此是穷尽匹配，将来新增一家��助商会变成**编译错误**，而不是界面上一块空白。
//!
//! Logo 不走 `Provider.icon` 那条路。那两列（`model.rs` 的 `icon` / `icon_color`）
//! 虽然会被写入，但全仓没有任何地方渲染它们，而且它们最终指向 `icons.rs` 的单色
//! `svg()` 通道——彩色第三方品牌标过不了那条路。所以这里直接存资源路径，由
//! `provider_editor` 用 `img()` 加载。

use serde_json::Value;

use super::{FormValues, Preset, dialect_base_url, set_bool, set_str};
use crate::AppType;
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
    /// scheme + host，**不带路径、不带尾部斜杠**。是否追加 `/v1` 由
    /// [`dialect_base_url`] 按目标应用决定。
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
    /// 会为其生成一键预设的应用。
    pub apps: &'static [AppType],
    /// 该中转已提供的接口方言，用于模型供应商编辑器预勾选「支持的接口」。
    /// 勾错了用户会拿到 404，所以这里只写目录维护者确认过的，其余留给「检测」按钮。
    pub dialects: &'static [Dialect],
}

/// 两家中转都同时提供 Anthropic Messages 与 OpenAI 兼容（含 Responses）接口。
///
/// 其余应用暂不纳入：它们各自要求一个必填的模型 id 或适配器选择，在不掌握对方模型
/// 目录的前提下填不出诚实的默认值，而预填一个会 404 的模型比不给预设更糟。
const SPONSORED_APPS: &[AppType] = &[AppType::Claude, AppType::ClaudeDesktop, AppType::Codex];

/// 同上：两家都是 Messages + OpenAI 兼容（Chat 与 Responses）全通。
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
        apps: SPONSORED_APPS,
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
        apps: SPONSORED_APPS,
        dialects: SPONSORED_DIALECTS,
    },
];

/// 按 id 查一家赞助商。
pub fn sponsor_by_id(id: SponsorId) -> &'static Sponsor {
    SPONSORS
        .iter()
        .find(|sponsor| sponsor.id == id)
        .expect("SPONSORS 覆盖全部 SponsorId")
}

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

/// 某赞助商在 `app` 下、走第 `route` 条线路的一键预设。
///
/// 值以 `codec.decode(&Value::Null, None)` 的默认值起底，只覆盖端点相关字段。
/// 这一点很关键：编辑器的 `apply_preset` 是**整体替换** `values`，而 Claude 的模型
/// 角色网格活在 `values["roles"]` 里——手写一份字段清单会把它抹成空网格。
pub fn preset_for(sponsor: &'static Sponsor, app: AppType, route: usize) -> Option<Preset> {
    if !sponsor.apps.contains(&app) {
        return None;
    }
    let origin = sponsor.routes.get(route)?.origin;
    let codec = super::config_for(app)?;
    let mut values = codec.decode(&Value::Null, None);
    apply_endpoint(&mut values, app, sponsor, origin);
    Some(Preset {
        name: sponsor.brand.to_string(),
        values,
        sponsor: Some(sponsor),
        route,
    })
}

/// `app` 支持的全部赞助商预设，默认取每家的第一条线路。
pub fn presets_for(app: AppType) -> Vec<Preset> {
    SPONSORS
        .iter()
        .filter_map(|sponsor| preset_for(sponsor, app, 0))
        .collect()
}

/// 把线路 origin 写进 `values` 的端点相关字段，其余保持 codec 的 decode 默认值。
///
/// 中转站一律走 Bearer（两家实测都以 `Authorization` 判定），所以显式指定 auth 字段
/// ——注意两个 Claude 系 codec 的 `auth_field` 取值词表不同，各用各自模块的常量。
fn apply_endpoint(values: &mut FormValues, app: AppType, sponsor: &Sponsor, origin: &str) {
    let base = dialect_base_url(app, origin);
    set_str(values, "base_url", base);
    set_str(values, "api_key", "");
    match app {
        AppType::Claude => {
            set_str(values, "auth_field", super::claude::AUTH_TOKEN_KEY);
            set_str(values, "api_format", "anthropic");
            set_str(values, "custom_user_agent", "");
            set_bool(values, "is_full_url", false);
        }
        AppType::ClaudeDesktop => {
            set_str(values, "auth_field", super::claude_desktop::AUTH_TOKEN);
        }
        AppType::Codex => {
            set_str(values, "provider_id", sponsor.id.as_str());
            set_str(values, "name", sponsor.brand);
            set_str(values, "auth_mode", super::codex::AUTH_API_KEY);
            set_str(values, "wire_api", "responses");
            set_bool(values, "remote_compaction", false);
            // 中转站不实现 Responses 的服务端存储。
            set_bool(values, "disable_response_storage", true);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_config::{FormSection, Severity, str_val};

    /// schema 里全部字段的 id。
    fn field_ids(sections: &[FormSection]) -> Vec<String> {
        sections
            .iter()
            .flat_map(|section| section.fields.iter().map(|field| field.id.clone()))
            .collect()
    }

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
                // dialect_base_url 直接拼 `/v1`，字面量带尾斜杠会拼出 `//v1`。
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
            // 同种线路出现两次会让分段控件出现两个同名段。
            assert_eq!(kinds.len(), count, "{} 的线路种类重复", sponsor.brand);
            assert!(!sponsor.routes.is_empty(), "{} 没有线路", sponsor.brand);
            assert!(!sponsor.apps.is_empty(), "{} 没有支持的应用", sponsor.brand);
        }
    }

    #[test]
    fn dialect_matches_per_app() {
        let krill = sponsor_by_id(SponsorId::Krill);

        let claude = preset_for(krill, AppType::Claude, 0).expect("claude 预设");
        assert_eq!(
            str_val(&claude.values, "base_url"),
            "https://api.krill-ai.net"
        );

        let desktop = preset_for(krill, AppType::ClaudeDesktop, 0).expect("claude desktop 预设");
        assert_eq!(
            str_val(&desktop.values, "base_url"),
            "https://api.krill-ai.net"
        );

        let codex = preset_for(krill, AppType::Codex, 0).expect("codex 预设");
        assert_eq!(
            str_val(&codex.values, "base_url"),
            "https://api.krill-ai.net/v1"
        );

        // 第二条线路（CDN）也走同一套方言规则。
        let cdn = preset_for(krill, AppType::Codex, 1).expect("codex cdn 预设");
        assert_eq!(
            str_val(&cdn.values, "base_url"),
            "https://api.cdn-krill-ai.com/v1"
        );
    }

    /// 编辑器整体替换 `values`，缺字段就渲染成空控件——Claude 的 `roles` 网格是
    /// 最容易被这样抹掉的一个。
    #[test]
    fn presets_fill_every_schema_field() {
        for &app in SPONSORED_APPS {
            let codec = super::super::config_for(app).expect("codec");
            let expected = field_ids(&codec.schema());
            for sponsor in SPONSORS {
                let preset = preset_for(sponsor, app, 0).expect("预设");
                for id in &expected {
                    assert!(
                        preset.values.contains_key(id),
                        "{app:?} 的 {} 预设缺少字段 {id}",
                        sponsor.brand
                    );
                }
            }
        }
    }

    #[test]
    fn presets_validate_except_the_api_key() {
        for &app in SPONSORED_APPS {
            let codec = super::super::config_for(app).expect("codec");
            for sponsor in SPONSORS {
                let preset = preset_for(sponsor, app, 0).expect("预设");
                // API Key 故意留空，由用户自行填写，所以只忽略这一个字段的错误。
                let blocking: Vec<String> = codec
                    .validate(&preset.values)
                    .into_iter()
                    .filter(|issue| issue.severity == Severity::Error)
                    .filter(|issue| issue.field.as_deref() != Some("api_key"))
                    .map(|issue| issue.message)
                    .collect();
                assert!(
                    blocking.is_empty(),
                    "{app:?} 的 {} 预设有阻塞性校验错误: {blocking:?}",
                    sponsor.brand
                );
            }
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

    #[test]
    fn route_index_out_of_range_is_none() {
        let hezubus = sponsor_by_id(SponsorId::HezuBus);
        assert!(preset_for(hezubus, AppType::Claude, 0).is_some());
        assert!(preset_for(hezubus, AppType::Claude, 1).is_none());
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

    #[test]
    fn unsupported_app_yields_no_presets() {
        assert!(presets_for(AppType::Hermes).is_empty());
        assert_eq!(presets_for(AppType::Claude).len(), SPONSORS.len());
    }
}
