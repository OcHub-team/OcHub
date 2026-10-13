//! Display text for built-in provider schemas. Protocol values and user input
//! never enter this catalog. Every key must supply all supported languages.

use crate::i18n::{self, Locale};
use std::fmt::Display;

macro_rules! catalog {
    ($($key:ident => ($zh:literal, $en:literal, $ja:literal),)+) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(super) enum TextKey { $($key,)+ }

        #[cfg(test)]
        const ALL: &[TextKey] = &[$(TextKey::$key,)+];

        fn lookup(key: TextKey, locale: Locale) -> &'static str {
            match (key, locale) {
                $((TextKey::$key, Locale::Zh) => $zh,
                  (TextKey::$key, Locale::En) => $en,
                  (TextKey::$key, Locale::Ja) => $ja,)+
            }
        }
    };
}

catalog! {
    CherryImportSection => ("Cherry Studio 导入参数", "Cherry Studio import settings", "Cherry Studio のインポート設定"),
    ProtocolType => ("协议类型", "Protocol type", "プロトコルの種類"),
    CherryProtocolHelp => ("决定 Cherry Studio 为此连接使用的请求协议。", "Protocol Cherry Studio uses for this connection.", "この接続で Cherry Studio が使用するプロトコルです。"),
    CherryBaseUrlHelp => ("点击导入时作为 baseUrl 传给 Cherry Studio。", "Passed to Cherry Studio as baseUrl when importing.", "インポート時に baseUrl として Cherry Studio に渡されます。"),
    CherryApiKeyHelp => ("API Key 会编码进本机 Deep Link，并由 Cherry Studio 显示确认。", "The API key is encoded in a local deep link. Cherry Studio asks for confirmation.", "API キーはローカルのディープリンクに含まれ、Cherry Studio で確認が表示されます。"),
    CherryPreviewName => ("cherrystudio://providers/api-keys（导入参数）", "cherrystudio://providers/api-keys (import settings)", "cherrystudio://providers/api-keys（インポート設定）"),
    CherryProtocolRequired => ("请选择 Cherry Studio 支持的协议类型。", "Select a protocol supported by Cherry Studio.", "Cherry Studio が対応するプロトコルを選択してください。"),
    BaseUrlRequired => ("Base URL 不能为空。", "Base URL is required.", "Base URL を入力してください。"),
    BaseUrlInvalid => ("Base URL 必须是有效的 http:// 或 https:// 地址。", "Base URL must be a valid http:// or https:// URL.", "Base URL には有効な http:// または https:// の URL を入力してください。"),
    ApiKeyRequired => ("API Key 不能为空。", "API Key is required.", "API Key を入力してください。"),
    EndpointAuthSection => ("端点与鉴权", "Endpoint and authentication", "エンドポイントと認証"),
    ClaudeBaseUrlHelp => ("写入 env.ANTHROPIC_BASE_URL；Claude Code 会在其后拼接 /v1/messages。", "Written to env.ANTHROPIC_BASE_URL; Claude Code appends /v1/messages.", "env.ANTHROPIC_BASE_URL に書き込まれ、Claude Code が /v1/messages を追加します。"),
    AuthVariable => ("鉴权变量", "Authentication variable", "認証変数"),
    AuthBearerHint => ("第三方服务默认；走 Authorization: Bearer", "Default for third-party services; uses Authorization: Bearer", "サードパーティサービスの既定値。Authorization: Bearer を使用します。"),
    AuthApiKeyHint => ("Anthropic 官方密钥；走 x-api-key", "Official Anthropic key; uses x-api-key", "Anthropic 公式キー用。x-api-key を使用します。"),
    ClaudeAuthHelp => ("决定密钥写入哪个 env 变量；二者互斥，所选项同时记入 meta.apiKeyField。", "Select the env variable that stores the key. The variables are mutually exclusive; the choice is also stored in meta.apiKeyField.", "キーを書き込む env 変数を選択します。両方は使用できません。選択内容は meta.apiKeyField にも保存されます。"),
    ClaudeApiKeyHelp => ("写入上方所选的鉴权变量。", "Written to the authentication variable selected above.", "上で選択した認証変数に書き込まれます。"),
    ModelsSection => ("模型", "Models", "モデル"),
    ModelLabel => ("模型", "Model", "モデル"),
    ClaudeDefaultModel => ("默认模型 (ANTHROPIC_MODEL)", "Default model (ANTHROPIC_MODEL)", "既定のモデル (ANTHROPIC_MODEL)"),
    ClaudeDefaultModelHelp => ("各角色未单独配置时的回退模型。", "Fallback when a role has no model of its own.", "ロールに個別のモデルがない場合に使用します。"),
    RoleModelMapping => ("角色模型映射", "Role model mapping", "ロールとモデルの対応"),
    RoleColumn => ("角色", "Role", "ロール"),
    ModelIdColumn => ("模型 ID", "Model ID", "モデル ID"),
    DisplayName => ("显示名", "Display name", "表示名"),
    OneMillionContext => ("1M 上下文", "1M context", "1M コンテキスト"),
    ClaudeRolesHelp => ("sonnet/opus/haiku/fable 对应 ANTHROPIC_DEFAULT_*_MODEL 与 *_MODEL_NAME；开启 1M 会给模型 ID 追加 [1M] 标记（haiku 不支持）。", "sonnet/opus/haiku/fable map to ANTHROPIC_DEFAULT_*_MODEL and *_MODEL_NAME. Enabling 1M adds [1M] to the model ID (not supported by haiku).", "sonnet/opus/haiku/fable は ANTHROPIC_DEFAULT_*_MODEL と *_MODEL_NAME に対応します。1M を有効にするとモデル ID に [1M] が付きます（haiku は非対応）。"),
    AdvancedSection => ("高级", "Advanced", "詳細設定"),
    ApiFormat => ("API 格式 (meta)", "API format (meta)", "API 形式 (meta)"),
    AnthropicNative => ("anthropic（原生）", "anthropic (native)", "anthropic（ネイティブ）"),
    NativeFormatHint => ("默认；不做请求体转换", "Default; no request body conversion", "既定値。リクエスト本文を変換しません。"),
    OpenAiChatUpstream => ("OpenAI Chat Completions 上游", "OpenAI Chat Completions upstream", "OpenAI Chat Completions の接続先"),
    OpenAiResponsesUpstream => ("OpenAI Responses 上游", "OpenAI Responses upstream", "OpenAI Responses の接続先"),
    GeminiNativeUpstream => ("Gemini 原生上游", "Native Gemini upstream", "Gemini ネイティブの接続先"),
    ClaudeApiFormatHelp => ("仅存于 meta.apiFormat，绝不写入 settings.json；用于网关协议转换。", "Stored only in meta.apiFormat for gateway protocol conversion; never written to settings.json.", "ゲートウェイのプロトコル変換用に meta.apiFormat にのみ保存されます。settings.json には書き込みません。"),
    CustomUserAgent => ("自定义 User-Agent (meta)", "Custom User-Agent (meta)", "カスタム User-Agent (meta)"),
    CustomUserAgentHelp => ("存于 meta.customUserAgent；留空则使用默认 UA。", "Stored in meta.customUserAgent. Leave blank to use the default user agent.", "meta.customUserAgent に保存されます。空欄の場合は既定の User-Agent を使用します。"),
    FullBaseUrl => ("Base URL 为完整地址 (meta)", "Base URL is a complete endpoint (meta)", "Base URL は完全なエンドポイント (meta)"),
    FullBaseUrlHelp => ("存于 meta.isFullUrl；上游已是完整端点、网关不再追加路径时开启。", "Stored in meta.isFullUrl. Enable when the upstream URL is complete and the gateway should not append a path.", "meta.isFullUrl に保存されます。接続先 URL が完全で、ゲートウェイでパスを追加しない場合に有効にします。"),
    SettingsJsonParseFailed => ("settings.json 解析失败: {e}", "Could not parse settings.json: {e}", "settings.json を解析できません: {e}"),
    ApiKeyMissing => ("尚未填写 API Key。", "No API Key has been entered.", "API Key が入力されていません。"),
    ClaudeAuthWarning => ("已选 ANTHROPIC_API_KEY（x-api-key）；多数第三方服务需改用 ANTHROPIC_AUTH_TOKEN（Bearer）。", "ANTHROPIC_API_KEY (x-api-key) is selected; most third-party services require ANTHROPIC_AUTH_TOKEN (Bearer).", "ANTHROPIC_API_KEY（x-api-key）が選択されています。多くのサードパーティサービスでは ANTHROPIC_AUTH_TOKEN（Bearer）が必要です。"),
    OneMillionUnsupported => ("{role} 不支持 1M 上下文，[1M] 标记将被忽略。", "{role} does not support 1M context; the [1M] marker will be ignored.", "{role} は 1M コンテキストに対応していないため、[1M] は無視されます。"),
    DesktopBaseUrlHelp => ("原生 Anthropic Messages 端点；跨格式服务请在 OcHub 模型供应商页面配置。", "Native Anthropic Messages endpoint. Configure other protocol formats on OcHub's Model Providers page.", "Anthropic Messages ネイティブのエンドポイントです。他の形式は OcHub のモデルプロバイダーページで設定してください。"),
    AuthField => ("鉴权字段", "Authentication field", "認証フィールド"),
    DesktopModels => ("Claude 模型（Sonnet / Opus / Fable / Haiku）", "Claude models (Sonnet / Opus / Fable / Haiku)", "Claude モデル（Sonnet / Opus / Fable / Haiku）"),
    DesktopModelsHelp => ("这里只接受对应角色的 Claude-safe 模型名；任意模型映射请在 OcHub 模型供应商页面配置。", "Only Claude-safe model names for the corresponding role are accepted here. Configure other mappings on OcHub's Model Providers page.", "各ロールに対応する Claude-safe なモデル名のみ指定できます。その他の対応付けは OcHub のモデルプロバイダーページで設定してください。"),
    DesktopBearerWarning => ("Claude Desktop profile 使用 Bearer；建议选择 ANTHROPIC_AUTH_TOKEN。", "Claude Desktop profiles use Bearer authentication; ANTHROPIC_AUTH_TOKEN is recommended.", "Claude Desktop のプロファイルは Bearer 認証を使用します。ANTHROPIC_AUTH_TOKEN を推奨します。"),
    DesktopMappingInvalid => ("{route_id} 不能映射到 {model}；请在 OcHub 模型供应商页面配置模型映射。", "{route_id} cannot map to {model}; configure model mappings on OcHub's Model Providers page.", "{route_id} を {model} に対応付けることはできません。OcHub のモデルプロバイダーページで設定してください。"),
    ProviderSection => ("供应商", "Provider", "プロバイダー"),
    CodexProviderIdHelp => ("config.toml 中 [model_providers.<id>] 的键，也是 model_provider 的值。", "The [model_providers.<id>] key in config.toml, also used as model_provider.", "config.toml の [model_providers.<id>] のキーで、model_provider の値にも使用されます。"),
    CodexProviderName => ("普通 Provider 名称", "Regular provider name", "通常のプロバイダー名"),
    CodexProviderNameHelp => ("这是 config.toml 中的 provider name，不是 OcHub 列表名称；开启远程压缩时会强制写为 OpenAI。", "The provider name in config.toml, separate from the OcHub list name. Remote compaction forces this value to OpenAI.", "config.toml の provider name です。OcHub の一覧名とは異なります。リモート圧縮を有効にすると OpenAI に固定されます。"),
    RemoteCompaction => ("远程压缩", "Remote compaction", "リモート圧縮"),
    RemoteCompactionHelp => ("Codex 仅在 provider name 精确为 OpenAI 时开启远程压缩。请确认上游完整支持 Responses 远程压缩。", "Codex enables remote compaction only when the provider name is exactly OpenAI. Verify that the upstream fully supports Responses remote compaction.", "Codex は provider name が正確に OpenAI の場合のみリモート圧縮を有効にします。接続先が Responses のリモート圧縮に完全対応していることを確認してください。"),
    CodexBaseUrlHelp => ("第三方 API 模式必须填写，通常以 /v1 结尾；仅账号登录时可留空使用 Codex 官方端点。", "Required for third-party API modes; usually ends in /v1. Leave blank in account-only mode to use the official Codex endpoint.", "サードパーティ API モードでは必須で、通常 /v1 で終わります。アカウントのみのモードでは空欄にすると Codex 公式エンドポイントを使用します。"),
    AuthMethod => ("鉴权方式", "Authentication method", "認証方法"),
    ThirdPartyApiOnly => ("仅第三方 API", "Third-party API only", "サードパーティ API のみ"),
    ChatGptLoginOnly => ("仅 ChatGPT 账号登录", "ChatGPT account login only", "ChatGPT アカウントログインのみ"),
    ChatGptAndApi => ("ChatGPT 登录 + 第三方 API", "ChatGPT login + third-party API", "ChatGPT ログイン + サードパーティ API"),
    CombinedAuthHint => ("auth.json 保留登录态，请求使用 experimental_bearer_token", "Keep the login in auth.json; use experimental_bearer_token for requests", "auth.json のログインを保持し、リクエストには experimental_bearer_token を使用します。"),
    CombinedAuthHelp => ("组合模式不会把第三方密钥伪装成 HTTP 头：账号仍由 auth.json 管理，模型请求的 Authorization 由 experimental_bearer_token 提供。", "Combined mode keeps the account in auth.json and supplies request authorization through experimental_bearer_token; it does not disguise the third-party key as an HTTP header.", "併用モードではアカウントを auth.json で管理し、モデルリクエストの認証に experimental_bearer_token を使用します。サードパーティキーを HTTP ヘッダーに偽装しません。"),
    ThirdPartyApiKey => ("第三方 API Key", "Third-party API Key", "サードパーティ API Key"),
    ThirdPartyApiKeyHelp => ("仅两种第三方 API 模式使用；保存后写入当前 provider 的 experimental_bearer_token。", "Used only by the two third-party API modes; saved as experimental_bearer_token for the current provider.", "サードパーティ API を使用する 2 つのモード専用です。現在のプロバイダーの experimental_bearer_token に保存されます。"),
    ReasoningEffort => ("推理强度 (model_reasoning_effort)", "Reasoning effort (model_reasoning_effort)", "推論の強度 (model_reasoning_effort)"),
    NotSet => ("（不设置）", "(Not set)", "（設定しない）"),
    ReasoningEffortHelp => ("仅 Responses API 且依模型而定。", "Responses API only; availability depends on the model.", "Responses API 専用です。利用できるかはモデルによって異なります。"),
    ResponsesOnlyHint => ("当前 Codex 仅支持 responses", "Current Codex supports only responses", "現在の Codex は responses のみ対応しています。"),
    CodexWireApiHelp => ("chat 已被 Codex 移除；chat-only 上游需经网关翻译。", "Codex removed chat; chat-only upstreams require gateway translation.", "Codex では chat が削除されました。chat のみの接続先にはゲートウェイでの変換が必要です。"),
    DisableResponseStorage => ("禁用响应存储", "Disable response storage", "レスポンスの保存を無効にする"),
    DisableResponseStorageHelp => ("disable_response_storage = true（ZDR / 不支持存储的上游）。", "disable_response_storage = true (ZDR or upstreams without storage support).", "disable_response_storage = true（ZDR または保存非対応の接続先）。"),
    SupportsWebSockets => ("支持 Responses WebSocket", "Supports Responses WebSocket", "Responses WebSocket に対応"),
    SupportsWebSocketsHelp => ("直连时可手动声明；模型供应商模式由所选供应商的 WebSocket 能力决定，不可单独覆盖。", "Set manually for direct connections. In Model Provider mode, this is determined by the selected provider and cannot be overridden.", "直接接続では手動で設定できます。モデルプロバイダーモードでは選択したプロバイダーの機能で決まり、個別に上書きできません。"),
    QueryParameters => ("Query 参数", "Query parameters", "クエリパラメーター"),
    QueryParametersHelp => ("追加到每次请求 URL（如 Azure 的 api-version）。", "Appended to each request URL, such as Azure's api-version.", "各リクエスト URL に追加します（Azure の api-version など）。"),
    HttpHeaders => ("HTTP 头", "HTTP headers", "HTTP ヘッダー"),
    ConfigTomlParseFailed => ("config.toml 解析失败: {e}", "Could not parse config.toml: {e}", "config.toml を解析できません: {e}"),
    AuthJsonParseFailed => ("auth.json 解析失败: {e}", "Could not parse auth.json: {e}", "auth.json を解析できません: {e}"),
    CodexChatRemoved => ("wire_api = \"chat\" 已被 Codex 移除（约 2026-02）。请改用 responses；纯 chat-completions 上游需经网关翻译。", "Codex removed wire_api = \"chat\" around February 2026. Use responses; chat-completions-only upstreams require gateway translation.", "Codex では 2026 年 2 月頃に wire_api = \"chat\" が削除されました。responses を使用してください。chat-completions のみの接続先にはゲートウェイでの変換が必要です。"),
    CodexBaseUrlHint => ("Base URL 通常应以 /v1 结尾（Codex 会在其后拼接 /responses）。", "Base URL usually ends in /v1; Codex appends /responses.", "Base URL は通常 /v1 で終わります。Codex が /responses を追加します。"),
    CodexApiKeyMissing => ("第三方 API 模式尚未填写 API Key。", "No API Key has been entered for third-party API mode.", "サードパーティ API モードの API Key が入力されていません。"),
    CodexUnusedApiKey => ("仅账号登录模式不会使用该第三方 API Key，保存时将移除。", "Account-only mode does not use this third-party API Key; it will be removed when saved.", "アカウントのみのモードではこのサードパーティ API Key を使用しません。保存時に削除されます。"),
    CodexLoginMissing => ("此供应商尚未保存 ChatGPT 登录态；应用时会沿用当前 auth.json，请先在 Codex 完成登录。", "This provider has no saved ChatGPT login. Applying it keeps the current auth.json; sign in through Codex first.", "このプロバイダーには ChatGPT のログインが保存されていません。適用時は現在の auth.json を使用します。先に Codex でログインしてください。"),
    OpenAiOfficial => ("OpenAI 官方", "Official OpenAI", "OpenAI 公式"),
    ConfigName => ("配置名称", "Configuration name", "設定名"),
    GrokProfileHelp => ("[models].default 与 [model.<name>] 使用的客户端配置名。", "Client configuration name used by [models].default and [model.<name>].", "[models].default と [model.<name>] で使用するクライアント設定名です。"),
    UpstreamModel => ("上游模型", "Upstream model", "接続先モデル"),
    EnvironmentVariable => ("环境变量", "Environment variable", "環境変数"),
    EnvironmentVariableName => ("环境变量名", "Environment variable name", "環境変数名"),
    ProtocolSection => ("协议", "Protocol", "プロトコル"),
    ContextWindow => ("上下文窗口", "Context window", "コンテキストウィンドウ"),
    FieldRequired => ("{label}不能为空。", "{label} is required.", "{label} を入力してください。"),
    CredentialMissing => ("请填写 API Key 或环境变量名。", "Enter an API Key or an environment variable name.", "API Key または環境変数名を入力してください。"),
    ContextWindowInvalid => ("上下文窗口必须是正整数。", "Context window must be a positive integer.", "コンテキストウィンドウには正の整数を入力してください。"),
    ApiMode => ("API 模式 (api_mode)", "API mode (api_mode)", "API モード (api_mode)"),
    OpenAiCompatibleHint => ("OpenAI 兼容 /chat/completions", "OpenAI-compatible /chat/completions", "OpenAI 互換の /chat/completions"),
    HermesApiModeHelp => ("必填：决定 Hermes 用哪种协议请求该供应商；缺省会被当作 chat_completions，会让 Anthropic/Codex/Bedrock 供应商失效。", "Required: determines the provider's request protocol. The default is chat_completions, which will not work for Anthropic/Codex/Bedrock providers.", "必須です。プロバイダーへのリクエストプロトコルを指定します。既定の chat_completions では Anthropic/Codex/Bedrock プロバイダーを利用できません。"),
    ModelList => ("模型列表", "Model list", "モデル一覧"),
    ContextLength => ("上下文长度", "Context length", "コンテキスト長"),
    HermesModelsHelp => ("首行的模型 ID 会写入单数 model: 字段，供 Hermes 运行时与 /model 选择器作为默认模型。", "The first row's model ID is written to the singular model: field as the default for Hermes and its /model picker.", "先頭行のモデル ID が単数の model: フィールドに書き込まれ、Hermes と /model セレクターの既定値になります。"),
    RateLimitDelay => ("限流延迟 (rate_limit_delay)", "Rate limit delay (rate_limit_delay)", "レート制限の待機時間 (rate_limit_delay)"),
    RateLimitDelayHelp => ("每次请求之间的间隔秒数（浮点）；留空表示不限流。", "Seconds between requests (decimals allowed). Leave blank for no rate limit.", "リクエスト間の待機秒数です（小数可）。空欄の場合は制限しません。"),
    ConfigYamlParseFailed => ("config.yaml 解析失败: {e}", "Could not parse config.yaml: {e}", "config.yaml を解析できません: {e}"),
    CustomProvidersMissing => ("缺少 custom_providers 条目", "Missing custom_providers entry", "custom_providers の項目がありません"),
    HermesApiModeRequired => ("api_mode 为必填；缺省会被当作 chat_completions，将使 Anthropic/Codex/Bedrock 供应商失效。", "api_mode is required. Its default, chat_completions, will not work for Anthropic/Codex/Bedrock providers.", "api_mode は必須です。既定の chat_completions では Anthropic/Codex/Bedrock プロバイダーを利用できません。"),
    HermesApiModeInvalid => ("未知的 api_mode \"{api_mode}\"；必须是 chat_completions / anthropic_messages / codex_responses / bedrock_converse 之一。", "Unknown api_mode \"{api_mode}\"; expected chat_completions / anthropic_messages / codex_responses / bedrock_converse.", "不明な api_mode \"{api_mode}\" です。chat_completions / anthropic_messages / codex_responses / bedrock_converse のいずれかを指定してください。"),
    HermesModelsMissing => ("未配置任何模型；切换到该供应商时 model.default 不会被更新。", "No models are configured; switching to this provider will not update model.default.", "モデルが設定されていません。このプロバイダーに切り替えても model.default は更新されません。"),
    HermesContextInvalid => ("模型 \"{model}\" 的上下文长度 \"{ctx}\" 不是正整数，将被忽略。", "Model \"{model}\" has an invalid context length \"{ctx}\"; it must be a positive integer and will be ignored.", "モデル \"{model}\" のコンテキスト長 \"{ctx}\" は正の整数ではないため、無視されます。"),
    RateLimitDelayInvalid => ("限流延迟必须是数字（秒），将被忽略。", "Rate limit delay must be a number of seconds; it will be ignored.", "レート制限の待機時間は秒数で入力してください。この値は無視されます。"),
    KimiProviderIdHelp => ("写入 config.toml 的 [providers.<id>]，模型通过该 ID 引用供应商。", "Written to [providers.<id>] in config.toml; models refer to the provider by this ID.", "config.toml の [providers.<id>] に書き込まれ、モデルはこの ID でプロバイダーを参照します。"),
    ProtocolTypeHelp => ("必须与上游实际协议一致。", "Must match the upstream's actual protocol.", "接続先の実際のプロトコルに合わせてください。"),
    DefaultModelAlias => ("默认模型别名", "Default model alias", "既定のモデルエイリアス"),
    DefaultModelAliasHelp => ("必须与下方某一行的模型别名一致；切换供应商时会写入 default_model。", "Must match a model alias below; written to default_model when switching providers.", "下のモデルエイリアスと一致させてください。プロバイダー切り替え時に default_model に書き込まれます。"),
    ModelAlias => ("模型别名", "Model alias", "モデルエイリアス"),
    UpstreamModelId => ("上游模型 ID", "Upstream model ID", "接続先モデル ID"),
    Context => ("上下文", "Context", "コンテキスト"),
    MaximumOutput => ("最大输出", "Maximum output", "最大出力"),
    Capabilities => ("能力", "Capabilities", "機能"),
    KimiModelsHelp => ("上下文为必填正整数；能力使用英文逗号分隔。", "Context must be a positive integer. Separate capabilities with commas.", "コンテキストは正の整数で入力してください。機能は半角カンマで区切ります。"),
    CustomHeaders => ("自定义请求头", "Custom headers", "カスタムヘッダー"),
    CredentialEnvironment => ("凭据环境映射", "Credential environment mapping", "認証情報の環境変数マッピング"),
    KimiEnvironmentHelp => ("这些键写入 [providers.<id>.env]，并不会修改 shell 环境。", "Written to [providers.<id>.env]; does not change the shell environment.", "[providers.<id>.env] に書き込まれます。シェルの環境は変更しません。"),
    KimiTomlParseFailed => ("config.toml 解析失败: {error}", "Could not parse config.toml: {error}", "config.toml を解析できません: {error}"),
    KimiTomlConvertFailed => ("config.toml 转换失败: {error}", "Could not convert config.toml: {error}", "config.toml を変換できません: {error}"),
    ProviderIdRequired => ("Provider ID 不能为空。", "Provider ID is required.", "Provider ID を入力してください。"),
    ProtocolTypeRequired => ("协议类型不能为空。", "Protocol type is required.", "プロトコルの種類を指定してください。"),
    ModelRequired => ("至少需要配置一个模型。", "Configure at least one model.", "モデルを 1 つ以上設定してください。"),
    KimiUpstreamModelMissing => ("模型 {alias} 缺少上游模型 ID。", "Model {alias} has no upstream model ID.", "モデル {alias} の接続先モデル ID がありません。"),
    KimiContextInvalid => ("模型 {alias} 的上下文必须是正整数。", "Model {alias} requires a positive integer for context.", "モデル {alias} のコンテキストには正の整数を入力してください。"),
    DefaultModelAliasInvalid => ("默认模型别名必须存在于模型列表中。", "The default model alias must exist in the model list.", "既定のモデルエイリアスはモデル一覧に存在する必要があります。"),
    CustomPreset => ("自定义", "Custom", "カスタム"),
    OfficialSubscriptionPreset => ("官方订阅", "Official subscription", "公式サブスクリプション"),
    RawEditingUnsupported => ("此应用暂不支持直接编辑文件，请使用上方表单。", "Direct file editing is not supported for this app yet. Use the form above.", "このアプリではファイルの直接編集にまだ対応していません。上のフォームを使用してください。"),
    StationAuthHelp => ("模型供应商模式下 Authorization 一律由网关签发的密钥承担；选择组合模式可在此之上保留 auth.json 里的 ChatGPT 登录态。", "In Model Provider mode, authorization always uses the gateway-issued key. Combined mode also preserves the ChatGPT login in auth.json.", "モデルプロバイダーモードでは、認証に常にゲートウェイ発行のキーを使用します。併用モードでは auth.json の ChatGPT ログインも保持します。"),
    StationCompactionHelp => ("远程压缩请求会转发到该模型供应商的 Responses 上游；开启后 config.toml 里的 provider name 会强制写为 OpenAI。", "Remote compaction requests go to this provider's Responses upstream. Enabling it forces provider name in config.toml to OpenAI.", "リモート圧縮リクエストはこのプロバイダーの Responses 接続先に転送されます。有効にすると config.toml の provider name が OpenAI に固定されます。"),
    StationCompactionUnavailable => ("该模型供应商没有启用中的 Responses 上游，网关无法转发远程压缩请求。", "This provider has no enabled Responses upstream, so the gateway cannot forward remote compaction requests.", "このプロバイダーには有効な Responses 接続先がないため、ゲートウェイでリモート圧縮リクエストを転送できません。"),
    ApiProtocol => ("API 协议", "API protocol", "API プロトコル"),
    ChatCompletionsHint => ("/chat/completions 风格", "/chat/completions format", "/chat/completions 形式"),
    ResponsesHint => ("/responses 风格", "/responses format", "/responses 形式"),
    MessagesHint => ("/v1/messages 风格", "/v1/messages format", "/v1/messages 形式"),
    OpenClawProtocolHelp => ("OpenClaw 用此枚举决定请求协议；必填，默认 openai-completions。", "Required protocol selector for OpenClaw requests; the default is openai-completions.", "OpenClaw のリクエストプロトコルを選択します。必須で、既定値は openai-completions です。"),
    OpenClawBaseUrlHelp => ("写入 baseUrl；OpenClaw 按所选协议在其后拼接路径。", "Written to baseUrl; OpenClaw appends the path for the selected protocol.", "baseUrl に書き込まれ、OpenClaw が選択したプロトコルのパスを追加します。"),
    OpenClawApiKeyHelp => ("写入 apiKey。", "Written to apiKey.", "apiKey に書き込まれます。"),
    Reasoning => ("推理", "Reasoning", "推論"),
    OpenClawModelsHelp => ("provider.models[]，每行一个模型；全部保留，非破坏式回写。", "One model per row in provider.models[]. All entries are preserved when saving.", "provider.models[] の各行がモデル 1 つに対応します。保存時にすべての項目を保持します。"),
    OpenClawHeadersHelp => ("写入 headers{}，附加到每次请求。", "Written to headers{} and appended to every request.", "headers{} に書き込まれ、各リクエストに追加されます。"),
    OpenClawJsonParseFailed => ("openclaw.json 解析失败: {e}", "Could not parse openclaw.json: {e}", "openclaw.json を解析できません: {e}"),
    OpenClawProviderMissing => ("缺少 models.providers.<id> 配置", "Missing models.providers.<id> configuration", "models.providers.<id> の設定がありません"),
    ApiProtocolRequired => ("API 协议为必填项。", "API protocol is required.", "API プロトコルを指定してください。"),
    OpenClawProtocolInvalid => ("api = \"{api}\" 不是合法协议；必须是 openai-completions / openai-responses / anthropic-messages / google-generative-ai / bedrock-converse-stream 之一。", "api = \"{api}\" is not a valid protocol; expected openai-completions / openai-responses / anthropic-messages / google-generative-ai / bedrock-converse-stream.", "api = \"{api}\" は無効です。openai-completions / openai-responses / anthropic-messages / google-generative-ai / bedrock-converse-stream のいずれかを指定してください。"),
    BaseUrlMissing => ("尚未填写 Base URL。", "No Base URL has been entered.", "Base URL が入力されていません。"),
    DuplicateModelId => ("模型 ID \"{id}\" 重复。", "Duplicate model ID \"{id}\".", "モデル ID \"{id}\" が重複しています。"),
    OpenClawContextInvalid => ("模型 \"{id}\" 的上下文窗口不是有效整数，将被忽略。", "Model \"{id}\" has an invalid integer for its context window; it will be ignored.", "モデル \"{id}\" のコンテキストウィンドウは有効な整数ではないため、無視されます。"),
    ModelsMissing => ("尚未配置任何模型。", "No models are configured.", "モデルが設定されていません。"),
    OpenCodeProviderIdHelp => ("opencode.json 中 provider.<id> 的键，也是 OpenCode 内引用该供应商的标识。", "The provider.<id> key in opencode.json, also used to identify the provider within OpenCode.", "opencode.json の provider.<id> のキーで、OpenCode 内のプロバイダー識別子にも使用されます。"),
    AiSdkPackage => ("AI SDK 包 (npm)", "AI SDK package (npm)", "AI SDK パッケージ (npm)"),
    OpenCodeCompatibleHint => ("OpenAI 兼容的第三方端点（最常用）", "OpenAI-compatible third-party endpoint (most common)", "OpenAI 互換のサードパーティエンドポイント（一般的な選択肢）"),
    OfficialOpenAiHint => ("官方 OpenAI", "Official OpenAI", "OpenAI 公式"),
    BedrockHint => ("Amazon Bedrock（鉴权经选项扩展）", "Amazon Bedrock (credentials in additional options)", "Amazon Bedrock（認証情報は追加オプションで設定）"),
    OpenCodePackageHelp => ("决定 OpenCode 加载哪个 AI SDK 适配器；必填，请按上游真实协议选择。", "Required: selects the AI SDK adapter OpenCode loads. Choose the upstream's actual protocol.", "必須です。OpenCode が読み込む AI SDK アダプターを、接続先の実際のプロトコルに合わせて選択してください。"),
    OpenCodeNameHelp => ("OpenCode UI 中展示的名称；留空则回退到 Provider ID。", "Name shown in OpenCode. Leave blank to use Provider ID.", "OpenCode に表示する名前です。空欄の場合は Provider ID を使用します。"),
    EndpointSection => ("端点", "Endpoint", "エンドポイント"),
    OpenCodeBaseUrlHelp => ("OpenAI 兼容端点通常以 /v1 结尾；官方包可留空使用默认地址。", "OpenAI-compatible endpoints usually end in /v1. Official packages may use their default URL when blank.", "OpenAI 互換エンドポイントは通常 /v1 で終わります。公式パッケージでは空欄にすると既定の URL を使用できます。"),
    OpenCodeApiKeyHelp => ("写入 options.apiKey；Bedrock 等需要其它鉴权字段时请改用下方“选项扩展”。", "Written to options.apiKey. Use Additional options below for other credentials, such as Bedrock authentication.", "options.apiKey に書き込まれます。Bedrock などの別の認証情報は、下の追加オプションで設定してください。"),
    OptionsSection => ("选项", "Options", "オプション"),
    AdditionalOptions => ("选项扩展 (options.*)", "Additional options (options.*)", "追加オプション (options.*)"),
    AdditionalOptionsHelp => ("写入 options 下的额外字段（如 setCacheKey、region/accessKeyId/secretAccessKey 等 Bedrock 鉴权）。布尔/数字会按字面量解析。", "Extra options fields, such as setCacheKey or Bedrock region/accessKeyId/secretAccessKey. Boolean and numeric values are parsed as literals.", "setCacheKey や Bedrock の region/accessKeyId/secretAccessKey など、options の追加フィールドです。真偽値と数値はリテラルとして解析します。"),
    OptionHeaders => ("请求头 (options.headers)", "Request headers (options.headers)", "リクエストヘッダー (options.headers)"),
    OptionHeadersHelp => ("附加到每次请求的 HTTP 头。", "Added to every HTTP request.", "各 HTTP リクエストに追加されます。"),
    OpenCodeModelsHelp => ("可配置多个模型；模型 ID 是 OpenCode 内部键，显示名是 UI 文本，两者相互独立。上下文/最大输出为可选数字（token）。", "Multiple models are supported. Model ID is OpenCode's internal key; display name is independent UI text. Context and maximum output are optional token counts.", "複数のモデルを設定できます。モデル ID は OpenCode の内部キーで、表示名とは別です。コンテキストと最大出力は任意のトークン数です。"),
    OpenCodeJsonParseFailed => ("opencode.json 解析失败: {e}", "Could not parse opencode.json: {e}", "opencode.json を解析できません: {e}"),
    OpenCodeProviderMissing => ("缺少 provider.<id> 配置", "Missing provider.<id> configuration", "provider.<id> の設定がありません"),
    AiSdkPackageRequired => ("必须选择 AI SDK 包 (npm)。", "Select an AI SDK package (npm).", "AI SDK パッケージ (npm) を選択してください。"),
    AiSdkPackageUnknown => ("未知的 npm 包 \"{npm}\"，OpenCode 可能无法加载。", "Unknown npm package \"{npm}\"; OpenCode may not be able to load it.", "不明な npm パッケージ \"{npm}\" です。OpenCode で読み込めない可能性があります。"),
    BedrockCredentialsHint => ("Bedrock 通常需要 region / accessKeyId / secretAccessKey，请在“选项扩展”中填写。", "Bedrock usually requires region / accessKeyId / secretAccessKey. Enter them in Additional options.", "Bedrock では通常 region / accessKeyId / secretAccessKey が必要です。追加オプションに入力してください。"),
    OpenCodeBaseUrlMissing => ("openai-compatible 端点通常需要填写 Base URL。", "An openai-compatible endpoint usually requires a Base URL.", "openai-compatible のエンドポイントでは通常 Base URL が必要です。"),
    ModelIdRequired => ("模型缺少 ID（ID 与显示名不同，必须填写）。", "A model has no ID. ID and display name are different; ID is required.", "モデル ID がありません。ID は表示名とは異なり、入力が必要です。"),
}

