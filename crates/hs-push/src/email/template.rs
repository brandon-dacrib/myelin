//! The notification email itself: its subject, and its text and HTML bodies. Built here in
//! Rust with escaping rather than through a template engine: the mail has one shape, and a
//! dependency for it would be the larger cost (decision 0007).
//!
//! The shape follows Synapse's `notif_mail` templates: a line saying there are new messages,
//! then one section per room with the room's name linked to the room, each message as the
//! sender's name, the time and a snippet, and a footer naming the service. A message in an
//! encrypted room shows no snippet, since the server cannot read it.

use std::fmt::Write as _;

use ruma::RoomId;
use serde_json::Value;

use super::Subjects;

/// How much of a message's body a snippet shows.
const SNIPPET_CHARS: usize = 200;

/// One notification as the mail shows it. Serialized when the email it is in is held
/// (`super::held`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotificationLine {
    /// The sender's display name, else their user id.
    pub sender: String,
    /// When the event was sent, in milliseconds since the epoch.
    pub ts_ms: u64,
    /// What the line says after the sender's name.
    pub text: LineText,
}

/// What a notification line says.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "text")]
pub enum LineText {
    /// A readable message: its body, cut to [`SNIPPET_CHARS`].
    Snippet(String),
    /// An encrypted message: the server cannot show it.
    Encrypted,
    /// An invitation to the room.
    Invite,
    /// Something that is not a message (a file, a sticker, a state change).
    Activity(String),
}

/// One room's part of the mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomSection {
    /// The room.
    pub room_id: ruma::OwnedRoomId,
    /// The room's name, if it has one.
    pub name: Option<String>,
    /// How many notifications the reader has not read in the room, from the notification
    /// counts (`crate::counts`), which may be more than the lines shown.
    pub unread: u64,
    /// The notifications shown, oldest first.
    pub lines: Vec<NotificationLine>,
}

impl RoomSection {
    fn is_invite(&self) -> bool {
        self.lines.iter().any(|l| l.text == LineText::Invite)
    }

    fn single_sender(&self) -> Option<&str> {
        let first = self.lines.first()?;
        self.lines
            .iter()
            .all(|l| l.sender == first.sender)
            .then_some(first.sender.as_str())
    }
}

/// Everything the mail is rendered from.
#[derive(Debug, Clone)]
pub struct MailInput<'a> {
    /// The service's name (`email.app_name`).
    pub app_name: &'a str,
    /// The web client links open (`email.client_base_url`), or `None` for `matrix.to`.
    pub client_base_url: Option<&'a str>,
    /// The subjects to choose from.
    pub subjects: &'a Subjects,
    /// The rooms with unread notifications, in the order they are shown.
    pub rooms: &'a [RoomSection],
}

/// The line for `event`, or `None` for an event the mail should not mention (a redaction, a
/// receipt, an edit's placeholder).
#[must_use]
pub fn line_for(event: &Value, sender_display_name: Option<&str>) -> Option<NotificationLine> {
    let sender = sender_display_name
        .map(str::to_owned)
        .or_else(|| event["sender"].as_str().map(str::to_owned))?;
    let ts_ms = event["origin_server_ts"].as_u64().unwrap_or(0);
    let content = &event["content"];
    let text = match event["type"].as_str()? {
        "m.room.encrypted" => LineText::Encrypted,
        "m.room.member" if content["membership"] == "invite" => LineText::Invite,
        "m.room.message" => {
            let body = content["body"].as_str().unwrap_or_default();
            match content["msgtype"].as_str().unwrap_or("m.text") {
                "m.text" | "m.notice" => LineText::Snippet(snippet(body)),
                "m.emote" => LineText::Snippet(format!("* {}", snippet(body))),
                "m.image" => LineText::Activity("sent an image".to_owned()),
                "m.file" => LineText::Activity("sent a file".to_owned()),
                "m.audio" => LineText::Activity("sent an audio message".to_owned()),
                "m.video" => LineText::Activity("sent a video".to_owned()),
                "m.location" => LineText::Activity("shared a location".to_owned()),
                _ if !body.is_empty() => LineText::Snippet(snippet(body)),
                _ => LineText::Activity("sent a message".to_owned()),
            }
        }
        "m.sticker" => LineText::Activity("sent a sticker".to_owned()),
        "m.room.name" => LineText::Activity("changed the room's name".to_owned()),
        "m.room.topic" => LineText::Activity("changed the room's topic".to_owned()),
        "m.call.invite" => LineText::Activity("started a call".to_owned()),
        "m.room.member" => return None,
        _ => LineText::Activity("sent a message".to_owned()),
    };
    Some(NotificationLine {
        sender,
        ts_ms,
        text,
    })
}

