use std::cell::Cell;
use std::cell::OnceCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use futures::prelude::*;
use gsv::prelude::*;
use gtk::{gio, glib};
use ntfy_daemon::models;
use ntfy_daemon::ntfy_capnp::{system_notifier, Status};
use tracing::warn;

use crate::application::NotifyApplication;
use crate::config::{APP_ID, PROFILE};
use crate::subscription::Subscription;
use crate::widgets::*;

pub trait SpawnWithToast {
    fn spawn_with_near_toast<T, R: std::fmt::Display>(
        &self,
        f: impl Future<Output = Result<T, R>> + 'static,
    );
}

impl<W: glib::IsA<gtk::Widget>> SpawnWithToast for W {
    fn spawn_with_near_toast<T, R: std::fmt::Display>(
        &self,
        f: impl Future<Output = Result<T, R>> + 'static,
    ) {
        let toast_overlay: Option<adw::ToastOverlay> = self
            .ancestor(adw::ToastOverlay::static_type())
            .and_downcast();
        let win: Option<NotifyWindow> = self.ancestor(NotifyWindow::static_type()).and_downcast();
        glib::MainContext::default().spawn_local(async move {
            if let Err(e) = f.await {
                if let Some(o) = toast_overlay
                    .as_ref()
                    .or_else(|| win.as_ref().map(|win| win.imp().toast_overlay.as_ref()))
                {
                    o.add_toast(adw::Toast::builder().title(&e.to_string()).build())
                }
            }
        });
    }
}

mod imp {
    use super::*;

    #[derive(gtk::CompositeTemplate)]
    #[template(resource = "/com/ranfdev/Notify/ui/window.ui")]
    pub struct NotifyWindow {
        #[template_child]
        pub headerbar: TemplateChild<adw::HeaderBar>,
        #[template_child]
        pub message_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub subscription_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub entry: TemplateChild<gtk::Entry>,
        #[template_child]
        pub navigation_split_view: TemplateChild<adw::NavigationSplitView>,
        #[template_child]
        pub subscription_view: TemplateChild<adw::ToolbarView>,
        #[template_child]
        pub subscription_menu_btn: TemplateChild<gtk::MenuButton>,
        pub subscription_list_model: gio::ListStore,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub welcome_view: TemplateChild<adw::StatusPage>,
        #[template_child]
        pub list_view: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub message_scroll: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub banner: TemplateChild<adw::Banner>,
        #[template_child]
        pub send_btn: TemplateChild<gtk::Button>,
        #[template_child]
        pub code_btn: TemplateChild<gtk::Button>,
        pub notifier: OnceCell<system_notifier::Client>,
        pub conn: OnceCell<gio::SocketConnection>,
        pub settings: gio::Settings,
        pub banner_binding: Cell<Option<(Subscription, glib::SignalHandlerId)>>,
    }

    impl Default for NotifyWindow {
        fn default() -> Self {
            let this = Self {
                headerbar: Default::default(),
                message_list: Default::default(),
                entry: Default::default(),
                subscription_view: Default::default(),
                navigation_split_view: Default::default(),
                subscription_menu_btn: Default::default(),
                subscription_list: Default::default(),
                toast_overlay: Default::default(),
                stack: Default::default(),
                welcome_view: Default::default(),
                list_view: Default::default(),
                message_scroll: Default::default(),
                banner: Default::default(),
                subscription_list_model: gio::ListStore::new::<Subscription>(),
                settings: gio::Settings::new(APP_ID),
                notifier: Default::default(),
                conn: Default::default(),
                banner_binding: Default::default(),
                send_btn: Default::default(),
                code_btn: Default::default(),
            };

            this
        }
    }