pub(super) fn text(key: TextKey) -> &'static str {
    #[cfg(test)]
    if let Some(locale) = TEST_LOCALE.with(|locale| locale.get()) {
        return lookup(key, locale);
    }
    lookup(key, i18n::current())
}

/// Substitute named parameters once. Inserted values are copied verbatim, so
/// a model ID containing braces can never become another translation token.
pub(super) fn format(key: TextKey, args: &[(&str, &dyn Display)]) -> String {
    let template = text(key);
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        let Some(end) = rest[start + 1..].find('}') else {
            output.push_str(&rest[start..]);
            return output;
        };
        let end = start + 1 + end;
        let name = &rest[start + 1..end];
        if let Some((_, value)) = args.iter().find(|(key, _)| *key == name) {
            use std::fmt::Write;
            write!(output, "{value}").expect("writing to a String cannot fail");
        } else {
            output.push_str(&rest[start..=end]);
        }
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}

// Locale tests must not race application tests that use the process-wide locale.
#[cfg(test)]
thread_local! {
    static TEST_LOCALE: std::cell::Cell<Option<Locale>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn with_locale<T>(locale: Locale, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Locale>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LOCALE.with(|locale| locale.set(self.0));
        }
    }
    let _restore = Restore(TEST_LOCALE.with(|current| current.replace(Some(locale))));
    run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppType;
    use crate::provider_config::{self, FieldKind, FormSection, GridCellKind};
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    fn placeholders(template: &str) -> BTreeSet<&str> {
        template
            .split('{')
            .skip(1)
            .filter_map(|part| part.split_once('}').map(|(name, _)| name))
            .filter(|name| !name.is_empty())
            .collect()
    }

    #[test]
    fn every_message_has_all_locales_and_matching_parameters() {
        for key in ALL {
            let parameters = placeholders(lookup(*key, Locale::Zh));
            for locale in Locale::ALL {
                let translated = lookup(*key, locale);
                assert!(!translated.trim().is_empty(), "{key:?} / {locale:?}");
                assert_eq!(parameters, placeholders(translated), "{key:?} / {locale:?}");
            }
        }
    }

    fn schema_identity(schema: &[FormSection]) -> Value {
        Value::Array(
            schema
                .iter()
                .map(|section| {
                    let fields: Vec<Value> = section.fields.iter().map(|field| {
                let kind = match &field.kind {
                    FieldKind::Text { .. } => json!("text"),
                    FieldKind::Secret { .. } => json!("secret"),
                    FieldKind::Toggle => json!("toggle"),
                    FieldKind::KeyValue { .. } => json!("key_value"),
                    FieldKind::Select { options } => json!({
                        "select": options.iter().map(|option| &option.value).collect::<Vec<_>>()
                    }),
                    FieldKind::ModelGrid { columns } => json!({
                        "grid": columns.iter().map(|column| json!({
                            "key": column.key,
                            "kind": match column.kind {
                                GridCellKind::Text { .. } => "text",
                                GridCellKind::Toggle => "toggle",
                            }
                        })).collect::<Vec<_>>()
                    }),
                };
                json!({
                    "id": field.id,
                    "required": field.required,
                    "visible_when": field.visible_when,
                    "kind": kind,
                })
            }).collect();
                    json!({"id": section.id, "advanced": section.advanced, "fields": fields})
                })
                .collect(),
        )
    }

    #[test]
    fn changing_locale_preserves_schema_ids_protocols_and_preset_values() {
        for app in AppType::all() {
            let codec = provider_config::config_for(app).unwrap();
            let snapshot = || {
                let schema = codec.schema();
                let mut ids = BTreeSet::new();
                for section in &schema {
                    assert!(!section.id.is_empty(), "{app:?} needs a stable section ID");
                    assert!(ids.insert(&section.id), "duplicate section ID in {app:?}");
                }
                let presets: Vec<Value> = codec
                    .presets()
                    .iter()
                    .map(|preset| {
                        json!({
                            "values": preset.values,
                            "category": preset.category,
                            "display_name": preset.display_name,
                            "website_url": preset.website_url,
                        })
                    })
                    .collect();
                (schema_identity(&schema), presets)
            };
            let original = with_locale(Locale::Zh, snapshot);
            for locale in [Locale::En, Locale::Ja] {
                assert_eq!(
                    original,
                    with_locale(locale, snapshot),
                    "{app:?} / {locale:?}"
                );
            }
        }
    }

    fn contains_han(text: &str) -> bool {
        text.chars()
            .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
    }

    #[test]
    fn english_builtin_forms_do_not_fall_back_to_chinese_display_text() {
        with_locale(Locale::En, || {
            for app in AppType::all() {
                let codec = provider_config::config_for(app).unwrap();
                let mut displayed = Vec::new();
                for section in codec.schema() {
                    displayed.push(section.title);
                    for field in section.fields {
                        displayed.push(field.label);
                        displayed.extend(field.help);
                        match field.kind {
                            FieldKind::Text { placeholder } | FieldKind::Secret { placeholder } => {
                                displayed.push(placeholder);
                            }
                            FieldKind::Select { options } => {
                                for option in options {
                                    displayed.push(option.label);
                                    displayed.extend(option.hint);
                                }
                            }
                            FieldKind::ModelGrid { columns } => {
                                for column in columns {
                                    displayed.push(column.label);
                                    if let GridCellKind::Text { placeholder } = column.kind {
                                        displayed.push(placeholder);
                                    }
                                }
                            }
                            FieldKind::KeyValue {
                                key_placeholder,
                                value_placeholder,
                            } => {
                                displayed.push(key_placeholder);
                                displayed.push(value_placeholder);
                            }
                            FieldKind::Toggle => {}
                        }
                    }
                }
                displayed.extend(codec.presets().into_iter().map(|preset| preset.name));
                displayed.extend(
                    codec
                        .validate(&Default::default())
                        .into_iter()
                        .map(|issue| issue.message),
                );
                assert!(!displayed.is_empty());
                for text in displayed {
                    assert!(
                        !contains_han(&text),
                        "{app:?}: untranslated UI text {text:?}"
                    );
                }
            }
        });
    }

    #[test]
    fn kimi_presets_and_dynamic_validation_follow_english_and_japanese() {
        let codec = provider_config::config_for(AppType::KimiCode).unwrap();
        let alias = "用户模型-{alias}-{error}";
        let values = json!({
            "provider_id": "example", "provider_type": "openai",
            "models": [{ "alias": alias, "model": "", "context": "invalid" }]
        })
        .as_object()
        .unwrap()
        .clone();
        for (locale, custom, official, expected) in [
            (
                Locale::En,
                "Custom",
                "Official subscription",
                "has no upstream model ID",
            ),
            (
                Locale::Ja,
                "カスタム",
                "公式サブスクリプション",
                "接続先モデル ID がありません",
            ),
        ] {
            with_locale(locale, || {
                let presets = codec.presets();
                assert_eq!(presets[0].name, custom);
                assert_eq!(presets[1].name, official);
                let issues = codec.validate(&values);
                assert_eq!(issues.len(), 2);
                assert!(issues[0].message.contains(expected));
                assert!(issues.iter().all(|issue| issue.message.contains(alias)));
                assert!(
                    issues
                        .iter()
                        .all(|issue| issue.field.as_deref() == Some("models"))
                );
            });
        }
    }

    #[test]
    fn official_auth_sections_have_language_independent_identity() {
        for locale in Locale::ALL {
            with_locale(locale, || {
                for app in [
                    AppType::Claude,
                    AppType::ClaudeDesktop,
                    AppType::Codex,
                    AppType::GrokBuild,
                ] {
                    let codec = provider_config::config_for(app).unwrap();
                    let schema = codec.schema();
                    let section = schema
                        .iter()
                        .find(|section| section.id == "endpoint_auth")
                        .unwrap();
                    assert!(section.fields.iter().any(|field| field.id == "base_url"));
                    assert!(section.fields.iter().any(|field| field.id == "api_key"));
                }
            });
        }
    }

    #[test]
    fn direct_file_errors_translate_without_rewriting_parser_details() {
        let codec = provider_config::config_for(AppType::Codex).unwrap();
        let contents = ["model = \"用户文件-{e}\" trailing".to_string()];
        for (locale, prefix) in [
            (Locale::Zh, "config.toml 解析失败: "),
            (Locale::En, "Could not parse config.toml: "),
            (Locale::Ja, "config.toml を解析できません: "),
        ] {
            with_locale(locale, || {
                let error = codec.parse_files(&contents).unwrap_err();
                assert!(error.starts_with(prefix));
                assert!(error.contains("用户文件-{e}"));
            });
        }
    }
}