/// The first [`SNIPPET_CHARS`] characters of `body`, on one line, with an ellipsis if cut.
fn snippet(body: &str) -> String {
    let one_line: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= SNIPPET_CHARS {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(SNIPPET_CHARS).collect();
    cut.push('…');
    cut
}

/// The link that opens `room_id`: `<client_base_url>/#/room/<id>`, or `matrix.to`'s permalink.
#[must_use]
pub fn room_link(client_base_url: Option<&str>, room_id: &RoomId) -> String {
    match client_base_url {
        Some(base) => format!("{}/#/room/{}", base.trim_end_matches('/'), room_id),
        None => format!("https://matrix.to/#/{room_id}"),
    }
}

/// Fills Synapse's `%(app)s`, `%(person)s` and `%(room)s` placeholders.
fn fill(template: &str, app: &str, person: &str, room: &str) -> String {
    template
        .replace("%(app)s", app)
        .replace("%(person)s", person)
        .replace("%(room)s", room)
}

/// The subject for `input`, chosen as Synapse chooses: by how many rooms, whether the room
/// is named, how many messages and from how many people, and whether it is an invitation.
#[must_use]
pub fn subject(input: &MailInput<'_>) -> String {
    let app = input.app_name;
    let s = input.subjects;
    let Some(first) = input.rooms.first() else {
        return fill(&s.messages_in_room, app, "", "");
    };
    let person = first
        .single_sender()
        .or_else(|| first.lines.first().map(|l| l.sender.as_str()))
        .unwrap_or("someone");
    let room = first.name.as_deref().unwrap_or("");
    if input.rooms.len() > 1 {
        return if first.name.is_some() {
            fill(&s.messages_in_room_and_others, app, person, room)
        } else {
            fill(&s.messages_from_person_and_others, app, person, room)
        };
    }
    if first.is_invite() {
        return if first.name.is_some() {
            fill(&s.invite_from_person_to_room, app, person, room)
        } else {
            fill(&s.invite_from_person, app, person, room)
        };
    }
    let one = first.lines.len() <= 1 && first.unread <= 1;
    match (first.name.is_some(), one, first.single_sender().is_some()) {
        (true, true, _) => fill(&s.message_from_person_in_room, app, person, room),
        (true, false, _) => fill(&s.messages_in_room, app, person, room),
        (false, true, _) => fill(&s.message_from_person, app, person, room),
        (false, false, _) => fill(&s.messages_from_person, app, person, room),
    }
}

/// `HH:MM UTC` for a timestamp, or `?` for one that is not a date.
fn clock(ts_ms: u64) -> String {
    let secs = i64::try_from(ts_ms / 1000).unwrap_or(i64::MAX);
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(t) => format!("{:02}:{:02} UTC", t.hour(), t.minute()),
        Err(_) => "?".to_owned(),
    }
}

fn room_title(room: &RoomSection) -> String {
    match &room.name {
        Some(name) => name.clone(),
        None => match room.single_sender() {
            Some(sender) => sender.to_owned(),
            None => "a room".to_owned(),
        },
    }
}

fn line_text(line: &NotificationLine) -> String {
    match &line.text {
        LineText::Snippet(s) => s.clone(),
        LineText::Encrypted => "an encrypted message".to_owned(),
        LineText::Invite => "invited you".to_owned(),
        LineText::Activity(a) => a.clone(),
    }
}

/// The plain-text body.
#[must_use]
pub fn render_text(input: &MailInput<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "You have new messages on {}.", input.app_name);
    for room in input.rooms {
        let _ = writeln!(out);
        let link = room_link(input.client_base_url, &room.room_id);
        let _ = writeln!(out, "{} ({} unread)", room_title(room), room.unread);
        for line in &room.lines {
            let _ = writeln!(
                out,
                "  {} at {}: {}",
                line.sender,
                clock(line.ts_ms),
                line_text(line)
            );
        }
        let shown = u64::try_from(room.lines.len()).unwrap_or(u64::MAX);
        if room.unread > shown {
            let _ = writeln!(out, "  ... and {} more", room.unread - shown);
        }
        let _ = writeln!(out, "  Open the room: {link}");
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "You are receiving this because you asked {} to email you about messages you have not \
         read. To stop, remove the email notification in your client's settings.",
        input.app_name
    );
    out
}