    #[gtk::template_callbacks]
    impl NotifyWindow {
        #[template_callback]
        fn show_add_topic(&self, _btn: &gtk::Button) {
            let dialog = AddSubscriptionDialog::new();
            dialog.set_transient_for(Some(&self.obj().clone()));
            dialog.present();

            let this = self.obj().clone();
            let dc = dialog.clone();
            dialog.connect_local("subscribe-request", true, move |_| {
                let sub = match dc.subscription() {
                    Ok(sub) => sub,
                    Err(e) => {
                        warn!(errors = ?e, "trying to add invalid subscription");
                        return None;
                    }
                };
                this.add_subscription(sub);
                dc.close();
                None
            });
        }
        #[template_callback]
        fn discover_integrations(&self, _btn: &gtk::Button) {
            gtk::UriLauncher::new("https://docs.ntfy.sh/integrations/").launch(
                Some(&self.obj().clone()),
                gio::Cancellable::NONE,
                |_| {},
            );
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NotifyWindow {
        const NAME: &'static str = "NotifyWindow";
        type Type = super::NotifyWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.bind_template_callbacks();

            klass.install_action("win.unsubscribe", None, |this, _, _| {
                this.unsubscribe();
            });
            klass.install_action("win.show-subscription-info", None, |this, _, _| {
                this.show_subscription_info();
            });
            klass.install_action("win.clear-notifications", None, |this, _, _| {
                this.selected_subscription().map(|sub| {
                    this.spawn_with_near_toast(sub.clear_notifications());
                });
            });
            //klass.bind_template_instance_callbacks();
        }

        // You must call `Widget`'s `init_template()` within `instance_init()`.
        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for NotifyWindow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            // Devel Profile
            if PROFILE == "Devel" {
                obj.add_css_class("devel");
            }
        }

        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl WidgetImpl for NotifyWindow {}
    impl WindowImpl for NotifyWindow {
        // Save window state on delete event
        fn close_request(&self) -> glib::Propagation {
            if let Err(err) = self.obj().save_window_size() {
                warn!(error = %err, "Failed to save window state");
            }

            // Pass close request on to the parent
            self.parent_close_request()
        }
    }

