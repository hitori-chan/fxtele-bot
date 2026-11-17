//! Turning updates into actions. Nothing here talks to Telegram, so it's all testable.

use std::collections::HashMap;
use std::time::Instant;

use frankenstein::methods::{AnswerCallbackQueryParams, EditMessageTextParams, SendMessageParams};
use frankenstein::types::{ChatMember, ChatMemberUpdated, ChatType, LinkPreviewOptions, Message, ReplyParameters};
use frankenstein::updates::UpdateContent;
use tracing::{debug, info, warn};

use crate::access::{Access, Added, Name, Removed};
use crate::config::Config;
use crate::limits::Limits;
use crate::links::{Domains, swapped_links};

/// Telegram's limit for a message's text, in UTF-16 code units.
const MAX_TEXT: usize = 4096;
/// Groups waiting for the owner's answer at most: anyone can add the bot to groups, and each would
/// otherwise send the owner a prompt.
const MAX_PENDING: usize = 20;

/// What the bot should do in response to an update.
#[derive(Debug, PartialEq)]
pub enum Action {
    Send(SendMessageParams),
    Edit(EditMessageTextParams),
    Answer(AnswerCallbackQueryParams),
    Leave(i64),
}

pub struct App {
    /// The bot's @name, for commands addressed to it (`/menu@bot`).
    pub(crate) username: String,
    pub(crate) access: Access,
    domains: Domains,
    limits: Limits,
    max_links: usize,
    /// Groups someone else added the bot to, waiting for the owner: ID → title.
    pub(crate) pending: HashMap<i64, String>,
    /// Waiting groups that became supergroups: old ID → new, as the owner's prompt names the old.
    pub(crate) moved: HashMap<i64, i64>,
}

impl App {
    pub fn new(config: &Config, access: Access, username: String) -> Self {
        Self {
            username,
            access,
            domains: Domains::new(config.domains.clone()),
            limits: Limits::new(config.user_replies_per_minute, config.chat_replies_per_minute),
            max_links: config.max_links_per_message,
            pending: HashMap::new(),
            moved: HashMap::new(),
        }
    }

    pub fn handle(&mut self, update: UpdateContent, now: Instant) -> Vec<Action> {
        match update {
            UpdateContent::Message(message) => self.on_message(&message, now),
            UpdateContent::MyChatMember(update) => self.on_membership(&update),
            UpdateContent::CallbackQuery(query) => self.on_callback(&query),
            _ => Vec::new(),
        }
    }

    fn on_message(&mut self, message: &Message, now: Instant) -> Vec<Action> {
        let chat = &message.chat;
        if let Some(to) = message.migrate_to_chat_id {
            self.on_migration(chat.id, to);
            return Vec::new();
        }
        if let Some(from) = message.migrate_from_chat_id {
            self.on_migration(from, chat.id);
            return Vec::new();
        }

        self.remember_names(message);

        // A forwarded message is someone else's words: never a command, however it reads.
        let from_owner = message.from.as_deref().is_some_and(|user| self.access.is_owner(user));
        if from_owner
            && chat.type_field == ChatType::Private
            && message.forward_origin.is_none()
            && let Some(actions) = self.owner_message(message)
        {
            return actions;
        }

        if !self.access.allows(chat) {
            return Vec::new();
        }
        // Skip messages bots sent. Messages sent on behalf of a channel or an anonymous admin carry
        // `sender_chat` and come from a placeholder bot account; those count.
        let Some(from) = message.from.as_deref() else { return Vec::new() };
        if message.sender_chat.is_none() && from.is_bot {
            return Vec::new();
        }

        let links = swapped_links(message, &self.domains, self.max_links);
        let Some(reply) = reply(message, &links) else { return Vec::new() };
        let sender = message.sender_chat.as_ref().map_or_else(|| i64::try_from(from.id).unwrap_or(i64::MAX), |c| c.id);
        if let Err(limited) = self.limits.acquire(sender, chat.id, now) {
            info!(chat = chat.id, sender, ?limited, "rate limited, not replying");
            return Vec::new();
        }
        info!(chat = chat.id, links = links.len(), forwarded = message.forward_origin.is_some(), "replying");
        vec![Action::Send(reply)]
    }

