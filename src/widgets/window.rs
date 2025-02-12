use std::cell::Cell;
use std::cell::OnceCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use ntfy_daemon::models;
use ntfy_daemon::NtfyHandle;
use tracing::warn;

use crate::application::NotifyApplication;
use crate::config::{APP_ID, PROFILE};
use crate::error::*;
use crate::subscription::Status;
use crate::subscription::Subscription;
use crate::widgets::*;

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
        pub notifier: OnceCell<NtfyHandle>,
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
            let this = self.obj().clone();
            let dialog =
                AddSubscriptionDialog::new(this.selected_subscription().map(|x| x.server()));
            dialog.present(Some(&self.obj().clone()));

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
                    this.error_boundary()
                        .spawn(async move { sub.clear_notifications().await });
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
    pub fn new(app: &NotifyApplication, notifier: NtfyHandle) -> Self {
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

        imp.entry.connect_activate(move |_| this.publish_msg());
        let this = self.clone();
        imp.send_btn.connect_clicked(move |_| this.publish_msg());
    }
    fn publish_msg(&self) {
        let entry = self.imp().entry.clone();
        let this = self.clone();

        entry.error_boundary().spawn(async move {
            this.selected_subscription()
                .unwrap()
                .publish_msg(models::OutgoingMessage {
                    message: Some(entry.text().as_str().to_string()),
                    ..models::OutgoingMessage::default()
                })
                .await?;
            Ok(())
        });
    }
    fn connect_code_btn(&self) {
        let imp = self.imp();
        let this = self.clone();
        imp.code_btn.connect_clicked(move |_| {
            let this = this.clone();
            this.selected_subscription().map(move |sub| {
                AdvancedMessageDialog::new(sub, this.imp().entry.text().to_string())
                    .present(Some(&this))
            });
        });
    }
    fn show_subscription_info(&self) {
        let sub = SubscriptionInfoDialog::new(self.selected_subscription().unwrap());
        sub.present(Some(self));
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
        let this = self.clone();
        self.error_boundary().spawn(async move {
            let sub = this.notifier().subscribe(&sub.server, &sub.topic).await?;
            let imp = this.imp();

            // Subscription::new will use the pipelined client to retrieve info about the subscription
            let subscription = Subscription::new(sub);
            // We want to still check if there were any errors adding the subscription.

            imp.subscription_list_model.append(&subscription);
            let i = imp.subscription_list_model.n_items() - 1;
            let row = imp.subscription_list.row_at_index(i as i32);
            imp.subscription_list.select_row(row.as_ref());
            Ok(())
        });
    }

    fn unsubscribe(&self) {
        let sub = self.selected_subscription().unwrap();

        let this = self.clone();
        self.error_boundary().spawn(async move {
            this.notifier()
                .unsubscribe(sub.server().as_str(), sub.topic().as_str())
                .await?;

            let imp = this.imp();
            if let Some(i) = imp.subscription_list_model.find(&sub) {
                imp.subscription_list_model.remove(i);
            }
            Ok(())
        });
    }
    fn notifier(&self) -> &NtfyHandle {
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

                Self::build_subscription_row(&sub).upcast()
            });

        let this = self.clone();
        imp.subscription_list.connect_row_selected(move |_, _row| {
            this.selected_subscription_changed(this.selected_subscription().as_ref());
        });

        let this = self.clone();
        self.error_boundary().spawn(async move {
            glib::timeout_future_seconds(1).await;
            let list = this.notifier().list_subscriptions().await?;
            for sub in list {
                this.imp()
                    .subscription_list_model
                    .append(&Subscription::new(sub));
            }
            Ok(())
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
        let this = self.clone();
        let set_sensitive = move |b| {
            let imp = this.imp();
            imp.subscription_menu_btn.set_sensitive(b);
            imp.code_btn.set_sensitive(b);
            imp.send_btn.set_sensitive(b);
            imp.entry.set_sensitive(b);
        };
        if let Some((sub, id)) = imp.banner_binding.take() {
            sub.disconnect(id);
        }
        if let Some(sub) = sub {
            set_sensitive(true);
            imp.navigation_split_view.set_show_content(true);
            imp.message_list
                .bind_model(Some(&sub.imp().messages), move |obj| {
                    let b = obj.downcast_ref::<glib::BoxedAnyObject>().unwrap();
                    let msg = b.borrow::<models::ReceivedMessage>();

                    MessageRow::new(msg.clone()).upcast()
                });

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
            set_sensitive(false);
            imp.message_list
                .bind_model(gio::ListModel::NONE, |_| adw::Bin::new().into());
        }
    }
    fn flag_read(&self) {
        let vadj = self.imp().message_scroll.vadjustment();
        // There is nothing to scroll, so the user viewed all the messages
        if vadj.page_size() == vadj.upper()
            || ((vadj.page_size() + vadj.value() - vadj.upper()).abs() <= 1.0)
        {
            self.selected_subscription().map(|sub| {
                self.error_boundary()
                    .spawn(async move { sub.flag_all_as_read().await });
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

    fn build_subscription_row(sub: &Subscription) -> impl IsA<gtk::Widget> {
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
