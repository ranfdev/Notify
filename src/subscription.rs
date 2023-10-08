use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use capnp::capability::Promise;
use capnp_rpc::pry;
use glib::once_cell::sync::Lazy;
use glib::subclass::prelude::*;
use glib::subclass::Signal;
use glib::Properties;
use gtk::{gio, glib};
use ntfy_daemon::models;
use ntfy_daemon::ntfy_capnp::{output_channel, subscription, watch_handle, Status};
use tracing::{debug, debug_span, error, instrument};

struct TopicWatcher {
    sub: glib::WeakRef<Subscription>,
}
impl output_channel::Server for TopicWatcher {
    fn send_message(
        &mut self,
        params: output_channel::SendMessageParams,
        _results: output_channel::SendMessageResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Some(sub) = self.sub.upgrade() {
            let request = pry!(params.get());
            let message = pry!(request.get_message());

            let msg: models::Message = serde_json::from_str(&message).unwrap();
            sub.imp().messages.append(&glib::BoxedAnyObject::new(msg));
            sub.update_unread_count();
            Promise::ok(())
        } else {
            Promise::err(capnp::Error::failed("dead channel".to_string()))
        }
    }
    fn send_status(
        &mut self,
        params: output_channel::SendStatusParams,
        _: output_channel::SendStatusResults,
    ) -> capnp::capability::Promise<(), capnp::Error> {
        if let Some(sub) = self.sub.upgrade() {
            let status = pry!(pry!(params.get()).get_status());
            sub.imp().status.set(status);
            sub.notify_status();
            Promise::ok(())
        } else {
            Promise::err(capnp::Error::failed("dead channel".to_string()))
        }
    }
}

impl Drop for TopicWatcher {
    fn drop(&mut self) {
        debug!("Dropped topic watcher");
    }
}

mod imp {
    use super::*;

    #[derive(Properties)]
    #[properties(wrapper_type = super::Subscription)]
    pub struct Subscription {
        #[property(get)]
        pub display_name: RefCell<String>,
        #[property(get)]
        pub topic: RefCell<String>,
        #[property(get)]
        pub url: RefCell<String>,
        #[property(get)]
        pub server: RefCell<String>,
        #[property(get = Self::get_status, type = u8)]
        pub status: Rc<Cell<Status>>,
        #[property(get)]
        pub muted: Cell<bool>,
        #[property(get)]
        pub unread_count: Cell<u32>,
        pub read_until: Cell<u64>,
        pub messages: gio::ListStore,
        pub client: OnceCell<subscription::Client>,
        pub remote_handle: RefCell<Option<watch_handle::Client>>,
    }

    impl Subscription {
        fn get_status(&self) -> u8 {
            let s: u16 = Cell::get(&self.status).into();
            s as u8
        }
    }

