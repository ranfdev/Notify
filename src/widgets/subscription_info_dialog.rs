use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::Properties;
use gtk::gio;
use gtk::glib;

use crate::error::*;

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
        type ParentType = adw::Dialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
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

            self.display_name_entry
                .set_text(&this.subscription().unwrap().display_name());
            self.muted_switch_row
                .set_active(this.subscription().unwrap().muted());

            let debouncer = crate::async_utils::Debouncer::new();
            self.display_name_entry.connect_changed({
                move |entry| {
                    let entry = entry.clone();
                    let this = this.clone();
                    debouncer.call(std::time::Duration::from_millis(500), move || {
                        this.update_display_name(&entry);
                    })
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
    impl AdwDialogImpl for SubscriptionInfoDialog {}
}

glib::wrapper! {
    pub struct SubscriptionInfoDialog(ObjectSubclass<imp::SubscriptionInfoDialog>)
        @extends gtk::Widget, adw::Dialog,
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
            self.error_boundary().spawn(async move {
                let res = sub.set_display_name(entry.text().to_string()).await;
                res
            });
        }
    }
    fn update_muted(&self, switch: &adw::SwitchRow) {
        if let Some(sub) = self.subscription() {
            let switch = switch.clone();
            self.error_boundary()
                .spawn(async move { sub.set_muted(switch.is_active()).await })
        }
    }
}
