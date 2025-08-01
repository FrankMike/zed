use ai_onboarding::YoungAccountBanner;
use anthropic::AnthropicModelMode;
use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use client::{Client, CloudUserStore, ModelRequestUsage, UserStore, zed_urls};
use cloud_llm_client::{
    CLIENT_SUPPORTS_STATUS_MESSAGES_HEADER_NAME, CURRENT_PLAN_HEADER_NAME, CompletionBody,
    CompletionEvent, CompletionRequestStatus, CountTokensBody, CountTokensResponse,
    EXPIRED_LLM_TOKEN_HEADER_NAME, ListModelsResponse, MODEL_REQUESTS_RESOURCE_HEADER_VALUE, Plan,
    SERVER_SUPPORTS_STATUS_MESSAGES_HEADER_NAME, SUBSCRIPTION_LIMIT_RESOURCE_HEADER_NAME,
    TOOL_USE_LIMIT_REACHED_HEADER_NAME, ZED_VERSION_HEADER_NAME,
};
use futures::{
    AsyncBufReadExt, FutureExt, Stream, StreamExt, future::BoxFuture, stream::BoxStream,
};
use google_ai::GoogleModelMode;
use gpui::{
    AnyElement, AnyView, App, AsyncApp, Context, Entity, SemanticVersion, Subscription, Task,
};
use http_client::http::{HeaderMap, HeaderValue};
use http_client::{AsyncBody, HttpClient, Method, Response, StatusCode};
use language_model::{
    AuthenticateError, LanguageModel, LanguageModelCacheConfiguration,
    LanguageModelCompletionError, LanguageModelCompletionEvent, LanguageModelId, LanguageModelName,
    LanguageModelProvider, LanguageModelProviderId, LanguageModelProviderName,
    LanguageModelProviderState, LanguageModelProviderTosView, LanguageModelRequest,
    LanguageModelToolChoice, LanguageModelToolSchemaFormat, LlmApiToken,
    ModelRequestLimitReachedError, PaymentRequiredError, RateLimiter, RefreshLlmTokenListener,
};
use release_channel::AppVersion;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use settings::SettingsStore;
use smol::io::{AsyncReadExt, BufReader};
use std::pin::Pin;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use ui::{TintColor, prelude::*};
use util::{ResultExt as _, maybe};

use crate::provider::anthropic::{AnthropicEventMapper, count_anthropic_tokens, into_anthropic};
use crate::provider::google::{GoogleEventMapper, into_google};
use crate::provider::open_ai::{OpenAiEventMapper, count_open_ai_tokens, into_open_ai};

const PROVIDER_ID: LanguageModelProviderId = language_model::ZED_CLOUD_PROVIDER_ID;
const PROVIDER_NAME: LanguageModelProviderName = language_model::ZED_CLOUD_PROVIDER_NAME;

#[derive(Default, Clone, Debug, PartialEq)]
pub struct ZedDotDevSettings {
    pub available_models: Vec<AvailableModel>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AvailableProvider {
    Anthropic,
    OpenAi,
    Google,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AvailableModel {
    /// The provider of the language model.
    pub provider: AvailableProvider,
    /// The model's name in the provider's API. e.g. claude-3-5-sonnet-20240620
    pub name: String,
    /// The name displayed in the UI, such as in the assistant panel model dropdown menu.
    pub display_name: Option<String>,
    /// The size of the context window, indicating the maximum number of tokens the model can process.
    pub max_tokens: usize,
    /// The maximum number of output tokens allowed by the model.
    pub max_output_tokens: Option<u64>,
    /// The maximum number of completion tokens allowed by the model (o1-* only)
    pub max_completion_tokens: Option<u64>,
    /// Override this model with a different Anthropic model for tool calls.
    pub tool_override: Option<String>,
    /// Indicates whether this custom model supports caching.
    pub cache_configuration: Option<LanguageModelCacheConfiguration>,
    /// The default temperature to use for this model.
    pub default_temperature: Option<f32>,
    /// Any extra beta headers to provide when using the model.
    #[serde(default)]
    pub extra_beta_headers: Vec<String>,
    /// The model's mode (e.g. thinking)
    pub mode: Option<ModelMode>,
}

#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ModelMode {
    #[default]
    Default,
    Thinking {
        /// The maximum number of tokens to use for reasoning. Must be lower than the model's `max_output_tokens`.
        budget_tokens: Option<u32>,
    },
}

impl From<ModelMode> for AnthropicModelMode {
    fn from(value: ModelMode) -> Self {
        match value {
            ModelMode::Default => AnthropicModelMode::Default,
            ModelMode::Thinking { budget_tokens } => AnthropicModelMode::Thinking { budget_tokens },
        }
    }
}

pub struct CloudLanguageModelProvider {
    client: Arc<Client>,
    state: gpui::Entity<State>,
    _maintain_client_status: Task<()>,
}

pub struct State {
    client: Arc<Client>,
    llm_api_token: LlmApiToken,
    user_store: Entity<UserStore>,
    cloud_user_store: Entity<CloudUserStore>,
    status: client::Status,
    accept_terms_of_service_task: Option<Task<Result<()>>>,
    models: Vec<Arc<cloud_llm_client::LanguageModel>>,
    default_model: Option<Arc<cloud_llm_client::LanguageModel>>,
    default_fast_model: Option<Arc<cloud_llm_client::LanguageModel>>,
    recommended_models: Vec<Arc<cloud_llm_client::LanguageModel>>,
    _fetch_models_task: Task<()>,
    _settings_subscription: Subscription,
    _llm_token_subscription: Subscription,
}

impl State {
    fn new(
        client: Arc<Client>,
        user_store: Entity<UserStore>,
        cloud_user_store: Entity<CloudUserStore>,
        status: client::Status,
        cx: &mut Context<Self>,
    ) -> Self {
        let refresh_llm_token_listener = RefreshLlmTokenListener::global(cx);

        Self {
            client: client.clone(),
            llm_api_token: LlmApiToken::default(),
            user_store,
            cloud_user_store,
            status,
            accept_terms_of_service_task: None,
            models: Vec::new(),
            default_model: None,
            default_fast_model: None,
            recommended_models: Vec::new(),
            _fetch_models_task: cx.spawn(async move |this, cx| {
                maybe!(async move {
                    let (client, cloud_user_store, llm_api_token) =
                        this.read_with(cx, |this, _cx| {
                            (
                                client.clone(),
                                this.cloud_user_store.clone(),
                                this.llm_api_token.clone(),
                            )
                        })?;

                    loop {
                        let is_authenticated =
                            cloud_user_store.read_with(cx, |this, _cx| this.is_authenticated())?;
                        if is_authenticated {
                            break;
                        }

                        cx.background_executor()
                            .timer(Duration::from_millis(100))
                            .await;
                    }

                    let response = Self::fetch_models(client, llm_api_token).await?;
                    this.update(cx, |this, cx| {
                        this.update_models(response, cx);
                    })
                })
                .await
                .context("failed to fetch Zed models")
                .log_err();
            }),
            _settings_subscription: cx.observe_global::<SettingsStore>(|_, cx| {
                cx.notify();
            }),
            _llm_token_subscription: cx.subscribe(
                &refresh_llm_token_listener,
                move |this, _listener, _event, cx| {
                    let client = this.client.clone();
                    let llm_api_token = this.llm_api_token.clone();
                    cx.spawn(async move |this, cx| {
                        llm_api_token.refresh(&client).await?;
                        let response = Self::fetch_models(client, llm_api_token).await?;
                        this.update(cx, |this, cx| {
                            this.update_models(response, cx);
                        })
                    })
                    .detach_and_log_err(cx);
                },
            ),
        }
    }

