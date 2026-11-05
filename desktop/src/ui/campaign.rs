//! Send-list campaigns: progress per recipient, continuing an interrupted
//! send, following up people who did not reply, and AI-reworded variants so
//! a list does not receive one identical text over and over.

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Box, Button, CheckButton, Label, Orientation, ScrolledWindow, TextView};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::bridge::CampaignRow;

/// Ask the configured AI provider (Settings → AI Autocorrect) for `n`
/// rewordings of `text`. Blocking; call off the UI thread.
pub fn generate_variations(text: &str, n: usize) -> Result<Vec<String>, String> {
    let settings = crate::ui::settings::AppSettings::load();
    let provider = settings.ai_model.as_str();
    let key = if provider == "deepseek" {
        settings.deepseek_api_key.clone()
    } else {
        settings.ai_api_key.clone()
    };
    if provider == "none" || key.is_empty() {
        return Err("No AI provider set up — add a key in Settings → AI Autocorrect".into());
    }
    let prompt = format!(
        "Rewrite the WhatsApp message below {n} different ways, one for each of {n} people.\n\
         Keep the meaning, every fact, name, number, link and offer exactly. Keep the \
         sender's voice, language and level of formality, and roughly the same length. \
         Vary wording, word order and sentence structure so no two read the same. Do not \
         add greetings, sign-offs, emojis or claims the original does not have.\n\
         Reply with ONLY a JSON array of {n} strings.\n\nMessage:\n{text}"
    );
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(60))
        .build();
    let raw: String = if provider == "deepseek" {
        let body = serde_json::json!({
            "model": "deepseek-chat",
            "messages": [{ "role": "user", "content": prompt }],
            "temperature": 0.9,
            "max_tokens": 4096,
        });
        let json: serde_json::Value = agent
            .post("https://api.deepseek.com/chat/completions")
            .set("authorization", &format!("Bearer {key}"))
            .send_json(&body)
            .map_err(|e| format!("AI request failed: {e}"))?
            .into_json()
            .map_err(|e| format!("AI response unreadable: {e}"))?;
        json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    } else {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-flash-latest:generateContent?key={key}"
        );
        let body = serde_json::json!({
            "contents": [{ "parts": [{ "text": prompt }] }],
            "generationConfig": { "temperature": 0.9, "maxOutputTokens": 4096 },
        });
        let json: serde_json::Value = agent
            .post(&url)
            .send_json(&body)
            .map_err(|e| format!("AI request failed: {e}"))?
            .into_json()
            .map_err(|e| format!("AI response unreadable: {e}"))?;
        json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    // Models like to wrap JSON in a code fence.
    let start = raw.find('[').ok_or("AI did not return a list")?;
    let end = raw.rfind(']').ok_or("AI did not return a list")?;
    let list: Vec<String> =
        serde_json::from_str(&raw[start..=end]).map_err(|e| format!("AI list unreadable: {e}"))?;
    let list: Vec<String> = list
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if list.is_empty() {
        return Err("AI returned no variations".into());
    }
    Ok(list)
}

