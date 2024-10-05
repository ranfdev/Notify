use adw::prelude::*;
use adw::subclass::prelude::*;
use chrono::NaiveDateTime;
use gtk::{gio, glib};
use ntfy_daemon::models;
use tracing::error;

use crate::widgets::*;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct MessageRow {}

    #[glib::object_subclass]
    impl ObjectSubclass for MessageRow {
        const NAME: &'static str = "MessageRow";
        type Type = super::MessageRow;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for MessageRow {}

    impl WidgetImpl for MessageRow {}
    impl BinImpl for MessageRow {}
}

glib::wrapper! {
    pub struct MessageRow(ObjectSubclass<imp::MessageRow>)
        @extends gtk::Widget, adw::Bin;
}

impl MessageRow {
    pub fn new(msg: models::Message) -> Self {
        let this: Self = glib::Object::new();
        this.build_ui(msg);
        this
    }
    fn build_ui(&self, msg: models::Message) {
        let top_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);

        let time = gtk::Label::builder()
            .label(
                &NaiveDateTime::from_timestamp_opt(msg.time as i64, 0)
                    .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default(),
            )
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .wrap(true)
            .build();
        time.add_css_class("caption");

        top_box.append(&time);

        if let Some(p) = msg.priority {
            let text = format!(
                "Priority: {}",
                match p {
                    5 => "Max",
                    4 => "High",
                    3 => "Medium",
                    2 => "Low",
                    1 => "Min",
                    _ => "Invalid",
                }
            );
            let priority = gtk::Label::builder()
                .label(&text)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .wrap(true)
                .build();
            priority.add_css_class("caption");
            priority.add_css_class("chip");
            if p == 5 {
                priority.add_css_class("chip--danger")
            } else if p == 4 {
                priority.add_css_class("chip--warning")
            }
            top_box.append(&priority);
        }

        let b = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(8)
            .margin_end(8)
            .build();

        b.append(&top_box);
        if let Some(title) = msg.display_title() {
            let label = gtk::Label::builder()
                .label(&title)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .wrap(true)
                .selectable(true)
                .build();
            label.add_css_class("heading");
            b.append(&label);
        }

        if let Some(message) = msg.display_message() {
            let label = gtk::Label::builder()
                .label(&message)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .wrap(true)
                .selectable(true)
                .build();
            b.append(&label);
        }

        if msg.actions.len() > 0 {
            let action_btns = gtk::Box::builder().spacing(8).build();

            for a in msg.actions {
                let btn = self.build_action_btn(a);
                action_btns.append(&btn);
            }

            b.append(&action_btns);
        }
        if msg.tags.len() > 0 {
            let mut tags_text = String::from("tags: ");
            tags_text.push_str(&msg.tags.join(", "));
            let tags = gtk::Label::builder()
                .label(&tags_text)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .build();
            b.append(&tags);
        }

        self.set_child(Some(&b));
    }
    fn build_action_btn(&self, action: models::Action) -> gtk::Button {
        let btn = gtk::Button::new();
        match action {
            models::Action::View { label, url, .. } => {
                btn.set_label(&label);
                btn.set_tooltip_text(Some(&format!("Go to {url}")));
                btn.connect_clicked(move |_| {
                    gtk::UriLauncher::builder().uri(url.clone()).build().launch(
                        gtk::Window::NONE,
                        gio::Cancellable::NONE,
                        |_| {},
                    );
                });
            }
            models::Action::Http {
                label,
                method,
                url,
                body,
                headers,
                ..
            } => {
                btn.set_label(&label);
                btn.set_tooltip_text(Some(&format!("Send HTTP {method} to {url}")));
                let (tx, rx) = async_channel::unbounded();
                let this = self.clone();
                btn.connect_clicked({
                    let url = url.clone();
                    let method = method.clone();
                    move |_| {
                        let url = url.clone();
                        let method = method.clone();
                        let tx = tx.clone();
                        let body = body.clone();
                        let headers = headers.clone();
                        gio::spawn_blocking(move || {
                            let mut req = ureq::request(method.as_str(), url.as_str());
                            for (k, v) in headers.iter() {
                                req = req.set(&k, &v);
                            }
                            tx.send_blocking(req.send(body.as_bytes())).unwrap();
                        });
                    }
                });
                glib::MainContext::default().spawn_local(async move {
                    while let Ok(res) = rx.recv().await {
                        let method = method.clone();
                        let url = url.clone();
                        this.spawn_with_near_toast(async move {
                            match res {
                                Err(e) => {
                                    error!(error = ?e, "Error sending request");
                                    Err(format!("Error sending HTTP {method} to {url}"))
                                }
                                Ok(_) => Ok(()),
                            }
                        });
                    }
                });
            }
            models::Action::Broadcast { label, .. } => {
                btn.set_label(&label);
                btn.set_sensitive(false);
                btn.set_tooltip_text(Some("Broadcast action only available on Android"));
            }
        }
        btn
    }
}
