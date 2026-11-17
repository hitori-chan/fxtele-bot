//! The owner menu: commands, Telegram's user and group pickers, the list with remove buttons,
//! and the approve/leave prompt for groups someone else added the bot to.

use frankenstein::methods::{AnswerCallbackQueryParams, EditMessageTextParams, SendMessageParams};
use frankenstein::types::{
    CallbackQuery, ChatShared, InlineKeyboardButton, InlineKeyboardMarkup, KeyboardButton, KeyboardButtonRequestChat,
    KeyboardButtonRequestUsers, MaybeInaccessibleMessage, Message, ReplyKeyboardMarkup, ReplyMarkup, UsersShared,
};
use tracing::info;

use crate::access::{Added, Entry, Name, Removed};
use crate::app::{Action, App};

const PICK_PEOPLE: i32 = 1;
const PICK_GROUP: i32 = 2;
const ADD_PERSON: &str = "Add person";
const ADD_GROUP: &str = "Add group";
const LIST: &str = "List";
const PAGE_SIZE: usize = 8;

/// The commands shown in the owner's `/` menu.
pub const COMMANDS: [(&str, &str); 3] = [
    ("menu", "Add or remove people and groups"),
    ("allow", "Allow a person (positive ID) or a group (negative ID)"),
    ("remove", "Remove a person or a group by ID"),
];

#[derive(Debug, PartialEq, Eq)]
enum Callback {
    Page(usize),
    Remove(i64, usize),
    Approve(i64),
    Decline(i64),
    Noop,
}

impl Callback {
    fn parse(data: &str) -> Self {
        let mut parts = data.split(':');
        let kind = parts.next().unwrap_or_default();
        let first = parts.next();
        let second = parts.next();
        let page = |s: Option<&str>| s.and_then(|s| s.parse().ok());
        let id = |s: Option<&str>| s.and_then(|s| s.parse::<i64>().ok()).filter(|&id| id != 0);
        match kind {
            "l" => page(first).map_or(Self::Noop, Self::Page),
            "r" => id(first).zip(page(second)).map_or(Self::Noop, |(id, p)| Self::Remove(id, p)),
            "a" => id(first).map_or(Self::Noop, Self::Approve),
            "x" => id(first).map_or(Self::Noop, Self::Decline),
            _ => Self::Noop,
        }
    }
}

impl App {
    /// Handles the owner's private messages that belong to the menu. `None` means
    /// it's an ordinary message, handled like anyone else's.
    pub(crate) fn owner_message(&mut self, message: &Message) -> Option<Vec<Action>> {
        let chat = message.chat.id;
        if let Some(shared) = &message.users_shared {
            return Some(vec![self.people_picked(chat, shared)]);
        }
        if let Some(shared) = &message.chat_shared {
            return Some(vec![self.group_picked(chat, shared)]);
        }
        let text = message.text.as_deref()?.trim();
        if text == LIST {
            let (text, keyboard) = self.list_page(0);
            return Some(vec![send(chat, text, Some(keyboard))]);
        }
        let (command, args) = self.command(text)?;
        let reply = match command {
            "start" | "menu" => menu(chat),
            "allow" => self.allow_command(chat, args),
            "remove" => return Some(self.remove_command(chat, args)),
            _ => return None,
        };
        Some(vec![reply])
    }