    fn is_signed_out(&self, cx: &App) -> bool {
        !self.cloud_user_store.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let client = self.client.clone();
        cx.spawn(async move |state, cx| {
            client
                .authenticate_and_connect(true, &cx)
                .await
                .into_response()?;
            state.update(cx, |_, cx| cx.notify())
        })
    }

    fn has_accepted_terms_of_service(&self, cx: &App) -> bool {
        self.cloud_user_store.read(cx).has_accepted_tos()
    }

    fn accept_terms_of_service(&mut self, cx: &mut Context<Self>) {
        let user_store = self.user_store.clone();
        self.accept_terms_of_service_task = Some(cx.spawn(async move |this, cx| {
            let _ = user_store
                .update(cx, |store, cx| store.accept_terms_of_service(cx))?
                .await;
            this.update(cx, |this, cx| {
                this.accept_terms_of_service_task = None;
                cx.notify()
            })
        }));
    }

    fn update_models(&mut self, response: ListModelsResponse, cx: &mut Context<Self>) {
        let mut models = Vec::new();

        for model in response.models {
            models.push(Arc::new(model.clone()));

            // Right now we represent thinking variants of models as separate models on the client,
            // so we need to insert variants for any model that supports thinking.
            if model.supports_thinking {
                models.push(Arc::new(cloud_llm_client::LanguageModel {
                    id: cloud_llm_client::LanguageModelId(format!("{}-thinking", model.id).into()),
                    display_name: format!("{} Thinking", model.display_name),
                    ..model
                }));
            }
        }

        self.default_model = models
            .iter()
            .find(|model| model.id == response.default_model)
            .cloned();
        self.default_fast_model = models
            .iter()
            .find(|model| model.id == response.default_fast_model)
            .cloned();
        self.recommended_models = response
            .recommended_models
            .iter()
            .filter_map(|id| models.iter().find(|model| &model.id == id))
            .cloned()
            .collect();
        self.models = models;
        cx.notify();
    }

    async fn fetch_models(
        client: Arc<Client>,
        llm_api_token: LlmApiToken,
    ) -> Result<ListModelsResponse> {
        let http_client = &client.http_client();
        let token = llm_api_token.acquire(&client).await?;

        let request = http_client::Request::builder()
            .method(Method::GET)
            .uri(http_client.build_zed_llm_url("/models", &[])?.as_ref())
            .header("Authorization", format!("Bearer {token}"))
            .body(AsyncBody::empty())?;
        let mut response = http_client
            .send(request)
            .await
            .context("failed to send list models request")?;

        if response.status().is_success() {
            let mut body = String::new();
            response.body_mut().read_to_string(&mut body).await?;
            return Ok(serde_json::from_str(&body)?);
        } else {
            let mut body = String::new();
            response.body_mut().read_to_string(&mut body).await?;
            anyhow::bail!(
                "error listing models.\nStatus: {:?}\nBody: {body}",
                response.status(),
            );
        }
    }
}

impl CloudLanguageModelProvider {
    pub fn new(
        user_store: Entity<UserStore>,
        cloud_user_store: Entity<CloudUserStore>,
        client: Arc<Client>,
        cx: &mut App,
    ) -> Self {
        let mut status_rx = client.status();
        let status = *status_rx.borrow();

        let state = cx.new(|cx| {
            State::new(
                client.clone(),
                user_store.clone(),
                cloud_user_store.clone(),
                status,
                cx,
            )
        });

        let state_ref = state.downgrade();
        let maintain_client_status = cx.spawn(async move |cx| {
            while let Some(status) = status_rx.next().await {
                if let Some(this) = state_ref.upgrade() {
                    _ = this.update(cx, |this, cx| {
                        if this.status != status {
                            this.status = status;
                            cx.notify();
                        }
                    });
                } else {
                    break;
                }
            }
        });

        Self {
            client,
            state: state.clone(),
            _maintain_client_status: maintain_client_status,
        }
    }