/// `text` with the five HTML metacharacters escaped.
#[must_use]
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The HTML body: the same content as [`render_text`], with the room's name a link.
#[must_use]
pub fn render_html(input: &MailInput<'_>) -> String {
    let app = escape_html(input.app_name);
    let mut out = String::new();
    out.push_str(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>New messages</title></head>\n\
         <body style=\"font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, \
         sans-serif; color: #1f1f1f; max-width: 640px; margin: 0 auto; padding: 16px;\">\n",
    );
    let _ = writeln!(
        out,
        "<p style=\"font-size: 16px;\">You have new messages on <strong>{app}</strong>.</p>"
    );
    for room in input.rooms {
        let link = escape_html(&room_link(input.client_base_url, &room.room_id));
        let _ = writeln!(
            out,
            "<h2 style=\"font-size: 15px; margin: 24px 0 8px;\"><a href=\"{link}\" \
             style=\"color: #0b57d0;\">{}</a> <span style=\"font-weight: normal; color: \
             #666;\">({} unread)</span></h2>",
            escape_html(&room_title(room)),
            room.unread
        );
        out.push_str("<ul style=\"list-style: none; padding: 0; margin: 0;\">\n");
        for line in &room.lines {
            let text = match &line.text {
                LineText::Snippet(s) => escape_html(s),
                other => format!(
                    "<em>{}</em>",
                    escape_html(&line_text(&NotificationLine {
                        sender: String::new(),
                        ts_ms: 0,
                        text: other.clone(),
                    }))
                ),
            };
            let _ = writeln!(
                out,
                "<li style=\"margin: 6px 0;\"><strong>{}</strong> <span style=\"color: #666; \
                 font-size: 12px;\">{}</span><br>{}</li>",
                escape_html(&line.sender),
                clock(line.ts_ms),
                text
            );
        }
        let shown = u64::try_from(room.lines.len()).unwrap_or(u64::MAX);
        if room.unread > shown {
            let _ = writeln!(
                out,
                "<li style=\"margin: 6px 0; color: #666;\">… and {} more</li>",
                room.unread - shown
            );
        }
        out.push_str("</ul>\n");
        let _ = writeln!(
            out,
            "<p><a href=\"{link}\" style=\"color: #0b57d0;\">Open the room</a></p>"
        );
    }
    let _ = writeln!(
        out,
        "<p style=\"margin-top: 32px; font-size: 12px; color: #666;\">You are receiving this \
         because you asked {app} to email you about messages you have not read. To stop, remove \
         the email notification in your client's settings.</p>\n</body></html>"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn room(name: Option<&str>, lines: Vec<NotificationLine>, unread: u64) -> RoomSection {
        RoomSection {
            room_id: ruma::room_id!("!room:example.org").to_owned(),
            name: name.map(str::to_owned),
            unread,
            lines,
        }
    }

    fn line(sender: &str, text: LineText) -> NotificationLine {
        NotificationLine {
            sender: sender.to_owned(),
            ts_ms: 1_700_000_000_000,
            text,
        }
    }

    fn input<'a>(rooms: &'a [RoomSection], subjects: &'a Subjects) -> MailInput<'a> {
        MailInput {
            app_name: "Myelin",
            client_base_url: Some("https://app.example.org/"),
            subjects,
            rooms,
        }
    }

    #[test]
    fn lines_follow_the_event_kind_and_snippets_are_short() {
        let text = line_for(
            &json!({"type": "m.room.message", "sender": "@bob:x", "origin_server_ts": 5,
                    "content": {"msgtype": "m.text", "body": "hello\nthere   world"}}),
            Some("Bob"),
        )
        .unwrap();
        assert_eq!(text.sender, "Bob");
        assert_eq!(text.text, LineText::Snippet("hello there world".to_owned()));

        let long = "x".repeat(500);
        let cut = line_for(
            &json!({"type": "m.room.message", "sender": "@bob:x",
                    "content": {"msgtype": "m.text", "body": long}}),
            None,
        )
        .unwrap();
        assert_eq!(cut.sender, "@bob:x");
        let LineText::Snippet(s) = cut.text else {
            panic!()
        };
        assert_eq!(s.chars().count(), SNIPPET_CHARS + 1);
        assert!(s.ends_with('…'));

        let encrypted = line_for(
            &json!({"type": "m.room.encrypted", "sender": "@bob:x", "content": {"ciphertext": "zzz"}}),
            None,
        )
        .unwrap();
        assert_eq!(encrypted.text, LineText::Encrypted);
        let invite = line_for(
            &json!({"type": "m.room.member", "sender": "@bob:x", "state_key": "@a:x",
                    "content": {"membership": "invite"}}),
            None,
        )
        .unwrap();
        assert_eq!(invite.text, LineText::Invite);
        assert_eq!(
            line_for(
                &json!({"type": "m.room.message", "sender": "@bob:x",
                        "content": {"msgtype": "m.image", "body": "cat.png"}}),
                None
            )
            .unwrap()
            .text,
            LineText::Activity("sent an image".to_owned())
        );
        assert!(
            line_for(
                &json!({"type": "m.room.member", "sender": "@bob:x", "content": {"membership": "join"}}),
                None
            )
            .is_none()
        );
    }

    #[test]
    fn the_subject_is_chosen_as_synapse_chooses_it() {
        let subjects = Subjects::default();
        let one_named = [room(
            Some("Lunch"),
            vec![line("Bob", LineText::Snippet("hi".into()))],
            1,
        )];
        assert_eq!(
            subject(&input(&one_named, &subjects)),
            "[Myelin] You have a message on Myelin from Bob in the Lunch room..."
        );
        let many_named = [room(
            Some("Lunch"),
            vec![
                line("Bob", LineText::Snippet("hi".into())),
                line("Carol", LineText::Snippet("yo".into())),
            ],
            2,
        )];
        assert_eq!(
            subject(&input(&many_named, &subjects)),
            "[Myelin] You have messages on Myelin in the Lunch room..."
        );
        let dm_one = [room(None, vec![line("Bob", LineText::Encrypted)], 1)];
        assert_eq!(
            subject(&input(&dm_one, &subjects)),
            "[Myelin] You have a message on Myelin from Bob..."
        );
        let dm_many = [room(None, vec![line("Bob", LineText::Encrypted)], 3)];
        assert_eq!(
            subject(&input(&dm_many, &subjects)),
            "[Myelin] You have messages on Myelin from Bob..."
        );
        let invite = [room(Some("Lunch"), vec![line("Bob", LineText::Invite)], 1)];
        assert_eq!(
            subject(&input(&invite, &subjects)),
            "[Myelin] Bob has invited you to join the Lunch room on Myelin..."
        );
        let two_rooms = [
            room(
                Some("Lunch"),
                vec![line("Bob", LineText::Snippet("hi".into()))],
                1,
            ),
            room(None, vec![line("Carol", LineText::Snippet("yo".into()))], 1),
        ];
        assert_eq!(
            subject(&input(&two_rooms, &subjects)),
            "[Myelin] You have messages on Myelin in the Lunch room and others..."
        );
    }

    #[test]
    fn bodies_link_the_room_escape_html_and_hide_encrypted_content() {
        let subjects = Subjects::default();
        let rooms = [room(
            Some("R&D <team>"),
            vec![
                line("Bob", LineText::Snippet("<script>alert(1)</script>".into())),
                line("Carol", LineText::Encrypted),
            ],
            5,
        )];
        let input = input(&rooms, &subjects);
        let text = render_text(&input);
        assert!(text.contains("R&D <team> (5 unread)"));
        assert!(text.contains("Bob at 22:13 UTC: <script>alert(1)</script>"));
        assert!(text.contains("Carol at 22:13 UTC: an encrypted message"));
        assert!(text.contains("... and 3 more"));
        assert!(text.contains("Open the room: https://app.example.org/#/room/!room:example.org"));

        let html = render_html(&input);
        assert!(html.contains("R&amp;D &lt;team&gt;"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("<em>an encrypted message</em>"));
        assert!(html.contains("href=\"https://app.example.org/#/room/!room:example.org\""));

        assert_eq!(
            room_link(None, ruma::room_id!("!r:x")),
            "https://matrix.to/#/!r:x"
        );
    }
}
