use std::cell::OnceCell;
use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::subclass::Signal;
use gtk::gio;
use gtk::glib;
use ntfy_daemon::models;
use once_cell::sync::Lazy;

#[derive(Default, Debug, Clone)]
pub struct Widgets {
    pub topic_entry: adw::EntryRow,
    pub server_entry: adw::EntryRow,
    pub server_expander: adw::ExpanderRow,
    pub sub_btn: gtk::Button,
}
mod imp {
    pub use super::*;
    #[derive(Debug, Default)]
    pub struct AddSubscriptionDialog {
        pub widgets: RefCell<Widgets>,
        pub init_custom_server: OnceCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AddSubscriptionDialog {
        const NAME: &'static str = "AddSubscriptionDialog";
        type Type = super::AddSubscriptionDialog;
        type ParentType = adw::Dialog;

        fn class_init(klass: &mut Self::Class) {
            klass.add_binding_action(
                gtk::gdk::Key::Escape,
                gtk::gdk::ModifierType::empty(),
                "window.close",
            );
            klass.install_action("default.activate", None, |this, _, _| {
                this.emit_subscribe_request();
            });
        }
    }

    impl ObjectImpl for AddSubscriptionDialog {
        fn signals() -> &'static [Signal] {
            static SIGNALS: Lazy<Vec<Signal>> =
                Lazy::new(|| vec![Signal::builder("subscribe-request").build()]);
            SIGNALS.as_ref()
        }
    }
    impl WidgetImpl for AddSubscriptionDialog {}
    impl AdwDialogImpl for AddSubscriptionDialog {}
}

glib::wrapper! {
    pub struct AddSubscriptionDialog(ObjectSubclass<imp::AddSubscriptionDialog>)
        @extends gtk::Widget, adw::Dialog,
        @implements gio::ActionMap, gio::ActionGroup, gtk::Root;
}

impl AddSubscriptionDialog {
    pub fn new(custom_server: Option<String>) -> Self {
        let this: Self = glib::Object::builder().build();
        if let Some(s) = custom_server {
            if s != ntfy_daemon::models::DEFAULT_SERVER {
                this.imp().init_custom_server.set(s).unwrap();
            }
        }
        this.build_ui();
        this
    }
    fn build_ui(&self) {
        let imp = self.imp();
        let obj = self.clone();
        obj.set_title("Subscribe To Topic");

        relm4_macros::view! {
            toolbar_view = adw::ToolbarView {
                add_top_bar: &adw::HeaderBar::new(),
                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 12,
                    set_margin_end: 12,
                    set_margin_start: 12,
                    set_margin_top: 12,
                    set_margin_bottom: 12,
                    append = &gtk::Label {
                        add_css_class: "dim-label",
                        set_label: "Topics may not be password-protected, so choose a name that's not easy to guess. \
                            Once subscribed, you can PUT/POST notifications.",
                        set_wrap: true,
                        set_xalign: 0.0,
                        set_wrap_mode: gtk::pango::WrapMode::WordChar
                    },
                    append = &gtk::ListBox {
                        add_css_class: "boxed-list",
                        append: topic_entry = &adw::EntryRow {
                            set_title: "Topic",
                            set_activates_default: true,
                            add_suffix = &gtk::Button {
                                set_icon_name: "dice3-symbolic",
                                set_tooltip_text: Some("Generate name"),
                                set_valign: gtk::Align::Center,
                                add_css_class: "flat",
                                connect_clicked[topic_entry] => move |_| {
                                    use rand::distributions::Alphanumeric;
                                    use rand::{thread_rng, Rng};
                                    let mut rng = thread_rng();
                                    let chars: String = (0..10).map(|_| rng.sample(Alphanumeric) as char).collect();
                                    topic_entry.set_text(&chars);
                                }
                            }
                        },
                        append: server_expander = &adw::ExpanderRow {
                            set_title: "Custom server...",
                            set_enable_expansion: imp.init_custom_server.get().is_some(),
                            set_expanded: imp.init_custom_server.get().is_some(),
                            set_show_enable_switch: true,
                            add_row: server_entry = &adw::EntryRow {
                                set_title: "Server",
                                set_text: imp.init_custom_server.get().map(|x| x.as_str()).unwrap_or(""),
                            }
                        }
                    },
                    append: sub_btn = &gtk::Button {
                        set_label: "Subscribe",
                        add_css_class: "suggested-action",
                        add_css_class: "pill",
                        set_halign: gtk::Align::Center,
                        set_sensitive: false,
                        connect_clicked[obj] => move |_| {
                            obj.emit_subscribe_request();
                        }
                    }
                },
            },
        }

        let debounced_error_check = {
            let db = crate::async_utils::Debouncer::new();
            let objc = obj.clone();
            move || {
                db.call(std::time::Duration::from_millis(500), move || {
                    objc.check_errors()
                });
            }
        };

        let f = debounced_error_check.clone();
        topic_entry
            .delegate()
            .unwrap()
            .connect_changed(move |_| f.clone()());
        let f = debounced_error_check.clone();
        server_entry
            .delegate()
            .unwrap()
            .connect_changed(move |_| f.clone()());
        let f = debounced_error_check.clone();
        server_expander.connect_enable_expansion_notify(move |_| f.clone()());

        imp.widgets.replace(Widgets {
            topic_entry,
            server_expander,
            server_entry,
            sub_btn,
        });

        obj.set_content_width(480);
        obj.set_child(Some(&toolbar_view));
    }
    pub fn subscription(&self) -> Result<models::Subscription, Vec<ntfy_daemon::Error>> {
        let w = { self.imp().widgets.borrow().clone() };
        let mut sub = models::Subscription::builder(w.topic_entry.text().to_string());
        if w.server_expander.enables_expansion() {
            sub = sub.server(w.server_entry.text().to_string());
        }

        sub.build()
    }
    fn check_errors(&self) {
        let w = { self.imp().widgets.borrow().clone() };
        let sub = self.subscription();

        w.server_entry.remove_css_class("error");
        w.topic_entry.remove_css_class("error");
        w.sub_btn.set_sensitive(true);

        if let Err(errs) = sub {
            w.sub_btn.set_sensitive(false);
            for e in errs {
                match e {
                    ntfy_daemon::Error::InvalidTopic(_) => {
                        w.topic_entry.add_css_class("error");
                    }
                    ntfy_daemon::Error::InvalidServer(_) => {
                        w.server_entry.add_css_class("error");
                    }
                    _ => {}
                }
            }
        }
    }
    fn emit_subscribe_request(&self) {
        self.emit_by_name::<()>("subscribe-request", &[]);
    }
}