    fn create_language_model(
        &self,
        model: Arc<cloud_llm_client::LanguageModel>,
        llm_api_token: LlmApiToken,
    ) -> Arc<dyn LanguageModel> {
        Arc::new(CloudLanguageModel {
            id: LanguageModelId(SharedString::from(model.id.0.clone())),
            model,
            llm_api_token: llm_api_token.clone(),
            client: self.client.clone(),
            request_limiter: RateLimiter::new(4),
        })
    }
}

impl LanguageModelProviderState for CloudLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<gpui::Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for CloudLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn icon(&self) -> IconName {
        IconName::AiZed
    }

    fn default_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        let default_model = self.state.read(cx).default_model.clone()?;
        let llm_api_token = self.state.read(cx).llm_api_token.clone();
        Some(self.create_language_model(default_model, llm_api_token))
    }

    fn default_fast_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        let default_fast_model = self.state.read(cx).default_fast_model.clone()?;
        let llm_api_token = self.state.read(cx).llm_api_token.clone();
        Some(self.create_language_model(default_fast_model, llm_api_token))
    }

    fn recommended_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        let llm_api_token = self.state.read(cx).llm_api_token.clone();
        self.state
            .read(cx)
            .recommended_models
            .iter()
            .cloned()
            .map(|model| self.create_language_model(model, llm_api_token.clone()))
            .collect()
    }

    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        let llm_api_token = self.state.read(cx).llm_api_token.clone();
        self.state
            .read(cx)
            .models
            .iter()
            .cloned()
            .map(|model| self.create_language_model(model, llm_api_token.clone()))
            .collect()
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        let state = self.state.read(cx);
        !state.is_signed_out(cx) && state.has_accepted_terms_of_service(cx)
    }

    fn authenticate(&self, _cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        Task::ready(Ok(()))
    }

    fn configuration_view(&self, _: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|_| ConfigurationView::new(self.state.clone()))
            .into()
    }

    fn must_accept_terms(&self, cx: &App) -> bool {
        !self.state.read(cx).has_accepted_terms_of_service(cx)
    }

    fn render_accept_terms(
        &self,
        view: LanguageModelProviderTosView,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let state = self.state.read(cx);
        if state.has_accepted_terms_of_service(cx) {
            return None;
        }
        Some(
            render_accept_terms(view, state.accept_terms_of_service_task.is_some(), {
                let state = self.state.clone();
                move |_window, cx| {
                    state.update(cx, |state, cx| state.accept_terms_of_service(cx));
                }
            })
            .into_any_element(),
        )
    }

    fn reset_credentials(&self, _cx: &mut App) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }
}

fn render_accept_terms(
    view_kind: LanguageModelProviderTosView,
    accept_terms_of_service_in_progress: bool,
    accept_terms_callback: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let thread_fresh_start = matches!(view_kind, LanguageModelProviderTosView::ThreadFreshStart);
    let thread_empty_state = matches!(view_kind, LanguageModelProviderTosView::ThreadEmptyState);

    let terms_button = Button::new("terms_of_service", "Terms of Service")
        .style(ButtonStyle::Subtle)
        .icon(IconName::ArrowUpRight)
        .icon_color(Color::Muted)
        .icon_size(IconSize::XSmall)
        .when(thread_empty_state, |this| this.label_size(LabelSize::Small))
        .on_click(move |_, _window, cx| cx.open_url("https://zed.dev/terms-of-service"));

    let button_container = h_flex().child(
        Button::new("accept_terms", "I accept the Terms of Service")
            .when(!thread_empty_state, |this| {
                this.full_width()
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .icon(IconName::Check)
                    .icon_position(IconPosition::Start)
                    .icon_size(IconSize::Small)
            })
            .when(thread_empty_state, |this| {
                this.style(ButtonStyle::Tinted(TintColor::Warning))
                    .label_size(LabelSize::Small)
            })
            .disabled(accept_terms_of_service_in_progress)
            .on_click(move |_, window, cx| (accept_terms_callback)(window, cx)),
    );

    if thread_empty_state {
        h_flex()
            .w_full()
            .flex_wrap()
            .justify_between()
            .child(
                h_flex()
                    .child(
                        Label::new("To start using Zed AI, please read and accept the")
                            .size(LabelSize::Small),
                    )
                    .child(terms_button),
            )
            .child(button_container)
    } else {
        v_flex()
            .w_full()
            .gap_2()
            .child(
                h_flex()
                    .flex_wrap()
                    .when(thread_fresh_start, |this| this.justify_center())
                    .child(Label::new(
                        "To start using Zed AI, please read and accept the",
                    ))
                    .child(terms_button),
            )
            .child({
                match view_kind {
                    LanguageModelProviderTosView::TextThreadPopup => {
                        button_container.w_full().justify_end()
                    }
                    LanguageModelProviderTosView::Configuration => {
                        button_container.w_full().justify_start()
                    }
                    LanguageModelProviderTosView::ThreadFreshStart => {
                        button_container.w_full().justify_center()
                    }
                    LanguageModelProviderTosView::ThreadEmptyState => div().w_0(),
                }
            })
    }
}

pub struct CloudLanguageModel {
    id: LanguageModelId,
    model: Arc<cloud_llm_client::LanguageModel>,
    llm_api_token: LlmApiToken,
    client: Arc<Client>,
    request_limiter: RateLimiter,
}

struct PerformLlmCompletionResponse {
    response: Response<AsyncBody>,
    usage: Option<ModelRequestUsage>,
    tool_use_limit_reached: bool,
    includes_status_messages: bool,
}