    fn on_migration(&mut self, from: i64, to: i64) {
        if let Some(title) = self.pending.remove(&from) {
            info!(from, to, "a group waiting for the owner became a supergroup");
            self.pending.insert(to, title);
            self.moved.insert(from, to);
        }
        if self.access.migrate(from, to) {
            info!(from, to, "group became a supergroup; approval moved to the new ID");
        } else if self.access.is_permanent(from) {
            warn!(from, to, "a group in allowed_chats became a supergroup; replace {from} with {to} in bot.toml");
        }
    }

    /// Keeps allowed people's and groups' names current, for the owner's list.
    fn remember_names(&mut self, message: &Message) {
        let chat = &message.chat;
        if matches!(chat.type_field, ChatType::Group | ChatType::Supergroup)
            && self.access.allows_id(chat.id)
            && let Some(name) = Name::of_chat(chat)
        {
            self.access.remember(chat.id, name);
        }
        if let Some(user) = message.from.as_deref()
            && !user.is_bot
            && let Ok(id) = i64::try_from(user.id)
            && self.access.allows_id(id)
        {
            self.access.remember(id, Name::of_user(user));
        }
    }

    fn on_membership(&mut self, update: &ChatMemberUpdated) -> Vec<Action> {
        let chat = &update.chat;
        if !matches!(chat.type_field, ChatType::Group | ChatType::Supergroup) {
            return Vec::new();
        }
        let title = chat.title.clone().unwrap_or_else(|| chat.id.to_string());
        let was_in = is_member(&update.old_chat_member);
        let is_in = is_member(&update.new_chat_member);

        if !was_in && is_in {
            if self.access.is_owner(&update.from) {
                self.pending.remove(&chat.id);
                match self.access.add(chat.id, Name::of_chat(chat)) {
                    Added::New => info!(chat = chat.id, "added to a group by the owner; approved"),
                    Added::Already => info!(chat = chat.id, "added to an allowed group by the owner"),
                }
                return Vec::new();
            }
            if self.access.allows(chat) {
                info!(chat = chat.id, "added to an allowed group");
                return Vec::new();
            }
            // One prompt a group until the owner answers: adding and removing the bot over and
            // over mustn't flood the owner.
            if self.pending.contains_key(&chat.id) {
                info!(chat = chat.id, "added again to a group that waits for the owner; not asking twice");
                return Vec::new();
            }
            if self.pending.len() >= MAX_PENDING {
                warn!(
                    chat = chat.id,
                    "added to a group by someone else; {MAX_PENDING} groups already wait for the owner, so not asking"
                );
                return Vec::new();
            }
            info!(chat = chat.id, "added to a group by someone else; asking the owner");
            self.pending.insert(chat.id, title.clone());
            return vec![self.approval_prompt(chat.id, &title, &Name::of_user(&update.from))];
        }

        // A waiting group stays waiting: its prompt still works, and asks nothing new.
        if was_in && !is_in {
            match self.access.remove(chat.id) {
                Removed::Done => info!(chat = chat.id, "removed from an approved group; approval dropped"),
                Removed::Permanent => info!(chat = chat.id, "removed from a group in allowed_chats"),
                Removed::NotFound => debug!(chat = chat.id, "removed from a group"),
            }
        }
        Vec::new()
    }
}

fn is_member(member: &ChatMember) -> bool {
    match member {
        ChatMember::Creator(_) | ChatMember::Administrator(_) | ChatMember::Member(_) => true,
        ChatMember::Restricted(restricted) => restricted.is_member,
        ChatMember::Left(_) | ChatMember::Kicked(_) => false,
    }
}