    /// Splits `/command@bot args`; commands addressed to another bot are `None`.
    fn command<'a>(&self, text: &'a str) -> Option<(&'a str, &'a str)> {
        let rest = text.strip_prefix('/')?;
        let (word, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let command = match word.split_once('@') {
            Some((command, bot)) if bot.eq_ignore_ascii_case(&self.username) => command,
            Some(_) => return None,
            None => word,
        };
        Some((command, args.trim()))
    }

    fn allow_command(&mut self, chat: i64, args: &str) -> Action {
        let Some(id) = parse_id(args) else {
            return send(chat, "Usage: /allow <id>, a person's ID (positive) or a group's ID (negative).".into(), None);
        };
        let label = self.label(id);
        let text = match (self.access.add(id, None), id > 0) {
            (Added::New, true) => format!("Added {label}."),
            (Added::New, false) => format!("Added group {label}. Add me to it if I'm not there yet."),
            (Added::Already, _) => format!("{label} is already allowed."),
        };
        info!(id, "owner allowed by ID");
        send(chat, text, None)
    }

    fn remove_command(&mut self, chat: i64, args: &str) -> Vec<Action> {
        let Some(id) = parse_id(args) else {
            return vec![send(chat, "Usage: /remove <id>".into(), None)];
        };
        let (text, leave) = self.remove(id);
        let mut actions = vec![send(chat, text, None)];
        actions.extend(leave);
        actions
    }

    /// Removes an entry. Returns the reply text and, for a removed group, the leave action.
    fn remove(&mut self, id: i64) -> (String, Option<Action>) {
        let label = self.label(id);
        match self.access.remove(id) {
            Removed::Done if id < 0 => {
                info!(id, "owner removed a group; leaving it");
                self.pending.remove(&id);
                (format!("Removed group {label} and left it."), Some(Action::Leave(id)))
            }
            Removed::Done => {
                info!(id, "owner removed a person");
                (format!("Removed {label}."), None)
            }
            Removed::Permanent => (format!("{label} is in bot.toml; edit the file to remove it."), None),
            Removed::NotFound => (format!("{label} isn't on the list."), None),
        }
    }

    fn people_picked(&mut self, chat: i64, shared: &UsersShared) -> Action {
        let mut added = Vec::new();
        let mut already = Vec::new();
        for user in &shared.users {
            let Ok(id) = i64::try_from(user.user_id) else { continue };
            let name = user
                .first_name
                .as_deref()
                .map(|first| Name::new(first, user.last_name.as_deref(), user.username.clone()));
            let result = self.access.add(id, name);
            let label = self.label(id);
            match result {
                Added::New => added.push(label),
                Added::Already => already.push(label),
            }
        }
        info!(added = added.len(), "owner added people from the picker");
        let mut text = Vec::new();
        if !added.is_empty() {
            text.push(format!("Added {}.", added.join(", ")));
        }
        if !already.is_empty() {
            text.push(format!("Already allowed: {}.", already.join(", ")));
        }
        send(chat, text.join("\n"), None)
    }

    fn group_picked(&mut self, chat: i64, shared: &ChatShared) -> Action {
        let id = shared.chat_id;
        let name = shared.title.clone().map(|name| Name { name, username: shared.username.clone() });
        let result = self.access.add(id, name);
        let label = self.label(id);
        info!(id, "owner added a group from the picker");
        let text = match result {
            Added::New => format!("Added group {label}. Add me to it if I'm not there yet."),
            Added::Already => format!("{label} is already allowed."),
        };
        send(chat, text, None)
    }

    /// The prompt the owner gets when someone else adds the bot to a group.
    pub(crate) fn approval_prompt(&self, group: i64, title: &str, added_by: &Name) -> Action {
        let text = format!("{added_by} added me to {title} ({group}). I'll stay silent there until you approve it.");
        let keyboard = InlineKeyboardMarkup::builder()
            .inline_keyboard(vec![vec![button("Approve", format!("a:{group}")), button("Leave", format!("x:{group}"))]])
            .build();
        send(self.access.owner(), text, Some(keyboard))
    }

    pub(crate) fn on_callback(&mut self, query: &CallbackQuery) -> Vec<Action> {
        let mut answer = AnswerCallbackQueryParams::builder().callback_query_id(query.id.clone()).build();
        if !self.access.is_owner(&query.from) {
            return vec![Action::Answer(answer)];
        }
        let target = query.message.as_ref().map(|message| match message {
            MaybeInaccessibleMessage::Message(m) => (m.chat.id, m.message_id),
            MaybeInaccessibleMessage::InaccessibleMessage(m) => (m.chat.id, m.message_id),
        });
        let mut actions = Vec::new();
        let mut edit = |text: String, keyboard: Option<InlineKeyboardMarkup>| {
            if let Some((chat, message_id)) = target {
                let params = EditMessageTextParams::builder()
                    .chat_id(chat)
                    .message_id(message_id)
                    .text(text)
                    .maybe_reply_markup(keyboard)
                    .build();
                actions.push(Action::Edit(params));
            }
        };

        let mut leave = None;
        match Callback::parse(query.data.as_deref().unwrap_or_default()) {
            Callback::Page(page) => {
                let (text, keyboard) = self.list_page(page);
                edit(text, Some(keyboard));
            }
            Callback::Remove(id, page) => {
                let (text, leave_action) = self.remove(id);
                leave = leave_action;
                // Telegram refuses a notice over 200 characters.
                answer.text = Some(truncate(&text, 199));
                let (text, keyboard) = self.list_page(page);
                edit(text, Some(keyboard));
            }
            Callback::Approve(id) => {
                let id = self.moved.remove(&id).unwrap_or(id);
                let waiting = self.pending.remove(&id);
                let title = waiting.clone().unwrap_or_else(|| self.label(id));
                // The title the prompt had, unless the group's name is known; never just its ID.
                let name = self.access.name(id).cloned().or_else(|| waiting.map(|name| Name { name, username: None }));
                self.access.add(id, name);
                info!(id, "owner approved a group");
                edit(format!("Approved {title}."), None);
            }
            Callback::Decline(id) => {
                let id = self.moved.remove(&id).unwrap_or(id);
                let title = self.pending.remove(&id).unwrap_or_else(|| self.label(id));
                self.access.remove(id);
                info!(id, "owner declined a group; leaving it");
                leave = Some(Action::Leave(id));
                edit(format!("Left {title}."), None);
            }
            Callback::Noop => {}
        }
        actions.push(Action::Answer(answer));
        actions.extend(leave);
        actions
    }

    /// One page of the allowlist, with a remove button for every entry not from `bot.toml`.
    pub(crate) fn list_page(&self, page: usize) -> (String, InlineKeyboardMarkup) {
        let entries = self.access.entries();
        if entries.is_empty() {
            let text = format!("Only you can use me right now. Use {ADD_PERSON} or {ADD_GROUP} in /menu.");
            return (text, InlineKeyboardMarkup::builder().inline_keyboard(Vec::new()).build());
        }
        let pages = entries.len().div_ceil(PAGE_SIZE);
        let page = page.min(pages - 1);
        let shown = &entries[page * PAGE_SIZE..entries.len().min((page + 1) * PAGE_SIZE)];

        let people = entries.iter().filter(|e| e.id > 0).count();
        let groups = entries.len() - people;
        let mut text = format!(
            "Allowed: {people} {}, {groups} {}",
            if people == 1 { "person" } else { "people" },
            if groups == 1 { "group" } else { "groups" },
        );
        if pages > 1 {
            text.push_str(&format!(" (page {} of {pages})", page + 1));
        }
        let mut heading = None;
        let mut rows = Vec::new();
        for entry in shown {
            let section = if entry.id > 0 { "People" } else { "Groups" };
            if heading != Some(section) {
                text.push_str(&format!("\n\n{section}"));
                heading = Some(section);
            }
            text.push_str(&format!("\n• {}", entry_line(entry)));
            if !entry.permanent {
                let label = entry.name.as_ref().map_or_else(|| entry.id.to_string(), |n| n.name.clone());
                rows.push(vec![button(&format!("Remove {}", truncate(&label, 40)), format!("r:{}:{page}", entry.id))]);
            }
        }
        let mut nav = Vec::new();
        if page > 0 {
            nav.push(button("‹ Previous", format!("l:{}", page - 1)));
        }
        if page + 1 < pages {
            nav.push(button("Next ›", format!("l:{}", page + 1)));
        }
        if !nav.is_empty() {
            rows.push(nav);
        }
        (text, InlineKeyboardMarkup::builder().inline_keyboard(rows).build())
    }

    fn label(&self, id: i64) -> String {
        self.access.name(id).map_or_else(|| id.to_string(), |name| format!("{name} ({id})"))
    }
}