impl CloudLanguageModel {
    async fn perform_llm_completion(
        client: Arc<Client>,
        llm_api_token: LlmApiToken,
        app_version: Option<SemanticVersion>,
        body: CompletionBody,
    ) -> Result<PerformLlmCompletionResponse> {
        let http_client = &client.http_client();

        let mut token = llm_api_token.acquire(&client).await?;
        let mut refreshed_token = false;

        loop {
            let request_builder = http_client::Request::builder()
                .method(Method::POST)
                .uri(http_client.build_zed_llm_url("/completions", &[])?.as_ref());
            let request_builder = if let Some(app_version) = app_version {
                request_builder.header(ZED_VERSION_HEADER_NAME, app_version.to_string())
            } else {
                request_builder
            };

            let request = request_builder
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .header(CLIENT_SUPPORTS_STATUS_MESSAGES_HEADER_NAME, "true")
                .body(serde_json::to_string(&body)?.into())?;
            let mut response = http_client.send(request).await?;
            let status = response.status();
            if status.is_success() {
                let includes_status_messages = response
                    .headers()
                    .get(SERVER_SUPPORTS_STATUS_MESSAGES_HEADER_NAME)
                    .is_some();

                let tool_use_limit_reached = response
                    .headers()
                    .get(TOOL_USE_LIMIT_REACHED_HEADER_NAME)
                    .is_some();

                let usage = if includes_status_messages {
                    None
                } else {
                    ModelRequestUsage::from_headers(response.headers()).ok()
                };

                return Ok(PerformLlmCompletionResponse {
                    response,
                    usage,
                    includes_status_messages,
                    tool_use_limit_reached,
                });
            }

            if !refreshed_token
                && response
                    .headers()
                    .get(EXPIRED_LLM_TOKEN_HEADER_NAME)
                    .is_some()
            {
                token = llm_api_token.refresh(&client).await?;
                refreshed_token = true;
                continue;
            }

            if status == StatusCode::FORBIDDEN
                && response
                    .headers()
                    .get(SUBSCRIPTION_LIMIT_RESOURCE_HEADER_NAME)
                    .is_some()
            {
                if let Some(MODEL_REQUESTS_RESOURCE_HEADER_VALUE) = response
                    .headers()
                    .get(SUBSCRIPTION_LIMIT_RESOURCE_HEADER_NAME)
                    .and_then(|resource| resource.to_str().ok())
                {
                    if let Some(plan) = response
                        .headers()
                        .get(CURRENT_PLAN_HEADER_NAME)
                        .and_then(|plan| plan.to_str().ok())
                        .and_then(|plan| cloud_llm_client::Plan::from_str(plan).ok())
                    {
                        let plan = match plan {
                            cloud_llm_client::Plan::ZedFree => proto::Plan::Free,
                            cloud_llm_client::Plan::ZedPro => proto::Plan::ZedPro,
                            cloud_llm_client::Plan::ZedProTrial => proto::Plan::ZedProTrial,
                        };
                        return Err(anyhow!(ModelRequestLimitReachedError { plan }));
                    }
                }
            } else if status == StatusCode::PAYMENT_REQUIRED {
                return Err(anyhow!(PaymentRequiredError));
            }

            let mut body = String::new();
            let headers = response.headers().clone();
            response.body_mut().read_to_string(&mut body).await?;
            return Err(anyhow!(ApiError {
                status,
                body,
                headers
            }));
        }
    }
}

#[derive(Debug, Error)]
#[error("cloud language model request failed with status {status}: {body}")]
struct ApiError {
    status: StatusCode,
    body: String,
    headers: HeaderMap<HeaderValue>,
}

/// Represents error responses from Zed's cloud API.
///
/// Example JSON for an upstream HTTP error:
/// ```json
/// {
///   "code": "upstream_http_error",
///   "message": "Received an error from the Anthropic API: upstream connect error or disconnect/reset before headers, reset reason: connection timeout",
///   "upstream_status": 503
/// }
/// ```
#[derive(Debug, serde::Deserialize)]
struct CloudApiError {
    code: String,
    message: String,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_status_code")]
    upstream_status: Option<StatusCode>,
    #[serde(default)]
    retry_after: Option<f64>,
}

fn deserialize_optional_status_code<'de, D>(deserializer: D) -> Result<Option<StatusCode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<u16> = Option::deserialize(deserializer)?;
    Ok(opt.and_then(|code| StatusCode::from_u16(code).ok()))
}

impl From<ApiError> for LanguageModelCompletionError {
    fn from(error: ApiError) -> Self {
        if let Ok(cloud_error) = serde_json::from_str::<CloudApiError>(&error.body) {
            if cloud_error.code.starts_with("upstream_http_") {
                let status = if let Some(status) = cloud_error.upstream_status {
                    status
                } else if cloud_error.code.ends_with("_error") {
                    error.status
                } else {
                    // If there's a status code in the code string (e.g. "upstream_http_429")
                    // then use that; otherwise, see if the JSON contains a status code.
                    cloud_error
                        .code
                        .strip_prefix("upstream_http_")
                        .and_then(|code_str| code_str.parse::<u16>().ok())
                        .and_then(|code| StatusCode::from_u16(code).ok())
                        .unwrap_or(error.status)
                };

                return LanguageModelCompletionError::UpstreamProviderError {
                    message: cloud_error.message,
                    status,
                    retry_after: cloud_error.retry_after.map(Duration::from_secs_f64),
                };
            }
        }

        let retry_after = None;
        LanguageModelCompletionError::from_http_status(
            PROVIDER_NAME,
            error.status,
            error.body,
            retry_after,
        )
    }
}

