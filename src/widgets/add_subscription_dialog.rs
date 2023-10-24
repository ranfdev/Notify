use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::once_cell::sync::Lazy;
use glib::subclass::Signal;
use gtk::gio;
use gtk::glib;
use ntfy_daemon::models;

mod imp {
    pub use super::*;
    #[derive(Debug, Default)]
    pub struct AddSubscriptionDialog {
        pub topic_entry: RefCell<adw::EntryRow>,
        pub server_entry: RefCell<adw::EntryRow>,
        pub sub_btn: RefCell<gtk::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AddSubscriptionDialog {
        const NAME: &'static str = "AddSubscriptionDialog";
        type Type = super::AddSubscriptionDialog;
        type ParentType = adw::Window;

        fn class_init(klass: &mut Self::Class) {
            klass.add_binding_action(
                gtk::gdk::Key::Escape,
                gtk::gdk::ModifierType::empty(),
                "window.close",
                None,
            );
            klass.install_action("default.activate", None, |this, _, _| {
                this.emit_subscribe_request();
                this.close();
            });
        }
    }

    impl ObjectImpl for AddSubscriptionDialog {
        fn signals() -> &'static [Signal] {
            static SIGNALS: Lazy<Vec<Signal>> =
                Lazy::new(|| vec![Signal::builder("subscribe-request").build()]);
            SIGNALS.as_ref()
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj().clone();
            obj.build_ui();
        }
    }
    impl WidgetImpl for AddSubscriptionDialog {}
    impl WindowImpl for AddSubscriptionDialog {}
    impl AdwWindowImpl for AddSubscriptionDialog {}
}

glib::wrapper! {
    pub struct AddSubscriptionDialog(ObjectSubclass<imp::AddSubscriptionDialog>)
        @extends gtk::Widget, gtk::Window, adw::Window,
        @implements gio::ActionMap, gio::ActionGroup, gtk::Root;
}

impl AddSubscriptionDialog {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }
    fn build_ui(&self) {
        let imp = self.imp();
        let obj = self.clone();
        obj.set_title(Some("Subscribe To Topic"));
        obj.set_modal(true);
        obj.set_default_width(360);

        relm4_macros::view! {
            toolbar_view = adw::ToolbarView {
                add_top_bar: &adw::HeaderBar::new(),
                #[wrap(Some)]
                set_content = &adw::Clamp {
                    #[wrap(Some)]
                    set_child = &gtk::Box {
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
                            append = &adw::ExpanderRow {
                                set_title: "Custom server...",
                                set_enable_expansion: false,
                                set_show_enable_switch: true,
                                add_row: server_entry = &adw::EntryRow {
                                    set_title: "Server",
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
                                obj.close();
                            }
                        }
                    },
                },
            },
        }

        let (tx, rx) = glib::MainContext::channel(Default::default());
        let txc = tx.clone();
        topic_entry.delegate().unwrap().connect_changed(move |_| {
            txc.send(()).unwrap();
        });
        let rx = crate::async_utils::debounce_channel(std::time::Duration::from_millis(500), rx);
        let objc = obj.clone();
        rx.attach(None, move |_| {
            objc.check_errors();
            glib::ControlFlow::Continue
        });
        imp.topic_entry.replace(topic_entry);
        imp.server_entry.replace(server_entry);
        imp.sub_btn.replace(sub_btn);

        obj.set_content(Some(&toolbar_view));
    }
    pub fn topic(&self) -> String {
        self.imp().topic_entry.borrow().text().to_string()
    }
    pub fn server(&self) -> String {
        self.imp().server_entry.borrow().text().to_string()
    }
    fn check_errors(&self) {
        let imp = self.imp();
        let topic_entry = imp.topic_entry.borrow().clone();
        let sub_btn = imp.sub_btn.borrow().clone();
        if let Err(_) = models::validate_topic(&topic_entry.delegate().unwrap().text()) {
            topic_entry.add_css_class("error");
            sub_btn.set_sensitive(false);
        } else {
            topic_entry.remove_css_class("error");
            sub_btn.set_sensitive(true);
        }
    }
    fn emit_subscribe_request(&self) {
        self.emit_by_name::<()>("subscribe-request", &[]);
    }
}
