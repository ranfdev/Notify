use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::once_cell::sync::Lazy;
use glib::subclass::Signal;
use glib::Properties;
use gtk::gio;
use gtk::glib;

mod imp {
    pub use super::*;
    #[derive(Debug, Default, Properties)]
    #[properties(wrapper_type = super::AddSubscriptionDialog)]
    pub struct AddSubscriptionDialog {
        #[property(name = "topic", get = |imp: &Self| imp.topic_entry.text(), type = glib::GString)]
        pub topic_entry: adw::EntryRow,
        #[property(name = "server", get = |imp: &Self| imp.server_entry.text(), type = glib::GString)]
        pub server_entry: adw::EntryRow,
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
    #[glib::derived_properties]
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

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&adw::HeaderBar::new());

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_end(12)
            .margin_start(12)
            .margin_top(12)
            .margin_bottom(12)
            .build();
        let clamp = adw::Clamp::new();
        clamp.set_child(Some(&content));

        let description = {
            let d = gtk::Label::builder()
                    .label("Topics may not be password-protected, so choose a name that's not easy to guess. Once subscribed, you can PUT/POST notifications.")
                    .wrap(true)
                    .xalign(0.0)
                    .wrap_mode(gtk::pango::WrapMode::WordChar)
                    .build();
            d.add_css_class("dim-label");
            d
        };

        content.append(&description);

        let topic_entry = {
            let e = &imp.topic_entry;
            e.set_title("Topic");
            e.set_activates_default(true);

            let rand_btn = {
                let b = gtk::Button::builder()
                    .icon_name("dice3-symbolic")
                    .tooltip_text("Generate Name")
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                let ec = e.clone();
                b.connect_clicked(move |_| {
                    use rand::distributions::Alphanumeric;
                    use rand::{thread_rng, Rng};
                    let mut rng = thread_rng();
                    let chars: String = (0..10).map(|_| rng.sample(Alphanumeric) as char).collect();
                    ec.set_text(&chars);
                });
                b
            };

            e.add_suffix(&rand_btn);
            e
        };
        // TODO: Reserved topics
        /*let reserved_switch = {
            adw::SwitchRow::builder()
                .title("Reserved")
                .subtitle("For Ntfy Pro users only")
                .build()
        };*/
        let server_entry = &imp.server_entry;
        server_entry.set_title("Server");

        let expander_row = {
            let e = adw::ExpanderRow::builder()
                .title("Custom Server...")
                .enable_expansion(false)
                .show_enable_switch(true)
                .build();
            e.add_row(server_entry);
            e
        };
        let list_box = {
            let l = gtk::ListBox::new();
            l.add_css_class("boxed-list");
            l.append(topic_entry);
            // l.append(&reserved_switch);
            l.append(&expander_row);
            l
        };
        content.append(&list_box);

        let sub_btn = {
            let b = gtk::Button::new();
            b.set_label("Subscribe");
            b.add_css_class("suggested-action");
            b.add_css_class("pill");
            b.set_halign(gtk::Align::Center);

            let wc = obj.clone();
            b.connect_clicked(move |_| {
                wc.emit_subscribe_request();
                wc.close();
            });
            b
        };

        content.append(&sub_btn);
        toolbar_view.set_content(Some(&clamp));

        obj.set_content(Some(&toolbar_view));
    }
    fn emit_subscribe_request(&self) {
        self.emit_by_name::<()>("subscribe-request", &[]);
    }
}