impl LanguageModel for CloudLanguageModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        LanguageModelName::from(self.model.display_name.clone())
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn upstream_provider_id(&self) -> LanguageModelProviderId {
        use cloud_llm_client::LanguageModelProvider::*;
        match self.model.provider {
            Anthropic => language_model::ANTHROPIC_PROVIDER_ID,
            OpenAi => language_model::OPEN_AI_PROVIDER_ID,
            Google => language_model::GOOGLE_PROVIDER_ID,
        }
    }

    fn upstream_provider_name(&self) -> LanguageModelProviderName {
        use cloud_llm_client::LanguageModelProvider::*;
        match self.model.provider {
            Anthropic => language_model::ANTHROPIC_PROVIDER_NAME,
            OpenAi => language_model::OPEN_AI_PROVIDER_NAME,
            Google => language_model::GOOGLE_PROVIDER_NAME,
        }
    }

    fn supports_tools(&self) -> bool {
        self.model.supports_tools
    }

    fn supports_images(&self) -> bool {
        self.model.supports_images
    }

    fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool {
        match choice {
            LanguageModelToolChoice::Auto
            | LanguageModelToolChoice::Any
            | LanguageModelToolChoice::None => true,
        }
    }

    fn supports_burn_mode(&self) -> bool {
        self.model.supports_max_mode
    }

    fn telemetry_id(&self) -> String {
        format!("zed.dev/{}", self.model.id)
    }

    fn tool_input_format(&self) -> LanguageModelToolSchemaFormat {
        match self.model.provider {
            cloud_llm_client::LanguageModelProvider::Anthropic
            | cloud_llm_client::LanguageModelProvider::OpenAi => {
                LanguageModelToolSchemaFormat::JsonSchema
            }
            cloud_llm_client::LanguageModelProvider::Google => {
                LanguageModelToolSchemaFormat::JsonSchemaSubset
            }
        }
    }

    fn max_token_count(&self) -> u64 {
        self.model.max_token_count as u64
    }

    fn max_token_count_in_burn_mode(&self) -> Option<u64> {
        self.model
            .max_token_count_in_max_mode
            .filter(|_| self.model.supports_max_mode)
            .map(|max_token_count| max_token_count as u64)
    }

    fn cache_configuration(&self) -> Option<LanguageModelCacheConfiguration> {
        match &self.model.provider {
            cloud_llm_client::LanguageModelProvider::Anthropic => {
                Some(LanguageModelCacheConfiguration {
                    min_total_token: 2_048,
                    should_speculate: true,
                    max_cache_anchors: 4,
                })
            }
            cloud_llm_client::LanguageModelProvider::OpenAi
            | cloud_llm_client::LanguageModelProvider::Google => None,
        }
    }

    fn count_tokens(
        &self,
        request: LanguageModelRequest,
        cx: &App,
    ) -> BoxFuture<'static, Result<u64>> {
        match self.model.provider {
            cloud_llm_client::LanguageModelProvider::Anthropic => {
                count_anthropic_tokens(request, cx)
            }
            cloud_llm_client::LanguageModelProvider::OpenAi => {
                let model = match open_ai::Model::from_id(&self.model.id.0) {
                    Ok(model) => model,
                    Err(err) => return async move { Err(anyhow!(err)) }.boxed(),
                };
                count_open_ai_tokens(request, model, cx)
            }
            cloud_llm_client::LanguageModelProvider::Google => {
                let client = self.client.clone();
                let llm_api_token = self.llm_api_token.clone();
                let model_id = self.model.id.to_string();
                let generate_content_request =
                    into_google(request, model_id.clone(), GoogleModelMode::Default);
                async move {
                    let http_client = &client.http_client();
                    let token = llm_api_token.acquire(&client).await?;

                    let request_body = CountTokensBody {
                        provider: cloud_llm_client::LanguageModelProvider::Google,
                        model: model_id,
                        provider_request: serde_json::to_value(&google_ai::CountTokensRequest {
                            generate_content_request,
                        })?,
                    };
                    let request = http_client::Request::builder()
                        .method(Method::POST)
                        .uri(
                            http_client
                                .build_zed_llm_url("/count_tokens", &[])?
                                .as_ref(),
                        )
                        .header("Content-Type", "application/json")
                        .header("Authorization", format!("Bearer {token}"))
                        .body(serde_json::to_string(&request_body)?.into())?;
                    let mut response = http_client.send(request).await?;
                    let status = response.status();
                    let headers = response.headers().clone();
                    let mut response_body = String::new();
                    response
                        .body_mut()
                        .read_to_string(&mut response_body)
                        .await?;

                    if status.is_success() {
                        let response_body: CountTokensResponse =
                            serde_json::from_str(&response_body)?;

                        Ok(response_body.tokens as u64)
                    } else {
                        Err(anyhow!(ApiError {
                            status,
                            body: response_body,
                            headers
                        }))
                    }
                }
                .boxed()
            }
        }
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>,
            LanguageModelCompletionError,
        >,
    > {
        let thread_id = request.thread_id.clone();
        let prompt_id = request.prompt_id.clone();
        let intent = request.intent;
        let mode = request.mode;
        let app_version = cx.update(|cx| AppVersion::global(cx)).ok();
        let thinking_allowed = request.thinking_allowed;
        match self.model.provider {
            cloud_llm_client::LanguageModelProvider::Anthropic => {
                let request = into_anthropic(
                    request,
                    self.model.id.to_string(),
                    1.0,
                    self.model.max_output_tokens as u64,
                    if thinking_allowed && self.model.id.0.ends_with("-thinking") {
                        AnthropicModelMode::Thinking {
                            budget_tokens: Some(4_096),
                        }
                    } else {
                        AnthropicModelMode::Default
                    },
                );
                let client = self.client.clone();
                let llm_api_token = self.llm_api_token.clone();
                let future = self.request_limiter.stream(async move {
                    let PerformLlmCompletionResponse {
                        response,
                        usage,
                        includes_status_messages,
                        tool_use_limit_reached,
                    } = Self::perform_llm_completion(
                        client.clone(),
                        llm_api_token,
                        app_version,
                        CompletionBody {
                            thread_id,
                            prompt_id,
                            intent,
                            mode,
                            provider: cloud_llm_client::LanguageModelProvider::Anthropic,
                            model: request.model.clone(),
                            provider_request: serde_json::to_value(&request)
                                .map_err(|e| anyhow!(e))?,
                        },
                    )
                    .await
                    .map_err(|err| match err.downcast::<ApiError>() {
                        Ok(api_err) => anyhow!(LanguageModelCompletionError::from(api_err)),
                        Err(err) => anyhow!(err),
                    })?;

                    let mut mapper = AnthropicEventMapper::new();
                    Ok(map_cloud_completion_events(
                        Box::pin(
                            response_lines(response, includes_status_messages)
                                .chain(usage_updated_event(usage))
                                .chain(tool_use_limit_reached_event(tool_use_limit_reached)),
                        ),
                        move |event| mapper.map_event(event),
                    ))
                });
                async move { Ok(future.await?.boxed()) }.boxed()
            }
            cloud_llm_client::LanguageModelProvider::OpenAi => {
                let client = self.client.clone();
                let model = match open_ai::Model::from_id(&self.model.id.0) {
                    Ok(model) => model,
                    Err(err) => return async move { Err(anyhow!(err).into()) }.boxed(),
                };
                let request = into_open_ai(
                    request,
                    model.id(),
                    model.supports_parallel_tool_calls(),
                    None,
                );
                let llm_api_token = self.llm_api_token.clone();
                let future = self.request_limiter.stream(async move {
                    let PerformLlmCompletionResponse {
                        response,
                        usage,
                        includes_status_messages,
                        tool_use_limit_reached,
                    } = Self::perform_llm_completion(
                        client.clone(),
                        llm_api_token,
                        app_version,
                        CompletionBody {
                            thread_id,
                            prompt_id,
                            intent,
                            mode,
                            provider: cloud_llm_client::LanguageModelProvider::OpenAi,
                            model: request.model.clone(),
                            provider_request: serde_json::to_value(&request)
                                .map_err(|e| anyhow!(e))?,
                        },
                    )
                    .await?;

                    let mut mapper = OpenAiEventMapper::new();
                    Ok(map_cloud_completion_events(
                        Box::pin(
                            response_lines(response, includes_status_messages)
                                .chain(usage_updated_event(usage))
                                .chain(tool_use_limit_reached_event(tool_use_limit_reached)),
                        ),
                        move |event| mapper.map_event(event),
                    ))
                });
                async move { Ok(future.await?.boxed()) }.boxed()
            }
            cloud_llm_client::LanguageModelProvider::Google => {
                let client = self.client.clone();
                let request =
                    into_google(request, self.model.id.to_string(), GoogleModelMode::Default);
                let llm_api_token = self.llm_api_token.clone();
                let future = self.request_limiter.stream(async move {
                    let PerformLlmCompletionResponse {
                        response,
                        usage,
                        includes_status_messages,
                        tool_use_limit_reached,
                    } = Self::perform_llm_completion(
                        client.clone(),
                        llm_api_token,
                        app_version,
                        CompletionBody {
                            thread_id,
                            prompt_id,
                            intent,
                            mode,
                            provider: cloud_llm_client::LanguageModelProvider::Google,
                            model: request.model.model_id.clone(),
                            provider_request: serde_json::to_value(&request)
                                .map_err(|e| anyhow!(e))?,
                        },
                    )
                    .await?;

                    let mut mapper = GoogleEventMapper::new();
                    Ok(map_cloud_completion_events(
                        Box::pin(
                            response_lines(response, includes_status_messages)
                                .chain(usage_updated_event(usage))
                                .chain(tool_use_limit_reached_event(tool_use_limit_reached)),
                        ),
                        move |event| mapper.map_event(event),
                    ))
                });
                async move { Ok(future.await?.boxed()) }.boxed()
            }
        }
    }
}

