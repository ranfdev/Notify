use std::cell::OnceCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};
use ntfy_daemon::ntfy_capnp::system_notifier;

use crate::error::*;

mod imp {
    use super::*;

    #[derive(gtk::CompositeTemplate)]
    #[template(resource = "/com/ranfdev/Notify/ui/preferences.ui")]
    pub struct NotifyPreferences {
        #[template_child]
        pub server_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub username_entry: TemplateChild<adw::EntryRow>,
        #[template_child]
        pub password_entry: TemplateChild<adw::PasswordEntryRow>,
        #[template_child]
        pub add_btn: TemplateChild<gtk::Button>,
        #[template_child]
        pub added_accounts: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub added_accounts_group: TemplateChild<adw::PreferencesGroup>,
        pub notifier: OnceCell<system_notifier::Client>,
    }

    impl Default for NotifyPreferences {
        fn default() -> Self {
            let this = Self {
                server_entry: Default::default(),
                username_entry: Default::default(),
                password_entry: Default::default(),
                add_btn: Default::default(),
                added_accounts: Default::default(),
                added_accounts_group: Default::default(),
                notifier: Default::default(),
            };

            this
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NotifyPreferences {
        const NAME: &'static str = "NotifyPreferences";
        type Type = super::NotifyPreferences;
        type ParentType = adw::PreferencesDialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for NotifyPreferences {
        fn dispose(&self) {
            self.dispose_template();
        }
    }

    impl WidgetImpl for NotifyPreferences {}
    impl AdwDialogImpl for NotifyPreferences {}
    impl PreferencesDialogImpl for NotifyPreferences {}
}

glib::wrapper! {
    pub struct NotifyPreferences(ObjectSubclass<imp::NotifyPreferences>)
        @extends gtk::Widget, adw::Dialog, adw::PreferencesDialog,
        @implements gio::ActionMap, gio::ActionGroup, gtk::Root;
}

impl NotifyPreferences {
    pub fn new(notifier: system_notifier::Client) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp()
            .notifier
            .set(notifier)
            .map_err(|_| "notifier")
            .unwrap();
        let this = obj.clone();
        obj.imp().add_btn.connect_clicked(move |btn| {
            let this = this.clone();
            btn.error_boundary()
                .spawn(async move { this.add_account().await });
        });
        let this = obj.clone();
        obj.imp()
            .added_accounts
            .error_boundary()
            .spawn(async move { this.show_accounts().await });
        obj
    }

    pub async fn show_accounts(&self) -> anyhow::Result<()> {
        let imp = self.imp();
        let req = imp.notifier.get().unwrap().list_accounts_request();
        let res = req.send().promise.await?;

        let accounts = res.get()?.get_list()?;

        imp.added_accounts_group.set_visible(!accounts.is_empty());

        imp.added_accounts.remove_all();
        for a in accounts {
            let server = a.get_server()?.to_string()?;
            let username = a.get_username()?.to_string()?;

            let row = adw::ActionRow::builder()
                .title(&server)
                .subtitle(&username)
                .build();
            row.add_css_class("property");
            row.add_suffix(&{
                let btn = gtk::Button::builder()
                    .icon_name("user-trash-symbolic")
                    .build();
                btn.add_css_class("flat");
                let this = self.clone();
                btn.connect_clicked(move |btn| {
                    let this = this.clone();
                    let username = username.clone();
                    let server = server.clone();
                    btn.error_boundary()
                        .spawn(async move { this.remove_account(&server, &username).await });
                });
                btn
            });
            imp.added_accounts.append(&row);
        }
        Ok(())
    }
    pub async fn add_account(&self) -> anyhow::Result<()> {
        let imp = self.imp();
        let password = imp.password_entry.text();
        let server = imp.server_entry.text();
        let username = imp.username_entry.text();

        let mut req = imp.notifier.get().unwrap().add_account_request();
        let mut acc = req.get().get_account()?;
        acc.set_username(username[..].into());
        acc.set_server(server[..].into());
        req.get().set_password(password[..].into());

        req.send().promise.await?;

        self.show_accounts().await?;

        Ok(())
    }
    pub async fn remove_account(&self, server: &str, username: &str) -> anyhow::Result<()> {
        let mut req = self.imp().notifier.get().unwrap().remove_account_request();
        let mut acc = req.get().get_account()?;

        acc.set_username(username[..].into());
        acc.set_server(server[..].into());

        req.send().promise.await?;

        self.show_accounts().await?;

        Ok(())
    }
}
