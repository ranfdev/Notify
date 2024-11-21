use std::cell::OnceCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gsv::prelude::*;
use gtk::{gio, glib};

use crate::error::*;
use crate::subscription::Subscription;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct AdvancedMessageDialog {
        pub subscription: OnceCell<Subscription>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AdvancedMessageDialog {
        const NAME: &'static str = "AdvancedMessageDialog";
        type Type = super::AdvancedMessageDialog;
        type ParentType = adw::Dialog;
    }

    impl ObjectImpl for AdvancedMessageDialog {}
    impl WidgetImpl for AdvancedMessageDialog {}
    impl AdwDialogImpl for AdvancedMessageDialog {}
}

glib::wrapper! {
    pub struct AdvancedMessageDialog(ObjectSubclass<imp::AdvancedMessageDialog>)
        @extends gtk::Widget, adw::Dialog;
}

impl AdvancedMessageDialog {
    pub fn new(subscription: Subscription, message: String) -> Self {
        let this: Self = glib::Object::new();
        this.imp().subscription.set(subscription).unwrap();
        this.build_ui(
            this.imp().subscription.get().unwrap().topic().clone(),
            message,
        );
        this
    }
    fn build_ui(&self, topic: String, message: String) {
        self.set_title("Advanced Message");
        self.set_content_height(480);
        self.set_content_width(480);
        let this = self.clone();
        relm4_macros::view! {
            content = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},
                #[wrap(Some)]
                set_content: toast_overlay = &adw::ToastOverlay {
                    #[wrap(Some)]
                    set_child = &gtk::ScrolledWindow {
                        #[wrap(Some)]
                        set_child = &gtk::Box {
                            set_margin_top: 8,
                            set_margin_bottom: 8,
                            set_margin_start: 8,
                            set_margin_end: 8,
                            set_spacing: 8,
                            set_orientation: gtk::Orientation::Vertical,
                            append = &gtk::Label {
                                set_label: "Here you can manually build the JSON message you want to POST to this topic",
                                set_natural_wrap_mode: gtk::NaturalWrapMode::None,
                                set_xalign: 0.0,
                                set_halign: gtk::Align::Start,
                                set_wrap_mode: gtk::pango::WrapMode::WordChar,
                                set_wrap: true,
                            },
                            append = &gtk::Label {
                                add_css_class: "heading",
                                set_label: "JSON",
                                set_xalign: 0.0,
                                set_halign: gtk::Align::Start,
                            },
                            append: text_view = &gsv::View {
                                add_css_class: "code",
                                set_tab_width: 4,
                                set_indent_width: 2,
                                set_auto_indent: true,
                                set_top_margin: 4,
                                set_bottom_margin: 4,
                                set_left_margin: 4,
                                set_right_margin: 4,
                                set_hexpand: true,
                                set_vexpand: true,
                                set_monospace: true,
                                set_background_pattern: gsv::BackgroundPatternType::Grid
                            },
                            append = &gtk::Label {
                                add_css_class: "heading",
                                set_label: "Snippets",
                                set_xalign: 0.0,
                                set_halign: gtk::Align::Start,
                            },
                            append = &gtk::FlowBox {
                                set_column_spacing: 4,
                                set_row_spacing: 4,
                                append = &gtk::Button {
                                    add_css_class: "pill",
                                    add_css_class: "small",
                                    set_label: "Title",
                                    connect_clicked[text_view] => move |_| {
                                        text_view.buffer().insert_at_cursor(r#""title": "Title of your message""#)
                                    }
                                },
                                append = &gtk::Button {
                                    add_css_class: "pill",
                                    add_css_class: "small",
                                    set_label: "Tags",
                                    connect_clicked[text_view] => move |_| {
                                        text_view.buffer().insert_at_cursor(r#""tags": ["warning","cd"]"#)
                                    }
                                },
                                append = &gtk::Button {
                                    add_css_class: "pill",
                                    add_css_class: "small",
                                    set_label: "Priority",
                                    connect_clicked[text_view] => move |_| {
                                        text_view.buffer().insert_at_cursor(r#""priority": 5"#)
                                    }
                                },
                                append = &gtk::Button {
                                    add_css_class: "pill",
                                    add_css_class: "small",
                                    set_label: "View Action",
                                    connect_clicked[text_view] => move |_| {
                                        text_view.buffer().insert_at_cursor(r#""actions": [
    {
      "action": "view",
      "label": "torvalds boosted your toot",
      "url": "https://joinmastodon.org"
    }
  ]"#)
                                    }
                                },
                                append = &gtk::Button {
                                    add_css_class: "pill",
                                    add_css_class: "small",
                                    set_label: "HTTP Action",
                                    connect_clicked[text_view] => move |_| {
                                        text_view.buffer().insert_at_cursor(r#""actions": [
    {
      "action": "http",
      "label": "Turn off lights",
      "method": "post",
      "url": "https://api.example.com/lights",
      "body": "OFF"
    }
  ]"#)
                                    }
                                },
                                append = &gtk::Button {
                                    add_css_class: "circular",
                                    add_css_class: "small",
                                    set_label: "?",
                                    connect_clicked => move |_| {
                                        gtk::UriLauncher::new("https://docs.ntfy.sh/publish/#publish-as-json").launch(
                                            None::<&gtk::Window>,
                                            gio::Cancellable::NONE,
                                            |_| {}
                                        );
                                    }
                                },
                            },
                            append = &gtk::Button {
                                set_margin_top: 8,
                                set_margin_bottom: 8,
                                add_css_class: "suggested-action",
                                add_css_class: "pill",
                                set_label: "Send",
                                connect_clicked[this, toast_overlay, text_view] => move |_| {
                                    let thisc = this.clone();
                                    let text_view = text_view.clone();
                                    let f = async move {
                                        let buffer = text_view.buffer();
                                        let msg = serde_json::from_str(&buffer.text(
                                            &mut buffer.start_iter(),
                                            &mut buffer.end_iter(),
                                            true,
                                        ))?;
                                        thisc.imp().subscription.get().unwrap()
                                            .publish_msg(msg).await
                                    };
                                    toast_overlay.error_boundary().spawn(f);
                                }
                            }
                        }
                    }
                }
            }
        }

        let lang = gsv::LanguageManager::default().language("json").unwrap();
        let buffer = gsv::Buffer::with_language(&lang);
        buffer.set_text(&format!(
            r#"{{
  "topic": "{topic}",
  "message": "{message}"
}}"#
        ));
        text_view.set_buffer(Some(&buffer));

        let manager = adw::StyleManager::default();
        let scheme_name = if manager.is_dark() {
            "solarized-dark"
        } else {
            "solarized-light"
        };
        let scheme = gsv::StyleSchemeManager::default().scheme(scheme_name);
        buffer.set_style_scheme(scheme.as_ref());
        this.set_child(Some(&content));
    }
}
