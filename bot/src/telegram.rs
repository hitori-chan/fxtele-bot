//! Talking to the Bot API: the HTTP client, long polling, startup setup and running actions.

use std::time::Duration;

use anyhow::{Context, Result};
use frankenstein::AsyncTelegramApi;
use frankenstein::client_reqwest::Bot;
use frankenstein::methods::{
    DeleteMyCommandsParams, DeleteWebhookParams, GetUpdatesParams, LeaveChatParams, SetMyCommandsParams,
};
use frankenstein::response::MethodResponse;
use frankenstein::types::{AllowedUpdate, BotCommand, BotCommandScope, BotCommandScopeChat, User};
use frankenstein::updates::Update;
use tracing::{debug, warn};

use crate::app::Action;
use crate::menu::COMMANDS;

/// How long one `getUpdates` call waits for news.
pub const POLL_SECONDS: u32 = 50;

pub struct Telegram {
    bot: Bot,
}

impl Telegram {
    pub fn new(token: &str) -> Result<Self> {
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|der| reqwest::Certificate::from_der(der))
            .collect::<Result<Vec<_>, _>>()?;
        let client = reqwest::Client::builder()
            .tls_certs_only(roots)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(u64::from(POLL_SECONDS) + 25))
            .pool_idle_timeout(Duration::from_secs(120))
            .build()
            .context("can't build the HTTP client")?;
        let bot = Bot::builder().api_url(format!("{}{token}", frankenstein::BASE_API_URL)).client(client).build();
        Ok(Self { bot })
    }

    pub async fn get_me(&self) -> Result<User, frankenstein::Error> {
        Ok(self.bot.get_me().await?.result)
    }

    /// Switches from webhooks to polling, and shows the command menu to the owner only.
    pub async fn setup(&self, owner: i64) {
        let webhook = DeleteWebhookParams::builder().drop_pending_updates(false).build();
        if let Err(e) = self.bot.delete_webhook(&webhook).await {
            warn!("can't remove the webhook: {}", describe(&e));
        }
        // Clear menus set for everyone (the old bot set some), then set the owner's.
        for scope in [
            BotCommandScope::Default,
            BotCommandScope::AllPrivateChats,
            BotCommandScope::AllGroupChats,
            BotCommandScope::AllChatAdministrators,
        ] {
            let params = DeleteMyCommandsParams::builder().scope(scope).build();
            if let Err(e) = self.bot.delete_my_commands(&params).await {
                warn!("can't clear a command menu: {}", describe(&e));
            }
        }
        let commands = COMMANDS
            .iter()
            .map(|(command, description)| BotCommand::builder().command(*command).description(*description).build())
            .collect();
        let scope = BotCommandScope::Chat(BotCommandScopeChat::builder().chat_id(owner).build());
        let params = SetMyCommandsParams::builder().commands(commands).scope(scope).build();
        if let Err(e) = self.bot.set_my_commands(&params).await {
            warn!("can't set the owner's command menu (has the owner started the bot?): {}", describe(&e));
        }
    }

    /// Long-polls for updates after `offset`, which also confirms everything before it.
    /// Each update is parsed on its own, so one Telegram can't parse doesn't block the rest.
    /// The next updates. The error is Telegram's own, so a revoked token can be told apart.
    pub async fn updates(
        &self,
        offset: Option<i64>,
        timeout: u32,
    ) -> Result<Vec<(i64, Option<Update>)>, frankenstein::Error> {
        let params = GetUpdatesParams::builder()
            .maybe_offset(offset)
            .timeout(timeout)
            .allowed_updates(vec![AllowedUpdate::Message, AllowedUpdate::MyChatMember, AllowedUpdate::CallbackQuery])
            .build();
        let response: MethodResponse<Vec<serde_json::Value>> = self.bot.request("getUpdates", Some(&params)).await?;
        Ok(response
            .result
            .into_iter()
            .filter_map(|raw| {
                let id = raw.get("update_id")?.as_i64()?;
                let update = serde_json::from_value(raw)
                    // Only the kind of error: serde's message can quote a value, which may be text.
                    .inspect_err(|e| warn!(update = id, kind = ?e.classify(), "skipping an update Telegram sent in an unknown shape"))
                    .ok();
                Some((id, update))
            })
            .collect())
    }

    /// Runs one action, waiting once if Telegram asks us to slow down. Whether it went through.
    pub async fn run(&self, action: &Action) -> bool {
        for attempt in 0..2 {
            // Telegram's echo of a sent or edited message isn't needed: left unread, it can't fail to
            // parse after the message went out, and the text it quotes isn't decoded.
            let result = match action {
                Action::Send(params) | Action::Ask(_, params) => {
                    self.bot.request::<_, Ignored>("sendMessage", Some(params)).await.map(drop)
                }
                Action::Edit(params) => self.bot.request::<_, Ignored>("editMessageText", Some(params)).await.map(drop),
                Action::Answer(params) => self.bot.answer_callback_query(params).await.map(drop),
                Action::Leave(chat) => {
                    self.bot.leave_chat(&LeaveChatParams::builder().chat_id(*chat).build()).await.map(drop)
                }
            };
            let Err(e) = result else { return true };
            if let frankenstein::Error::Api(response) = &e {
                if response.description.contains("message is not modified") {
                    return true;
                }
                if let Some(wait) = response.parameters.and_then(|p| p.retry_after)
                    && attempt == 0
                    && wait <= 60
                {
                    debug!(wait, "Telegram asked to wait before retrying");
                    tokio::time::sleep(Duration::from_secs(wait.into())).await;
                    continue;
                }
            }
            warn!(action = action.name(), chat = action.chat(), "Telegram refused: {}", describe(&e));
            return false;
        }
        false
    }
}