fn map_cloud_completion_events<T, F>(
    stream: Pin<Box<dyn Stream<Item = Result<CompletionEvent<T>>> + Send>>,
    mut map_callback: F,
) -> BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>
where
    T: DeserializeOwned + 'static,
    F: FnMut(T) -> Vec<Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>
        + Send
        + 'static,
{
    stream
        .flat_map(move |event| {
            futures::stream::iter(match event {
                Err(error) => {
                    vec![Err(LanguageModelCompletionError::from(error))]
                }
                Ok(CompletionEvent::Status(event)) => {
                    vec![Ok(LanguageModelCompletionEvent::StatusUpdate(event))]
                }
                Ok(CompletionEvent::Event(event)) => map_callback(event),
            })
        })
        .boxed()
}

fn usage_updated_event<T>(
    usage: Option<ModelRequestUsage>,
) -> impl Stream<Item = Result<CompletionEvent<T>>> {
    futures::stream::iter(usage.map(|usage| {
        Ok(CompletionEvent::Status(
            CompletionRequestStatus::UsageUpdated {
                amount: usage.amount as usize,
                limit: usage.limit,
            },
        ))
    }))
}

fn tool_use_limit_reached_event<T>(
    tool_use_limit_reached: bool,
) -> impl Stream<Item = Result<CompletionEvent<T>>> {
    futures::stream::iter(tool_use_limit_reached.then(|| {
        Ok(CompletionEvent::Status(
            CompletionRequestStatus::ToolUseLimitReached,
        ))
    }))
}

fn response_lines<T: DeserializeOwned>(
    response: Response<AsyncBody>,
    includes_status_messages: bool,
) -> impl Stream<Item = Result<CompletionEvent<T>>> {
    futures::stream::try_unfold(
        (String::new(), BufReader::new(response.into_body())),
        move |(mut line, mut body)| async move {
            match body.read_line(&mut line).await {
                Ok(0) => Ok(None),
                Ok(_) => {
                    let event = if includes_status_messages {
                        serde_json::from_str::<CompletionEvent<T>>(&line)?
                    } else {
                        CompletionEvent::Event(serde_json::from_str::<T>(&line)?)
                    };

                    line.clear();
                    Ok(Some((event, (line, body))))
                }
                Err(e) => Err(e.into()),
            }
        },
    )
}