/// Compose a campaign message with optional AI variants, then hand
/// `(text, variants)` to `on_send`. `variants` is empty when the user chose to
/// send the text as is; otherwise it includes the original.
pub fn show_compose_dialog(
    parent: &gtk4::Widget,
    title: &str,
    prefill: &str,
    recipients: usize,
    reword_default: bool,
    variation_count: usize,
    on_send: impl Fn(String, Vec<String>) + 'static,
) {
    let dialog = adw::Dialog::new();
    dialog.set_title(title);
    dialog.set_content_width(560);
    dialog.set_content_height(680);

    let body = Box::new(Orientation::Vertical, 10);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(16);
    body.set_margin_end(16);

    body.append(&heading(&format!("Message to {recipients} people")));
    let text_view = TextView::new();
    text_view.set_wrap_mode(gtk4::WrapMode::WordChar);
    text_view.set_top_margin(8);
    text_view.set_bottom_margin(8);
    text_view.set_left_margin(8);
    text_view.set_right_margin(8);
    text_view.buffer().set_text(prefill);
    let text_frame = gtk4::Frame::new(None);
    text_frame.set_child(Some(&text_view));
    text_view.set_size_request(-1, 100);
    body.append(&text_frame);

    let reword = CheckButton::with_label("Reword it for each person with AI (lowers spam risk)");
    reword.set_active(reword_default);
    body.append(&reword);

    let gen_row = Box::new(Orientation::Horizontal, 8);
    let gen_btn = Button::with_label("Generate variations");
    let gen_status = Label::new(None);
    gen_status.add_css_class("dim-label");
    gen_status.set_wrap(true);
    gen_status.set_hexpand(true);
    gen_status.set_halign(Align::Start);
    gen_row.append(&gen_btn);
    gen_row.append(&gen_status);
    body.append(&gen_row);

    // Each variant is editable before anything is sent.
    let variants_box = Box::new(Orientation::Vertical, 8);
    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&variants_box));
    body.append(&scroll);
    let variant_views: Rc<std::cell::RefCell<Vec<TextView>>> = Rc::default();

    let send_btn = Button::with_label(&format!("Send to {recipients}"));
    send_btn.add_css_class("suggested-action");
    send_btn.add_css_class("pill");
    body.append(&send_btn);

    let sync_controls: Rc<dyn Fn()> = {
        let reword = reword.clone();
        let gen_row = gen_row.clone();
        let scroll = scroll.clone();
        let send_btn = send_btn.clone();
        let views = variant_views.clone();
        Rc::new(move || {
            let on = reword.is_active();
            gen_row.set_visible(on);
            scroll.set_visible(on);
            // With rewording on, nothing goes out until variants exist.
            send_btn.set_sensitive(!on || !views.borrow().is_empty());
        })
    };
    {
        let s = sync_controls.clone();
        reword.connect_toggled(move |_| s());
    }
    sync_controls();

    {
        let text_view = text_view.clone();
        let gen_status = gen_status.clone();
        let variants_box = variants_box.clone();
        let views = variant_views.clone();
        let sync = sync_controls.clone();
        gen_btn.connect_clicked(move |btn| {
            let text = buffer_text(&text_view);
            if text.trim().is_empty() {
                gen_status.set_text("Write the message first.");
                return;
            }
            btn.set_sensitive(false);
            gen_status.set_text("Asking the AI for variations…");
            let (tx, rx) = async_channel::bounded(1);
            let n = variation_count.clamp(1, 15).min(recipients.max(2) - 1);
            std::thread::spawn(move || {
                let _ = tx.send_blocking(generate_variations(&text, n).map(|v| (text, v)));
            });
            let btn = btn.clone();
            let gen_status = gen_status.clone();
            let variants_box = variants_box.clone();
            let views = views.clone();
            let sync = sync.clone();
            glib::MainContext::default().spawn_local(async move {
                btn.set_sensitive(true);
                btn.set_label("Regenerate");
                match rx.recv().await {
                    Ok(Ok((original, list))) => {
                        while let Some(c) = variants_box.first_child() {
                            variants_box.remove(&c);
                        }
                        let mut all = vec![original];
                        all.extend(list);
                        let mut v = views.borrow_mut();
                        v.clear();
                        for (i, t) in all.iter().enumerate() {
                            variants_box.append(&heading(&if i == 0 {
                                "Original".to_string()
                            } else {
                                format!("Variation {i}")
                            }));
                            let tv = TextView::new();
                            tv.set_wrap_mode(gtk4::WrapMode::WordChar);
                            tv.set_left_margin(8);
                            tv.set_right_margin(8);
                            tv.set_top_margin(6);
                            tv.set_bottom_margin(6);
                            tv.buffer().set_text(t);
                            let f = gtk4::Frame::new(None);
                            f.set_child(Some(&tv));
                            variants_box.append(&f);
                            v.push(tv);
                        }
                        gen_status.set_text(&format!(
                            "{} versions, sent in rotation. Edit any of them before sending.",
                            all.len()
                        ));
                    }
                    Ok(Err(e)) => gen_status.set_text(&e),
                    Err(_) => gen_status.set_text("AI request did not finish."),
                }
                drop(views);
                sync();
            });
        });
    }

    {
        let dialog_c = dialog.clone();
        send_btn.connect_clicked(move |_| {
            let text = buffer_text(&text_view);
            if text.trim().is_empty() {
                return;
            }
            let variants: Vec<String> = if reword.is_active() {
                variant_views
                    .borrow()
                    .iter()
                    .map(buffer_text)
                    .filter(|t| !t.trim().is_empty())
                    .collect()
            } else {
                Vec::new()
            };
            dialog_c.close();
            on_send(text, variants);
        });
    }

    present(&dialog, &body, parent);
}