fn entry_line(entry: &Entry) -> String {
    let mut line = match &entry.name {
        Some(name) => format!("{name} · {}", entry.id),
        None => entry.id.to_string(),
    };
    if entry.permanent {
        line.push_str(" · bot.toml");
    }
    line
}

fn parse_id(args: &str) -> Option<i64> {
    args.split_whitespace().next()?.parse().ok().filter(|&id: &i64| id != 0)
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_string(),
    }
}

fn button(text: &str, data: String) -> InlineKeyboardButton {
    InlineKeyboardButton::builder().text(text).callback_data(data).build()
}

fn send(chat: i64, text: String, keyboard: Option<InlineKeyboardMarkup>) -> Action {
    Action::Send(
        SendMessageParams::builder()
            .chat_id(chat)
            .text(text)
            .maybe_reply_markup(keyboard.map(ReplyMarkup::InlineKeyboardMarkup))
            .build(),
    )
}

/// The menu: a keyboard with Telegram's pickers and the list button.
fn menu(chat: i64) -> Action {
    let people = KeyboardButtonRequestUsers::builder()
        .request_id(PICK_PEOPLE)
        .user_is_bot(false)
        .max_quantity(10)
        .request_name(true)
        .request_username(true)
        .build();
    let group = KeyboardButtonRequestChat::builder()
        .request_id(PICK_GROUP)
        .chat_is_channel(false)
        .request_title(true)
        .request_username(true)
        .build();
    let keyboard = ReplyKeyboardMarkup::builder()
        .keyboard(vec![
            vec![
                KeyboardButton::builder().text(ADD_PERSON).request_users(people).build(),
                KeyboardButton::builder().text(ADD_GROUP).request_chat(group).build(),
            ],
            vec![KeyboardButton::builder().text(LIST).build()],
        ])
        .resize_keyboard(true)
        .is_persistent(true)
        .build();
    let text = "Pick people or groups to allow, or open the list to remove them. \
                You can also use /allow <id> and /remove <id>.";
    Action::Send(
        SendMessageParams::builder()
            .chat_id(chat)
            .text(text)
            .reply_markup(ReplyMarkup::ReplyKeyboardMarkup(keyboard))
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::app::tests::{OWNER, app, chat, message, run, sent_text, user};
    use frankenstein::types::ChatType;

    fn owner_says(text: &str) -> Value {
        json!({ "message": message(OWNER, user(OWNER), text) })
    }

    fn callback(from: i64, data: &str) -> Value {
        json!({ "callback_query": {
            "id": "q", "from": user(from), "chat_instance": "c", "data": data,
            "message": { "message_id": 50, "date": 0, "chat": chat(OWNER), "text": "list" },
        }})
    }

    #[test]
    fn callback_data_round_trip() {
        assert_eq!(Callback::parse("l:2"), Callback::Page(2));
        assert_eq!(Callback::parse("r:-100123:1"), Callback::Remove(-100_123, 1));
        assert_eq!(Callback::parse("a:-5"), Callback::Approve(-5));
        assert_eq!(Callback::parse("x:-5"), Callback::Decline(-5));
        assert_eq!(Callback::parse("r:abc:1"), Callback::Noop);
        assert_eq!(Callback::parse("a:0"), Callback::Noop);
        assert_eq!(Callback::parse(""), Callback::Noop);
        assert!("r:-1001234567890123:999".len() <= 64);
    }

    #[test]
    fn menu_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let actions = run(&mut app, owner_says("/menu"));
        let [Action::Send(menu)] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(matches!(menu.reply_markup, Some(ReplyMarkup::ReplyKeyboardMarkup(_))));

        // Someone else, even an allowed user in their own chat: ignored.
        assert!(run(&mut app, json!({ "message": message(10, user(10), "/menu") })).is_empty());
        assert!(run(&mut app, json!({ "message": message(10, user(10), "/allow 55") })).is_empty());
        assert!(run(&mut app, json!({ "message": message(-100, user(OWNER), "/allow 55") })).is_empty());
        assert!(!app.access.allows(&crate::access::tests::chat(55, ChatType::Private)));

        // Buttons from someone else only get the spinner stopped.
        let actions = run(&mut app, callback(10, "x:-100"));
        assert!(matches!(actions.as_slice(), [Action::Answer(_)]));
    }

    #[test]
    fn commands_for_another_bot_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        assert_eq!(run(&mut app, owner_says("/menu@FixBot")).len(), 1);
        assert!(run(&mut app, owner_says("/menu@otherbot")).is_empty());
    }

    #[test]
    fn allow_and_remove_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        assert_eq!(sent_text(&run(&mut app, owner_says("/allow 55"))), ["Added 55."]);
        assert_eq!(sent_text(&run(&mut app, owner_says("/allow 55"))), ["55 is already allowed."]);
        assert!(sent_text(&run(&mut app, owner_says("/allow -200")))[0].starts_with("Added group -200."));
        assert!(sent_text(&run(&mut app, owner_says("/allow nope")))[0].starts_with("Usage"));

        assert_eq!(sent_text(&run(&mut app, owner_says("/remove 55"))), ["Removed 55."]);
        let actions = run(&mut app, owner_says("/remove -200"));
        assert_eq!(sent_text(&actions), ["Removed group -200 and left it."]);
        assert!(actions.contains(&Action::Leave(-200)), "removing a group leaves it");

        assert_eq!(
            sent_text(&run(&mut app, owner_says("/remove -100"))),
            ["-100 is in bot.toml; edit the file to remove it."]
        );
        assert_eq!(sent_text(&run(&mut app, owner_says("/remove 77"))), ["77 isn't on the list."]);
    }

    #[test]
    fn pickers_add_with_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let mut msg = message(OWNER, user(OWNER), "");
        msg.as_object_mut().unwrap().remove("text");
        msg["users_shared"] = json!({ "request_id": PICK_PEOPLE, "users": [
            { "user_id": 55, "first_name": "Ann", "last_name": "Lee", "username": "ann" },
            { "user_id": 10, "first_name": "Ten" },
        ]});
        let actions = run(&mut app, json!({ "message": msg.clone() }));
        assert_eq!(sent_text(&actions), ["Added Ann Lee (@ann) (55).\nAlready allowed: Ten (10)."]);

        msg.as_object_mut().unwrap().remove("users_shared");
        msg["chat_shared"] = json!({ "request_id": PICK_GROUP, "chat_id": -300, "title": "Friends" });
        let actions = run(&mut app, json!({ "message": msg }));
        assert!(sent_text(&actions)[0].starts_with("Added group Friends (-300)."));
        assert!(app.access.allows(&crate::access::tests::chat(-300, ChatType::Supergroup)));
    }

    #[test]
    fn list_with_remove_buttons_and_pages() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        for id in 20..30 {
            app.access.add(id, Some(Name { name: format!("P{id}"), username: None }));
        }
        let actions = run(&mut app, owner_says("List"));
        let [Action::Send(list)] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(list.text.starts_with("Allowed: 11 people, 1 group (page 1 of 2)"));
        assert!(list.text.contains("• 10 · bot.toml"));
        let Some(ReplyMarkup::InlineKeyboardMarkup(keyboard)) = &list.reply_markup else { panic!() };
        let buttons: Vec<&str> = keyboard.inline_keyboard.iter().flatten().map(|b| b.text.as_str()).collect();
        assert_eq!(buttons.len(), 8, "7 removable on page 1 (bot.toml entry has none) + Next");
        assert_eq!(buttons.last(), Some(&"Next ›"));

        let actions = run(&mut app, callback(OWNER, "l:1"));
        let [Action::Edit(page2), Action::Answer(_)] = actions.as_slice() else { panic!("{actions:?}") };
        assert!(page2.text.as_deref().unwrap().contains("Groups\n• -100 · bot.toml"));

        let actions = run(&mut app, callback(OWNER, "r:29:1"));
        let [Action::Edit(_), Action::Answer(answer)] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(answer.text.as_deref(), Some("Removed P29 (29)."));
        assert!(!app.access.entries().iter().any(|e| e.id == 29));
    }

    #[test]
    fn approve_and_leave_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        app.pending.insert(-300, "Friends".into());
        let actions = run(&mut app, callback(OWNER, "a:-300"));
        let [Action::Edit(edit), Action::Answer(_)] = actions.as_slice() else { panic!("{actions:?}") };
        assert_eq!(edit.text.as_deref(), Some("Approved Friends."));
        assert!(app.access.allows(&crate::access::tests::chat(-300, ChatType::Supergroup)));
        assert_eq!(app.access.name(-300).unwrap().name, "Friends");

        app.pending.insert(-400, "Strangers".into());
        let actions = run(&mut app, callback(OWNER, "x:-400"));
        assert!(actions.contains(&Action::Leave(-400)));
        assert!(app.pending.is_empty());

        // Approved after a restart, which forgot the prompt: no name rather than the ID as one.
        run(&mut app, callback(OWNER, "a:-500"));
        assert!(app.access.allows(&crate::access::tests::chat(-500, ChatType::Supergroup)));
        assert_eq!(app.access.name(-500), None);

        // A waiting group that became a supergroup: its prompt approves the new ID.
        app.pending.insert(-600, "Moved".into());
        let mut moved = json!({ "message_id": 9, "date": 0, "chat": { "id": -600, "type": "group", "title": "Moved" }, "from": user(5) });
        moved["migrate_to_chat_id"] = json!(-1_000_000_000_600_i64);
        run(&mut app, json!({ "message": moved }));
        run(&mut app, callback(OWNER, "a:-600"));
        assert!(app.access.allows(&crate::access::tests::chat(-1_000_000_000_600, ChatType::Supergroup)));
        assert!(app.pending.is_empty() && app.moved.is_empty());
    }

    #[test]
    fn forwarded_commands_dont_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let mut msg = message(OWNER, user(OWNER), "/allow 666");
        msg["forward_origin"] = json!({ "type": "user", "date": 0, "sender_user": user(8) });
        assert!(run(&mut app, json!({ "message": msg })).is_empty());
        assert!(!app.access.allows(&crate::access::tests::chat(666, ChatType::Private)));
    }
}