    impl Default for Subscription {
        fn default() -> Self {
            Self {
                display_name: Default::default(),
                topic: Default::default(),
                url: Default::default(),
                muted: Default::default(),
                server: Default::default(),
                status: Rc::new(Cell::new(Status::Down)),
                messages: gio::ListStore::new::<glib::BoxedAnyObject>(),
                client: Default::default(),
                unread_count: Default::default(),
                read_until: Default::default(),
                remote_handle: Default::default(),
            }
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for Subscription {
        fn signals() -> &'static [Signal] {
            static SIGNALS: Lazy<Vec<Signal>> =
                Lazy::new(|| vec![Signal::builder("awarded").build()]);
            SIGNALS.as_ref()
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Subscription {
        const NAME: &'static str = "TopicSubscription";
        type Type = super::Subscription;
    }
}

glib::wrapper! {
    pub struct Subscription(ObjectSubclass<imp::Subscription>);
}

impl Subscription {
    pub fn new(client: subscription::Client) -> Self {
        let this: Self = glib::Object::builder().build();
        let imp = this.imp();
        if let Err(_) = imp.client.set(client) {
            panic!();
        };

        let this_clone = this.clone();
        glib::MainContext::default().spawn_local(async move {
            match this_clone.load().await {
                Ok(_) => {}
                Err(e) => {
                    error!(error = %e, "loading subscription data");
                }
            }
        });
        this
    }

    fn init_info(
        &self,
        topic: &str,
        server: &str,
        muted: bool,
        read_until: u64,
        display_name: &str,
    ) {
        let imp = self.imp();
        imp.topic.replace(topic.to_string());
        self.notify_topic();
        imp.server.replace(server.to_string());
        self.notify_server();
        imp.muted.replace(muted);
        self.notify_muted();
        imp.read_until.replace(read_until);
        self.notify_unread_count();
        self._set_display_name(display_name.to_string());
    }

    fn load(&self) -> Promise<(), capnp::Error> {
        let imp = self.imp();
        let req_info = imp.client.get().unwrap().get_info_request();
        let req_messages = {
            let mut req = imp.client.get().unwrap().watch_request();
            req.get().set_watcher(capnp_rpc::new_client(TopicWatcher {
                sub: self.downgrade(),
            }));
            req
        };

        let this = self.clone();
        Promise::from_future(async move {
            let info = req_info.send().promise.await?;
            let info = info.get()?;
            this.init_info(
                info.get_topic()?,
                info.get_server()?,
                info.get_muted(),
                info.get_read_until(),
                info.get_display_name()?,
            );

            let message_stream = req_messages.send().promise.await?;
            let handle = message_stream.get()?.get_handle()?;
            this.imp().remote_handle.replace(Some(handle));
            Ok(())
        })
    }

    fn _set_display_name(&self, value: String) {
        let imp = self.imp();
        let value = if value.is_empty() {
            self.topic()
        } else {
            value
        };
        imp.display_name.replace(value);
        self.notify_display_name();
    }
    #[instrument(skip_all)]
    pub fn set_display_name(&self, value: String) -> Promise<(), capnp::Error> {
        let this = self.clone();
        Promise::from_future(async move {
            this._set_display_name(value);
            this.send_updated_info().await?;
            Ok(())
        })
    }

    fn send_updated_info(&self) -> Promise<(), capnp::Error> {
        let imp = self.imp();
        let mut req = imp.client.get().unwrap().update_info_request();
        let mut val = pry!(req.get().get_value());
        val.set_muted(imp.muted.get());
        val.set_display_name(&*imp.display_name.borrow());
        val.set_read_until(imp.read_until.get());
        Promise::from_future(async move {
            let _span = debug_span!("send_updated_info").entered();
            debug!("sending");
            req.send().promise.await?;
            Ok(())
        })
    }
    fn last_message(list: &gio::ListStore) -> Option<models::Message> {
        let n = list.n_items();
        let last = list
            .item(n.checked_sub(1)?)
            .and_downcast::<glib::BoxedAnyObject>()?;
        let last = last.borrow::<models::Message>();
        Some(last.clone())
    }
    fn update_unread_count(&self) {
        let imp = self.imp();
        if let Some(last) = Self::last_message(&imp.messages) {
            if last.time > imp.read_until.get() {
                imp.unread_count.set(1);
            } else {
                imp.unread_count.set(0);
            }
        } else {
            imp.unread_count.set(0);
        }
        self.notify_unread_count();
    }

    pub fn set_muted(&self, value: bool) -> Promise<(), capnp::Error> {
        let this = self.clone();
        Promise::from_future(async move {
            this.imp().muted.replace(value);
            this.notify_muted();
            this.send_updated_info().await?;
            Ok(())
        })
    }
    pub fn flag_all_as_read(&self) -> Promise<(), capnp::Error> {
        let imp = self.imp();
        let Some(last) = Self::last_message(&imp.messages) else {
            return Promise::ok(());
        };
        let value = last.time;

        let this = self.clone();
        Promise::from_future(async move {
            let mut req = this.imp().client.get().unwrap().update_read_until_request();
            req.get().set_value(value);
            req.send().promise.await?;
            this.imp().read_until.set(value);
            this.update_unread_count();
            Ok(())
        })
    }
    pub fn publish(&self, message: &str) -> Promise<(), capnp::Error> {
        let imp = self.imp();
        let mut req = imp.client.get().unwrap().publish_request();
        let msg = serde_json::to_string(&models::Message {
            topic: self.topic(),
            message: Some(message.to_string()),
            ..models::Message::default()
        })
        .map_err(|e| capnp::Error::failed(e.to_string()));

        req.get().set_message(&pry!(msg));

        Promise::from_future(async move {
            let _span = debug_span!("publish").entered();
            debug!("sending");
            req.send().promise.await?;
            Ok(())
        })
    }
    #[instrument(skip_all)]
    pub fn clear_notifications(&self) -> Promise<(), capnp::Error> {
        let imp = self.imp();
        let req = imp.client.get().unwrap().clear_notifications_request();
        let this = self.clone();
        Promise::from_future(async move {
            let _span = debug_span!("clear_notifications").entered();
            debug!("sending");
            req.send().promise.await?;
            this.imp().messages.remove_all();
            Ok(())
        })
    }

    pub fn nice_status(&self) -> Status {
        Status::try_from(self.imp().status.get() as u16).unwrap()
    }
}