/// The reply: swapped links one per line, previewing the first with large media. Links that
/// don't fit Telegram's limit are left out; with none that fits, there's no reply.
fn reply(message: &Message, links: &[String]) -> Option<SendMessageParams> {
    let mut text = String::new();
    let mut units = 0;
    let mut first = None;
    for link in links {
        let cost = link.encode_utf16().count() + usize::from(!text.is_empty());
        if units + cost > MAX_TEXT {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(link);
        units += cost;
        first.get_or_insert(link);
    }
    let preview = LinkPreviewOptions::builder().url(first?.clone()).prefer_large_media(true).build();
    let reply_to = ReplyParameters::builder().message_id(message.message_id).allow_sending_without_reply(true).build();
    let params = SendMessageParams::builder()
        .chat_id(message.chat.id)
        .maybe_message_thread_id(
            message.is_topic_message.unwrap_or(false).then_some(message.message_thread_id).flatten(),
        )
        .text(text)
        .link_preview_options(preview)
        .reply_parameters(reply_to)
        .build();
    Some(params)
}

#[cfg(test)]
pub mod tests {
    use std::path::Path;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::access::tests::config;

    pub const OWNER: i64 = 1;
    pub const BOT_ID: u64 = 999;

    pub fn app(dir: &Path) -> App {
        let mut config = config(dir);
        config.domains = [("x.com".to_string(), "fixupx.com".to_string())].into();
        config.user_replies_per_minute = 2;
        config.chat_replies_per_minute = 3;
        let access = Access::load(&config).unwrap();
        App::new(&config, access, "fixbot".into())
    }

    pub fn user(id: i64) -> Value {
        json!({ "id": id, "is_bot": false, "first_name": format!("User{id}") })
    }

    pub fn chat(id: i64) -> Value {
        if id > 0 {
            json!({ "id": id, "type": "private", "first_name": format!("User{id}") })
        } else {
            json!({ "id": id, "type": "supergroup", "title": format!("Group{id}") })
        }
    }

    /// A message with one `url` entity per whitespace-separated word that looks like a link.
    #[allow(clippy::needless_pass_by_value)]
    pub fn message(chat_id: i64, from: Value, text: &str) -> Value {
        let mut entities = Vec::new();
        let mut offset = 0;
        for word in text.split(' ') {
            if word.contains('.') {
                entities.push(json!({ "type": "url", "offset": offset, "length": word.len() }));
            }
            offset += word.len() + 1;
        }
        json!({
            "message_id": 7, "date": 0, "chat": chat(chat_id), "from": from,
            "text": text, "entities": entities,
        })
    }

    pub fn run(app: &mut App, update: Value) -> Vec<Action> {
        run_at(app, update, Instant::now())
    }

    pub fn run_at(app: &mut App, update: Value, now: Instant) -> Vec<Action> {
        let mut update = update;
        update["update_id"] = json!(1);
        let update: frankenstein::updates::Update = serde_json::from_value(update).unwrap();
        app.handle(update.content, now)
    }

    pub fn sent_text(actions: &[Action]) -> Vec<&str> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send(p) => Some(p.text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn replies_in_allowed_chats_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let text = "look https://x.com/a/status/1";
        for chat in [OWNER, 10, -100] {
            let actions = run(&mut app, json!({ "message": message(chat, user(chat.max(5)), text) }));
            assert_eq!(sent_text(&actions), ["https://fixupx.com/a/status/1"], "chat {chat}");
        }
        for chat in [11, -101] {
            assert!(run(&mut app, json!({ "message": message(chat, user(5), text) })).is_empty());
        }
        let other = json!({ "message": message(-100, user(6), "https://other.example/a hi") });
        assert!(run(&mut app, other).is_empty(), "no links to swap, no reply");
    }

    #[test]
    fn reply_shape() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let mut msg = message(-100, user(5), "https://x.com/1 https://x.com/2 https://other.example/");
        msg["is_topic_message"] = json!(true);
        msg["message_thread_id"] = json!(42);
        let actions = run(&mut app, json!({ "message": msg }));
        let [Action::Send(params)] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(params.text, "https://fixupx.com/1\nhttps://fixupx.com/2");
        assert_eq!(params.message_thread_id, Some(42));
        let preview = params.link_preview_options.as_ref().unwrap();
        assert_eq!(preview.url.as_deref(), Some("https://fixupx.com/1"));
        assert_eq!(preview.prefer_large_media, Some(true));
        let reply = params.reply_parameters.as_ref().unwrap();
        assert_eq!((reply.message_id, reply.allow_sending_without_reply), (Some(7), Some(true)));
    }

    #[test]
    fn forwarded_messages() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let bot = json!({ "id": 77, "is_bot": true, "first_name": "OtherBot" });
        let channel = json!({ "id": -1_000_000_000_555_i64, "type": "channel", "title": "News" });
        let origins = [
            json!({ "type": "user", "date": 0, "sender_user": user(8) }),
            json!({ "type": "user", "date": 0, "sender_user": bot }),
            json!({ "type": "channel", "date": 0, "chat": channel, "message_id": 3 }),
        ];
        // A minute apart, so the test's low rate limits don't interfere.
        let mut at = Instant::now();
        let mut later = || {
            at += Duration::from_secs(60);
            at
        };
        for origin in origins {
            let mut msg = message(-100, user(5), "https://x.com/f");
            msg["forward_origin"] = origin;
            assert_eq!(sent_text(&run_at(&mut app, json!({ "message": msg }), later())), ["https://fixupx.com/f"]);
        }

        // Channel post auto-forwarded into its discussion group: from the "Telegram" account, sender_chat is the channel.
        let mut msg =
            message(-100, json!({ "id": 777_000, "is_bot": false, "first_name": "Telegram" }), "https://x.com/c");
        msg["is_automatic_forward"] = json!(true);
        msg["sender_chat"] = channel.clone();
        msg["forward_origin"] = json!({ "type": "channel", "date": 0, "chat": channel, "message_id": 4 });
        assert_eq!(sent_text(&run_at(&mut app, json!({ "message": msg }), later())), ["https://fixupx.com/c"]);
    }

    #[test]
    fn skips_messages_sent_by_bots() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let bot = json!({ "id": 77, "is_bot": true, "first_name": "OtherBot" });
        assert!(run(&mut app, json!({ "message": message(-100, bot.clone(), "https://x.com/b") })).is_empty());

        // A bot forwarding someone's message is still a bot sending it here.
        let mut msg = message(-100, bot, "https://x.com/b");
        msg["forward_origin"] = json!({ "type": "user", "date": 0, "sender_user": user(8) });
        assert!(run(&mut app, json!({ "message": msg })).is_empty());

        // An anonymous admin posts through a placeholder bot account but carries sender_chat.
        let mut msg =
            message(-100, json!({ "id": 1_087_968_824, "is_bot": true, "first_name": "Group" }), "https://x.com/b");
        msg["sender_chat"] = chat(-100);
        assert_eq!(sent_text(&run(&mut app, json!({ "message": msg }))).len(), 1);
    }

    #[test]
    fn rate_limits_per_user_and_chat() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let t = Instant::now();
        let send = |app: &mut App, from: i64, at: Instant| {
            !run_at(app, json!({ "message": message(-100, user(from), "https://x.com/r") }), at).is_empty()
        };
        assert!(send(&mut app, 5, t));
        assert!(send(&mut app, 5, t));
        assert!(!send(&mut app, 5, t), "user limit is 2");
        assert!(send(&mut app, 6, t));
        assert!(!send(&mut app, 7, t), "chat limit is 3");
        assert!(send(&mut app, 7, t + Duration::from_secs(20)), "the chat budget refills");
    }

    #[test]
    fn owner_adds_bot_to_group_approves_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let joined = |from: Value, chat_id: i64| {
            json!({ "my_chat_member": {
                "chat": chat(chat_id), "from": from, "date": 0,
                "old_chat_member": { "status": "left", "user": { "id": BOT_ID, "is_bot": true, "first_name": "B" } },
                "new_chat_member": { "status": "member", "user": { "id": BOT_ID, "is_bot": true, "first_name": "B" } },
            }})
        };
        assert!(run(&mut app, joined(user(OWNER), -200)).is_empty());
        assert!(app.access.allows(&crate::access::tests::chat(-200, ChatType::Supergroup)));
        assert_eq!(app.access.name(-200).unwrap().name, "Group-200");

        // Someone else: the owner is asked, and the group stays unapproved.
        let actions = run(&mut app, joined(user(5), -300));
        let [Action::Send(prompt)] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(prompt.chat_id, Some(OWNER.into()));
        assert!(prompt.text.contains("Group-300"));
        assert!(run(&mut app, json!({ "message": message(-300, user(5), "https://x.com/a") })).is_empty());
        // Removed and added again while it waits: still one prompt.
        let removed = json!({ "my_chat_member": {
            "chat": chat(-300), "from": user(5), "date": 0,
            "old_chat_member": { "status": "member", "user": { "id": BOT_ID, "is_bot": true, "first_name": "B" } },
            "new_chat_member": { "status": "left", "user": { "id": BOT_ID, "is_bot": true, "first_name": "B" } },
        }});
        run(&mut app, removed);
        assert!(run(&mut app, joined(user(5), -300)).is_empty(), "not asked twice");

        // Strangers adding it to many groups don't flood the owner.
        for id in 0..MAX_PENDING as i64 * 2 {
            run(&mut app, joined(user(5), -1000 - id));
        }
        assert_eq!(app.pending.len(), MAX_PENDING);

        // Removed from the approved group: approval dropped.
        let left = json!({ "my_chat_member": {
            "chat": chat(-200), "from": user(5), "date": 0,
            "old_chat_member": { "status": "member", "user": { "id": BOT_ID, "is_bot": true, "first_name": "B" } },
            "new_chat_member": { "status": "kicked", "until_date": 0, "user": { "id": BOT_ID, "is_bot": true, "first_name": "B" } },
        }});
        run(&mut app, left);
        assert!(!app.access.allows(&crate::access::tests::chat(-200, ChatType::Supergroup)));
    }

    #[test]
    fn supergroup_migration() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.access.add(-200, None);
        let mut msg = json!({ "message_id": 9, "date": 0, "chat": { "id": -200, "type": "group", "title": "G" }, "from": user(5) });
        msg["migrate_to_chat_id"] = json!(-1_000_000_000_200_i64);
        run(&mut app, json!({ "message": msg }));
        assert!(app.access.allows(&crate::access::tests::chat(-1_000_000_000_200, ChatType::Supergroup)));
        assert!(!app.access.allows(&crate::access::tests::chat(-200, ChatType::Group)));
    }

    #[test]
    fn long_replies_fit_telegrams_limit() {
        let link = format!("https://fixupx.com/{}", "a".repeat(1500));
        let links = vec![link.clone(), link.clone(), link.clone(), link];
        let msg: Message = serde_json::from_value(message(-100, user(5), "x")).unwrap();
        let params = reply(&msg, &links).unwrap();
        assert_eq!(params.text.lines().count(), 2);
        assert!(params.text.encode_utf16().count() <= MAX_TEXT);

        // A link too long by itself is left out, and the next one previewed; none that fits, no reply.
        let huge = format!("https://fixupx.com/{}", "a".repeat(MAX_TEXT));
        let short = "https://fixupx.com/a".to_string();
        let params = reply(&msg, &[huge.clone(), short.clone()]).unwrap();
        assert_eq!(
            (params.text.as_str(), params.link_preview_options.unwrap().url),
            (short.as_str(), Some(short.clone()))
        );
        assert!(reply(&msg, &[huge]).is_none());
    }
}