#[derive(IntoElement, RegisterComponent)]
struct ZedAiConfiguration {
    is_connected: bool,
    plan: Option<Plan>,
    subscription_period: Option<(DateTime<Utc>, DateTime<Utc>)>,
    eligible_for_trial: bool,
    has_accepted_terms_of_service: bool,
    account_too_young: bool,
    accept_terms_of_service_in_progress: bool,
    accept_terms_of_service_callback: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    sign_in_callback: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
}

impl RenderOnce for ZedAiConfiguration {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let young_account_banner = YoungAccountBanner;

        let is_pro = self.plan == Some(Plan::ZedPro);
        let subscription_text = match (self.plan, self.subscription_period) {
            (Some(Plan::ZedPro), Some(_)) => {
                "You have access to Zed's hosted models through your Pro subscription."
            }
            (Some(Plan::ZedProTrial), Some(_)) => {
                "You have access to Zed's hosted models through your Pro trial."
            }
            (Some(Plan::ZedFree), Some(_)) => {
                "You have basic access to Zed's hosted models through the Free plan."
            }
            _ => {
                if self.eligible_for_trial {
                    "Subscribe for access to Zed's hosted models. Start with a 14 day free trial."
                } else {
                    "Subscribe for access to Zed's hosted models."
                }
            }
        };

        let manage_subscription_buttons = if is_pro {
            Button::new("manage_settings", "Manage Subscription")
                .full_width()
                .style(ButtonStyle::Tinted(TintColor::Accent))
                .on_click(|_, _, cx| cx.open_url(&zed_urls::account_url(cx)))
                .into_any_element()
        } else if self.plan.is_none() || self.eligible_for_trial {
            Button::new("start_trial", "Start 14-day Free Pro Trial")
                .full_width()
                .style(ui::ButtonStyle::Tinted(ui::TintColor::Accent))
                .on_click(|_, _, cx| cx.open_url(&zed_urls::start_trial_url(cx)))
                .into_any_element()
        } else {
            Button::new("upgrade", "Upgrade to Pro")
                .full_width()
                .style(ui::ButtonStyle::Tinted(ui::TintColor::Accent))
                .on_click(|_, _, cx| cx.open_url(&zed_urls::upgrade_to_zed_pro_url(cx)))
                .into_any_element()
        };

        if !self.is_connected {
            return v_flex()
                .gap_2()
                .child(Label::new("Sign in to have access to Zed's complete agentic experience with hosted models."))
                .child(
                    Button::new("sign_in", "Sign In to use Zed AI")
                        .icon_color(Color::Muted)
                        .icon(IconName::Github)
                        .icon_size(IconSize::Small)
                        .icon_position(IconPosition::Start)
                        .full_width()
                        .on_click({
                            let callback = self.sign_in_callback.clone();
                            move |_, window, cx| (callback)(window, cx)
                        }),
                );
        }

        v_flex()
            .gap_2()
            .w_full()
            .when(!self.has_accepted_terms_of_service, |this| {
                this.child(render_accept_terms(
                    LanguageModelProviderTosView::Configuration,
                    self.accept_terms_of_service_in_progress,
                    {
                        let callback = self.accept_terms_of_service_callback.clone();
                        move |window, cx| (callback)(window, cx)
                    },
                ))
            })
            .map(|this| {
                if self.has_accepted_terms_of_service && self.account_too_young {
                    this.child(young_account_banner).child(
                        Button::new("upgrade", "Upgrade to Pro")
                            .style(ui::ButtonStyle::Tinted(ui::TintColor::Accent))
                            .full_width()
                            .on_click(|_, _, cx| {
                                cx.open_url(&zed_urls::upgrade_to_zed_pro_url(cx))
                            }),
                    )
                } else if self.has_accepted_terms_of_service {
                    this.text_sm()
                        .child(subscription_text)
                        .child(manage_subscription_buttons)
                } else {
                    this
                }
            })
            .when(self.has_accepted_terms_of_service, |this| this)
    }
}

struct ConfigurationView {
    state: Entity<State>,
    accept_terms_of_service_callback: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    sign_in_callback: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
}

impl ConfigurationView {
    fn new(state: Entity<State>) -> Self {
        let accept_terms_of_service_callback = Arc::new({
            let state = state.clone();
            move |_window: &mut Window, cx: &mut App| {
                state.update(cx, |state, cx| {
                    state.accept_terms_of_service(cx);
                });
            }
        });

        let sign_in_callback = Arc::new({
            let state = state.clone();
            move |_window: &mut Window, cx: &mut App| {
                state.update(cx, |state, cx| {
                    state.authenticate(cx).detach_and_log_err(cx);
                });
            }
        });

        Self {
            state,
            accept_terms_of_service_callback,
            sign_in_callback,
        }
    }
}

impl Render for ConfigurationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let cloud_user_store = state.cloud_user_store.read(cx);

        ZedAiConfiguration {
            is_connected: !state.is_signed_out(cx),
            plan: cloud_user_store.plan(),
            subscription_period: cloud_user_store.subscription_period(),
            eligible_for_trial: cloud_user_store.trial_started_at().is_none(),
            has_accepted_terms_of_service: state.has_accepted_terms_of_service(cx),
            account_too_young: cloud_user_store.account_too_young(),
            accept_terms_of_service_in_progress: state.accept_terms_of_service_task.is_some(),
            accept_terms_of_service_callback: self.accept_terms_of_service_callback.clone(),
            sign_in_callback: self.sign_in_callback.clone(),
        }
    }
}

