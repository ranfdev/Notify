use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::Properties;
use gtk::gio;
use gtk::glib;

use crate::widgets::*;

mod imp {
    pub use super::*;
    #[derive(Debug, Default, Properties, gtk::CompositeTemplate)]
    #[template(resource = "/com/ranfdev/Notify/ui/subscription_info_dialog.ui")]
    #[properties(wrapper_type = super::SubscriptionInfoDialog)]
    pub struct SubscriptionInfoDialog {
        #[property(get, construct_only)]
        pub subscription: RefCell<Option<crate::subscription::Subscription>>,
        #[template_child]
        pub display_name_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub muted_switch_row: TemplateChild<adw::SwitchRow>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SubscriptionInfoDialog {
        const NAME: &'static str = "SubscriptionInfoDialog";
        type Type = super::SubscriptionInfoDialog;
        type ParentType = adw::Window;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
            klass.add_binding_action(
                gtk::gdk::Key::Escape,
                gtk::gdk::ModifierType::empty(),
                "window.close",
                None,
            );
        }

        // You must call `Widget`'s `init_template()` within `instance_init()`.
        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }
    #[glib::derived_properties]
    impl ObjectImpl for SubscriptionInfoDialog {
        fn constructed(&self) {
            self.parent_constructed();
            let this = self.obj().clone();

            let (tx, rx) = glib::MainContext::channel(glib::Priority::default());
            let rx =
                crate::async_utils::debounce_channel(std::time::Duration::from_millis(500), rx);
            rx.attach(None, move |entry| {
                this.update_display_name(&entry);
                glib::ControlFlow::Continue
            });

            let this = self.obj().clone();
            self.display_name_entry
                .set_text(&this.subscription().unwrap().display_name());
            self.muted_switch_row
                .set_active(this.subscription().unwrap().muted());

            self.display_name_entry.connect_changed({
                move |entry| {
                    tx.send(entry.clone()).unwrap();
                }
            });
            let this = self.obj().clone();
            self.muted_switch_row.connect_active_notify({
                move |switch| {
                    this.update_muted(switch);
                }
            });
        }
    }
    impl WidgetImpl for SubscriptionInfoDialog {}
    impl WindowImpl for SubscriptionInfoDialog {}
    impl AdwWindowImpl for SubscriptionInfoDialog {}
}

glib::wrapper! {
    pub struct SubscriptionInfoDialog(ObjectSubclass<imp::SubscriptionInfoDialog>)
        @extends gtk::Widget, gtk::Window, adw::Window,
        @implements gio::ActionMap, gio::ActionGroup, gtk::Root;
}

impl SubscriptionInfoDialog {
    pub fn new(subscription: crate::subscription::Subscription) -> Self {
        let this = glib::Object::builder()
            .property("subscription", subscription)
            .build();
        this
    }
    fn update_display_name(&self, entry: &impl IsA<gtk::Editable>) {
        if let Some(sub) = self.subscription() {
            let entry = entry.clone();
            self.spawn_with_near_toast(async move {
                let res = sub.set_display_name(entry.text().to_string()).await;
                res
            });
        }
    }
    fn update_muted(&self, switch: &adw::SwitchRow) {
        if let Some(sub) = self.subscription() {
            let switch = switch.clone();
            self.spawn_with_near_toast(async move { sub.set_muted(switch.is_active()).await })
        }
    }
}
