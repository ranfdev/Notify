use adw::prelude::*;
use adw::subclass::prelude::*;
use chrono::NaiveDateTime;
use gtk::glib;
use ntfy_daemon::models;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct MessageRow {}

    #[glib::object_subclass]
    impl ObjectSubclass for MessageRow {
        const NAME: &'static str = "MessageRow";
        type Type = super::MessageRow;
        type ParentType = gtk::Grid;
    }

    impl ObjectImpl for MessageRow {}

    impl WidgetImpl for MessageRow {}
    impl GridImpl for MessageRow {}
}

glib::wrapper! {
    pub struct MessageRow(ObjectSubclass<imp::MessageRow>)
        @extends gtk::Widget, gtk::Grid;
}

impl MessageRow {
    pub fn new(msg: models::Message) -> Self {
        let this: Self = glib::Object::new();
        this.build_ui(msg);
        this
    }
    fn build_ui(&self, msg: models::Message) {
        self.set_margin_top(8);
        self.set_margin_bottom(8);
        self.set_margin_start(8);
        self.set_margin_end(8);
        self.set_column_spacing(8);
        self.set_row_spacing(8);

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
        self.attach(&time, 0, 0, 1, 1);

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
            priority.set_halign(gtk::Align::End);
            self.attach(&priority, 1, 0, 2, 1);
        }

        if let Some(title) = msg.display_title() {
            let label = gtk::Label::builder()
                .label(&title)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .wrap(true)
                .selectable(true)
                .build();
            label.add_css_class("heading");
            self.attach(&label, 0, 1, 3, 1);
        }

        if let Some(message) = msg.display_message() {
            let label = gtk::Label::builder()
                .label(&message)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .wrap(true)
                .selectable(true)
                .hexpand(true)
                .build();
            self.attach(&label, 0, 2, 3, 1);
        }

        if msg.actions.len() > 0 {
            let action_btns = gtk::FlowBox::builder()
                .row_spacing(8)
                .column_spacing(8)
                .homogeneous(true)
                .selection_mode(gtk::SelectionMode::None)
                .build();

            for a in msg.actions {
                let btn = self.build_action_btn(a);
                action_btns.append(&btn);
            }

            self.attach(&action_btns, 0, 3, 3, 1);
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
            self.attach(&tags, 0, 4, 3, 1);
        }
    }
    fn build_action_btn(&self, action: models::Action) -> gtk::Button {
        let btn = gtk::Button::new();
        match &action {
            models::Action::View { label, url, .. } => {
                btn.set_label(&label);
                btn.set_tooltip_text(Some(&format!("Go to {url}")));
                btn.set_action_name(Some("app.message-action"));
                btn.set_action_target_value(Some(&serde_json::to_string(&action).unwrap().into()));
            }
            models::Action::Http {
                label, method, url, ..
            } => {
                btn.set_label(&label);
                btn.set_tooltip_text(Some(&format!("Send HTTP {method} to {url}")));
                btn.set_action_name(Some("app.message-action"));
                btn.set_action_target_value(Some(&serde_json::to_string(&action).unwrap().into()));
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