impl Action {
    const fn name(&self) -> &'static str {
        match self {
            Self::Send(_) => "send",
            Self::Ask(..) => "ask the owner",
            Self::Edit(_) => "edit",
            Self::Answer(_) => "answer",
            Self::Leave(_) => "leave",
        }
    }

    fn chat(&self) -> Option<i64> {
        let id = match self {
            Self::Send(p) | Self::Ask(_, p) => p.chat_id.as_ref(),
            Self::Edit(p) => p.chat_id.as_ref(),
            Self::Answer(_) => None,
            Self::Leave(chat) => return Some(*chat),
        };
        match id? {
            frankenstein::types::ChatId::Integer(id) => Some(*id),
            frankenstein::types::ChatId::String(_) => None,
        }
    }
}

/// A response whose result is skipped unread.
type Ignored = MethodResponse<serde::de::IgnoredAny>;

/// Describes an error without the response body, which can contain message text.
pub fn describe(error: &frankenstein::Error) -> String {
    match error {
        frankenstein::Error::Api(response) => format!("{} (code {})", response.description, response.error_code),
        // frankenstein drops the URL, which holds the token; the causes say what went wrong
        // (DNS, TLS, a timeout) and name no URL.
        frankenstein::Error::HttpReqwest(e) => {
            let mut text = format!("network error: {e}");
            let mut cause = std::error::Error::source(e);
            while let Some(e) = cause {
                text.push_str(": ");
                text.push_str(&e.to_string());
                cause = e.source();
            }
            text
        }
        // Only where and what kind: serde's message can quote a value, which may be text.
        frankenstein::Error::JsonDecode { source, .. } => {
            format!(
                "unreadable response ({:?} at line {}, column {})",
                source.classify(),
                source.line(),
                source.column()
            )
        }
        frankenstein::Error::JsonEncode { source, .. } => format!("can't encode the request: {source}"),
        other => other.to_string(),
    }
}

/// Whether the error means the token is wrong, so retrying is pointless.
pub fn is_bad_token(error: &frankenstein::Error) -> bool {
    matches!(error, frankenstein::Error::Api(r) if r.error_code == 401 || r.error_code == 404)
}