/// Per-recipient progress with the two follow-on actions: send to whoever has
/// not had it yet, and follow up whoever has not replied.
pub fn show_status_dialog(
    parent: &gtk4::Widget,
    list_name: &str,
    rows: Vec<CampaignRow>,
    on_continue: impl Fn(Vec<String>) + 'static,
    on_follow_up: impl Fn(Vec<String>) + 'static,
) {
    let total = rows.len();
    let not_sent: Vec<String> = rows
        .iter()
        .filter(|r| r.sent_at.is_none())
        .map(|r| r.chat_id.clone())
        .collect();
    let no_reply: Vec<String> = rows
        .iter()
        .filter(|r| r.sent_at.is_some() && r.replied_at.is_none())
        .map(|r| r.chat_id.clone())
        .collect();
    let replied = rows.iter().filter(|r| r.replied_at.is_some()).count();
    let followed = rows.iter().filter(|r| r.followed_up_at.is_some()).count();

    let dialog = adw::Dialog::new();
    dialog.set_title(list_name);
    dialog.set_content_width(520);
    dialog.set_content_height(700);

    let body = Box::new(Orientation::Vertical, 10);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(16);
    body.set_margin_end(16);

    let summary = Label::new(Some(&format!(
        "Sent {} of {total} · Replied {replied} · No reply {} · Not sent {}{}",
        total - not_sent.len(),
        no_reply.len(),
        not_sent.len(),
        if followed > 0 {
            format!(" · Followed up {followed}")
        } else {
            String::new()
        }
    )));
    summary.set_wrap(true);
    summary.set_halign(Align::Start);
    summary.add_css_class("heading");
    body.append(&summary);

    let list = gtk4::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk4::SelectionMode::None);
    let mut sorted = rows;
    // Most actionable first: not sent, then no reply, then replied.
    sorted.sort_by_key(|r| {
        (
            match (r.sent_at, r.replied_at) {
                (None, _) => 0,
                (Some(_), None) => 1,
                _ => 2,
            },
            r.name.to_lowercase(),
        )
    });
    for r in &sorted {
        let row = Box::new(Orientation::Horizontal, 8);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.set_margin_start(10);
        row.set_margin_end(10);
        let name = Label::new(Some(&r.name));
        name.set_hexpand(true);
        name.set_halign(Align::Start);
        name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        row.append(&name);
        let (text, class) = match (r.sent_at, r.replied_at, r.followed_up_at) {
            (None, _, _) => ("Not sent".to_string(), "warning"),
            (Some(_), Some(at), _) => (format!("Replied {}", when(at)), "success"),
            (Some(_), None, Some(at)) => (format!("Followed up {}", when(at)), "dim-label"),
            (Some(at), None, None) => (format!("Sent {}", when(at)), "dim-label"),
        };
        let state = Label::new(Some(&text));
        state.add_css_class(class);
        state.add_css_class("caption");
        row.append(&state);
        list.append(&row);
    }
    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    body.append(&scroll);

    let actions = Box::new(Orientation::Horizontal, 8);
    actions.set_homogeneous(true);
    let continue_btn = Button::with_label(&format!("Continue — {} not sent", not_sent.len()));
    continue_btn.set_sensitive(!not_sent.is_empty());
    continue_btn.add_css_class("pill");
    let follow_btn = Button::with_label(&format!("Follow up — {} no reply", no_reply.len()));
    follow_btn.set_sensitive(!no_reply.is_empty());
    follow_btn.add_css_class("pill");
    follow_btn.add_css_class("suggested-action");
    actions.append(&continue_btn);
    actions.append(&follow_btn);
    body.append(&actions);

    {
        let dialog_c = dialog.clone();
        continue_btn.connect_clicked(move |_| {
            dialog_c.close();
            on_continue(not_sent.clone());
        });
    }
    {
        let dialog_c = dialog.clone();
        follow_btn.connect_clicked(move |_| {
            dialog_c.close();
            on_follow_up(no_reply.clone());
        });
    }

    present(&dialog, &body, parent);
}

fn when(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%b %-d, %-I:%M %p")
                .to_string()
        })
        .unwrap_or_default()
}

fn heading(text: &str) -> Label {
    let l = Label::new(Some(text));
    l.set_halign(Align::Start);
    l.add_css_class("heading");
    l
}

fn buffer_text(tv: &TextView) -> String {
    let b = tv.buffer();
    b.text(&b.start_iter(), &b.end_iter(), false).to_string()
}

fn present(dialog: &adw::Dialog, body: &Box, parent: &gtk4::Widget) {
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&adw::HeaderBar::new());
    tv.set_content(Some(body));
    dialog.set_child(Some(&tv));
    dialog.present(Some(parent));
}