impl Component for ZedAiConfiguration {
    fn scope() -> ComponentScope {
        ComponentScope::Agent
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> Option<AnyElement> {
        fn configuration(
            is_connected: bool,
            plan: Option<Plan>,
            eligible_for_trial: bool,
            account_too_young: bool,
            has_accepted_terms_of_service: bool,
        ) -> AnyElement {
            ZedAiConfiguration {
                is_connected,
                plan,
                subscription_period: plan
                    .is_some()
                    .then(|| (Utc::now(), Utc::now() + chrono::Duration::days(7))),
                eligible_for_trial,
                has_accepted_terms_of_service,
                account_too_young,
                accept_terms_of_service_in_progress: false,
                accept_terms_of_service_callback: Arc::new(|_, _| {}),
                sign_in_callback: Arc::new(|_, _| {}),
            }
            .into_any_element()
        }

        Some(
            v_flex()
                .p_4()
                .gap_4()
                .children(vec![
                    single_example(
                        "Not connected",
                        configuration(false, None, false, false, true),
                    ),
                    single_example(
                        "Accept Terms of Service",
                        configuration(true, None, true, false, false),
                    ),
                    single_example(
                        "No Plan - Not eligible for trial",
                        configuration(true, None, false, false, true),
                    ),
                    single_example(
                        "No Plan - Eligible for trial",
                        configuration(true, None, true, false, true),
                    ),
                    single_example(
                        "Free Plan",
                        configuration(true, Some(Plan::ZedFree), true, false, true),
                    ),
                    single_example(
                        "Zed Pro Trial Plan",
                        configuration(true, Some(Plan::ZedProTrial), true, false, true),
                    ),
                    single_example(
                        "Zed Pro Plan",
                        configuration(true, Some(Plan::ZedPro), true, false, true),
                    ),
                ])
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::http::{HeaderMap, StatusCode};
    use language_model::LanguageModelCompletionError;

    #[test]
    fn test_api_error_conversion_with_upstream_http_error() {
        // upstream_http_error with 503 status should become ServerOverloaded
        let error_body = r#"{"code":"upstream_http_error","message":"Received an error from the Anthropic API: upstream connect error or disconnect/reset before headers, reset reason: connection timeout","upstream_status":503}"#;

        let api_error = ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: error_body.to_string(),
            headers: HeaderMap::new(),
        };

        let completion_error: LanguageModelCompletionError = api_error.into();

        match completion_error {
            LanguageModelCompletionError::UpstreamProviderError { message, .. } => {
                assert_eq!(
                    message,
                    "Received an error from the Anthropic API: upstream connect error or disconnect/reset before headers, reset reason: connection timeout"
                );
            }
            _ => panic!(
                "Expected UpstreamProviderError for upstream 503, got: {:?}",
                completion_error
            ),
        }

        // upstream_http_error with 500 status should become ApiInternalServerError
        let error_body = r#"{"code":"upstream_http_error","message":"Received an error from the OpenAI API: internal server error","upstream_status":500}"#;

        let api_error = ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: error_body.to_string(),
            headers: HeaderMap::new(),
        };

        let completion_error: LanguageModelCompletionError = api_error.into();

        match completion_error {
            LanguageModelCompletionError::UpstreamProviderError { message, .. } => {
                assert_eq!(
                    message,
                    "Received an error from the OpenAI API: internal server error"
                );
            }
            _ => panic!(
                "Expected UpstreamProviderError for upstream 500, got: {:?}",
                completion_error
            ),
        }

        // upstream_http_error with 429 status should become RateLimitExceeded
        let error_body = r#"{"code":"upstream_http_error","message":"Received an error from the Google API: rate limit exceeded","upstream_status":429}"#;

        let api_error = ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: error_body.to_string(),
            headers: HeaderMap::new(),
        };

        let completion_error: LanguageModelCompletionError = api_error.into();

        match completion_error {
            LanguageModelCompletionError::UpstreamProviderError { message, .. } => {
                assert_eq!(
                    message,
                    "Received an error from the Google API: rate limit exceeded"
                );
            }
            _ => panic!(
                "Expected UpstreamProviderError for upstream 429, got: {:?}",
                completion_error
            ),
        }

        // Regular 500 error without upstream_http_error should remain ApiInternalServerError for Zed
        let error_body = "Regular internal server error";

        let api_error = ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: error_body.to_string(),
            headers: HeaderMap::new(),
        };

        let completion_error: LanguageModelCompletionError = api_error.into();

        match completion_error {
            LanguageModelCompletionError::ApiInternalServerError { provider, message } => {
                assert_eq!(provider, PROVIDER_NAME);
                assert_eq!(message, "Regular internal server error");
            }
            _ => panic!(
                "Expected ApiInternalServerError for regular 500, got: {:?}",
                completion_error
            ),
        }

        // upstream_http_429 format should be converted to UpstreamProviderError
        let error_body = r#"{"code":"upstream_http_429","message":"Upstream Anthropic rate limit exceeded.","retry_after":30.5}"#;

        let api_error = ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: error_body.to_string(),
            headers: HeaderMap::new(),
        };

        let completion_error: LanguageModelCompletionError = api_error.into();

        match completion_error {
            LanguageModelCompletionError::UpstreamProviderError {
                message,
                status,
                retry_after,
            } => {
                assert_eq!(message, "Upstream Anthropic rate limit exceeded.");
                assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
                assert_eq!(retry_after, Some(Duration::from_secs_f64(30.5)));
            }
            _ => panic!(
                "Expected UpstreamProviderError for upstream_http_429, got: {:?}",
                completion_error
            ),
        }

        // Invalid JSON in error body should fall back to regular error handling
        let error_body = "Not JSON at all";

        let api_error = ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: error_body.to_string(),
            headers: HeaderMap::new(),
        };

        let completion_error: LanguageModelCompletionError = api_error.into();

        match completion_error {
            LanguageModelCompletionError::ApiInternalServerError { provider, .. } => {
                assert_eq!(provider, PROVIDER_NAME);
            }
            _ => panic!(
                "Expected ApiInternalServerError for invalid JSON, got: {:?}",
                completion_error
            ),
        }
    }
}