    impl ApplicationWindowImpl for NotifyWindow {}
    impl AdwApplicationWindowImpl for NotifyWindow {}
}

glib::wrapper! {
    pub struct NotifyWindow(ObjectSubclass<imp::NotifyWindow>)
        @extends gtk::Widget, gtk::Window, adw::Window, adw::ApplicationWindow,
        @implements gio::ActionMap, gio::ActionGroup, gtk::Root;
}

impl NotifyWindow {
    pub fn new(app: &NotifyApplication, notifier: system_notifier::Client) -> Self {
        let obj: Self = glib::Object::builder().property("application", app).build();

        if let Err(_) = obj.imp().notifier.set(notifier) {
            panic!("setting notifier for first time");
        };

        // Load latest window state
        obj.load_window_size();
        obj.bind_message_list();
        obj.connect_entry_and_send_btn();
        obj.connect_code_btn();
        obj.connect_items_changed();
        obj.selected_subscription_changed(None);
        obj.bind_flag_read();

        obj
    }
    fn connect_entry_and_send_btn(&self) {
        let imp = self.imp();
        let this = self.clone();
        let entry = imp.entry.clone();
        let publish = move || {
            let p = this
                .selected_subscription()
                .unwrap()
                .publish_msg(models::Message {
                    message: Some(entry.text().as_str().to_string()),
                    ..models::Message::default()
                });

            entry.spawn_with_near_toast(async move { p.await });
        };
        let publishc = publish.clone();
        imp.entry.connect_activate(move |_| publishc());
        imp.send_btn.connect_clicked(move |_| publish());
    }
    fn connect_code_btn(&self) {
        let imp = self.imp();
        let this = self.clone();
        imp.code_btn.connect_clicked(move |_| {
            this.show_docs_dialog();
        });
    }
    fn show_docs_dialog(&self) {
        let imp = self.imp();
        let this = self.clone();
        let topic = self.selected_subscription().unwrap().topic();
        let message = imp.entry.text();
        relm4_macros::view! {
            window = adw::Window {
                set_default_height: 400,
                set_modal: true,
                set_transient_for: Some(self),
                #[wrap(Some)]
                set_content = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {},
                    #[wrap(Some)]
                    set_content: toast_overlay = &adw::ToastOverlay {
                        #[wrap(Some)]
                        set_child = &adw::Clamp {
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
                                append = &gtk::ScrolledWindow {
                                    #[wrap(Some)]
                                    set_child: text_view = &gsv::View {
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
                                        connect_clicked[this] => move |_| {
                                            gtk::UriLauncher::new("https://docs.ntfy.sh/publish/#publish-as-json").launch(
                                                Some(&this),
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
                                            )).map_err(|e| capnp::Error::failed(e.to_string()))?;
                                            thisc.selected_subscription()
                                                .unwrap()
                                                .publish_msg(msg).await
                                        };
                                        toast_overlay.spawn_with_near_toast(f);
                                    }
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

        window.present();
    }
    fn show_subscription_info(&self) {
        let sub = SubscriptionInfoDialog::new(self.selected_subscription().unwrap());
        sub.set_transient_for(Some(self));
        sub.present();
    }
    fn connect_items_changed(&self) {
        let this = self.clone();
        self.imp()
            .subscription_list_model
            .connect_items_changed(move |list, _, _, _| {
                let imp = this.imp();
                if list.n_items() == 0 {
                    imp.stack.set_visible_child(&*imp.welcome_view);
                } else {
                    imp.stack.set_visible_child(&*imp.list_view);
                }
            });
    }

    fn add_subscription(&self, sub: models::Subscription) {
        let mut req = self.notifier().subscribe_request();

        req.get().set_server(&sub.server);
        req.get().set_topic(&sub.topic);
        let res = req.send();
        let this = self.clone();
        self.spawn_with_near_toast(async move {
            let imp = this.imp();

            // Subscription::new will use the pipelined client to retrieve info about the subscription
            let subscription = Subscription::new(res.pipeline.get_subscription());
            // We want to still check if there were any errors adding the subscription.
            res.promise.await?;

            imp.subscription_list_model.append(&subscription);
            let i = imp.subscription_list_model.n_items() - 1;
            let row = imp.subscription_list.row_at_index(i as i32);
            imp.subscription_list.select_row(row.as_ref());
            Ok::<(), capnp::Error>(())
        });
    }

    fn unsubscribe(&self) {
        let mut req = self.notifier().unsubscribe_request();

        let sub = self.selected_subscription().unwrap();

        req.get().set_server(&sub.server());
        req.get().set_topic(&sub.topic());

        let res = req.send();
        let this = self.clone();

        self.spawn_with_near_toast(async move {
            let imp = this.imp();
            res.promise.await?;

            if let Some(i) = imp.subscription_list_model.find(&sub) {
                imp.subscription_list_model.remove(i);
            }
            Ok::<(), capnp::Error>(())
        });
    }
    fn notifier(&self) -> &system_notifier::Client {
        self.imp().notifier.get().unwrap()
    }
    fn selected_subscription(&self) -> Option<Subscription> {
        let imp = self.imp();
        imp.subscription_list
            .selected_row()
            .and_then(|row| imp.subscription_list_model.item(row.index() as u32))
            .and_downcast::<Subscription>()
    }
    fn bind_message_list(&self) {
        let imp = self.imp();

        imp.subscription_list
            .bind_model(Some(&imp.subscription_list_model), |obj| {
                let sub = obj.downcast_ref::<Subscription>().unwrap();

                Self::build_subscription_ui(&sub).upcast()
            });

        let this = self.clone();
        imp.subscription_list.connect_row_selected(move |_, _row| {
            this.selected_subscription_changed(this.selected_subscription().as_ref());
        });

        let this = self.clone();
        let req = self.notifier().list_subscriptions_request();
        let res = req.send();
        self.spawn_with_near_toast(async move {
            let list = res.promise.await?;
            let list = list.get()?.get_list()?;
            let imp = this.imp();
            for sub in list {
                imp.subscription_list_model.append(&Subscription::new(sub?));
            }
            Ok::<(), capnp::Error>(())
        });
    }
    fn update_banner(&self, sub: Option<&Subscription>) {
        let imp = self.imp();
        if let Some(sub) = sub {
            match sub.nice_status() {
                Status::Degraded | Status::Down => imp.banner.set_revealed(true),
                Status::Up => imp.banner.set_revealed(false),
            }
        } else {
            imp.banner.set_revealed(false);
        }
    }
    fn selected_subscription_changed(&self, sub: Option<&Subscription>) {
        let imp = self.imp();
        self.update_banner(sub);
        if let Some((sub, id)) = imp.banner_binding.take() {
            sub.disconnect(id);
        }
        if let Some(sub) = sub {
            imp.navigation_split_view.set_show_content(true);
            imp.message_list
                .bind_model(Some(&sub.imp().messages), move |obj| {
                    let b = obj.downcast_ref::<glib::BoxedAnyObject>().unwrap();
                    let msg = b.borrow::<models::Message>();

                    MessageRow::new(msg.clone()).upcast()
                });
            imp.subscription_menu_btn.set_sensitive(true);
            imp.send_btn.set_sensitive(true);
            imp.code_btn.set_sensitive(true);
            imp.entry.set_sensitive(true);

            let this = self.clone();
            imp.banner_binding.set(Some((
                sub.clone(),
                sub.connect_status_notify(move |sub| {
                    this.update_banner(Some(sub));
                }),
            )));

            let this = self.clone();
            glib::idle_add_local_once(move || {
                this.flag_read();
            });
        } else {
            imp.message_list
                .bind_model(gio::ListModel::NONE, |_| adw::Bin::new().into());
            imp.subscription_menu_btn.set_sensitive(false);
            imp.code_btn.set_sensitive(false);
            imp.send_btn.set_sensitive(false);
            imp.entry.set_sensitive(false);
        }
    }
    fn flag_read(&self) {
        let vadj = self.imp().message_scroll.vadjustment();
        // There is nothing to scroll, so the user viewed all the messages
        if vadj.page_size() == vadj.upper()
            || ((vadj.page_size() + vadj.value() - vadj.upper()).abs() <= 1.0)
        {
            self.selected_subscription().map(|sub| {
                self.spawn_with_near_toast(sub.flag_all_as_read());
            });
        }
    }
    fn build_chip(text: &str) -> gtk::Label {
        let chip = gtk::Label::new(Some(text));
        chip.add_css_class("chip");
        chip.add_css_class("chip--small");
        chip.set_margin_top(4);
        chip.set_margin_bottom(4);
        chip.set_margin_start(4);
        chip.set_margin_end(4);
        chip.set_halign(gtk::Align::Center);
        chip.set_valign(gtk::Align::Center);
        chip
    }

    fn build_subscription_ui(sub: &Subscription) -> impl glib::IsA<gtk::Widget> {
        let b = gtk::Box::builder().spacing(4).build();

        let label = gtk::Label::builder()
            .xalign(0.0)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .wrap(true)
            .hexpand(true)
            .build();

        sub.bind_property("display-name", &label, "label")
            .sync_create()
            .build();

        let counter_chip = Self::build_chip("●");
        counter_chip.add_css_class("chip--info");
        counter_chip.add_css_class("circular");
        counter_chip.set_visible(false);
        let counter_chip_clone = counter_chip.clone();
        sub.connect_unread_count_notify(move |sub| {
            let c = sub.unread_count();
            counter_chip_clone.set_visible(c > 0);
        });

        let status_chip = Self::build_chip("Degraded");
        let status_chip_clone = status_chip.clone();

        sub.connect_status_notify(move |sub| match sub.nice_status() {
            Status::Degraded | Status::Down => {
                status_chip_clone.add_css_class("chip--degraded");
                status_chip_clone.set_visible(true);
            }
            _ => {
                status_chip_clone.set_visible(false);
            }
        });

        b.append(&counter_chip);
        b.append(&label);
        b.append(&status_chip);

        b
    }

    fn save_window_size(&self) -> Result<(), glib::BoolError> {
        let imp = self.imp();

        let (width, height) = self.default_size();

        imp.settings.set_int("window-width", width)?;
        imp.settings.set_int("window-height", height)?;

        imp.settings
            .set_boolean("is-maximized", self.is_maximized())?;

        Ok(())
    }
    fn bind_flag_read(&self) {
        let imp = self.imp();

        let this = self.clone();
        imp.message_scroll.connect_edge_reached(move |_, pos_type| {
            if pos_type == gtk::PositionType::Bottom {
                this.flag_read();
            }
        });
        let this = self.clone();
        self.connect_is_active_notify(move |_| {
            if this.is_active() {
                this.flag_read();
            }
        });
    }

    fn load_window_size(&self) {
        let imp = self.imp();

        let width = imp.settings.int("window-width");
        let height = imp.settings.int("window-height");
        let is_maximized = imp.settings.boolean("is-maximized");

        self.set_default_size(width, height);

        if is_maximized {
            self.maximize();
        }
    }
}
