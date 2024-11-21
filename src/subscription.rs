use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use capnp::capability::Promise;
use capnp_rpc::pry;
use futures::join;
use glib::subclass::prelude::*;
use glib::Properties;
use gtk::glib::MainContext;
use gtk::{gio, glib};
use ntfy_daemon::{models, ConnectionState, ListenerEvent};
use tracing::{debug, error, instrument};

#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Down = 0,
    Degraded = 1,
    Up = 2,
}

impl From<u16> for Status {
    fn from(value: u16) -> Self {
        match value {
            0 => Status::Down,
            1 => Status::Degraded,
            2 => Status::Up,
            _ => panic!("Invalid value for Status"),
        }
    }
}

impl From<Status> for u16 {
    fn from(status: Status) -> Self {
        status as u16
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
        pub client: OnceCell<ntfy_daemon::SubscriptionHandle>,
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
            }
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for Subscription {}

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
    pub fn new(client: ntfy_daemon::SubscriptionHandle) -> Self {
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

        let this = self.clone();
        Promise::from_future(async move {
            let remote_subscription = this.imp().client.get().unwrap();
            let model = remote_subscription.model().await;

            this.init_info(
                &model.topic,
                &model.server,
                model.muted,
                model.read_until,
                &model.display_name,
            );

            let (prev_msgs, mut rx) = remote_subscription.attach().await;

            for msg in prev_msgs {
                this.handle_event(msg);
            }

            while let Ok(ev) = rx.recv().await {
                this.handle_event(ev);
            }
            Ok(())
        })
    }

    fn handle_event(&self, ev: ListenerEvent) {
        match dbg!(ev) {
            ListenerEvent::Message(msg) => {
                self.imp().messages.append(&glib::BoxedAnyObject::new(msg));
                self.update_unread_count();
            }
            ListenerEvent::ConnectionStateChanged(connection_state) => {
                self.set_connection_state(connection_state);
            }
        }
    }

    fn set_connection_state(&self, state: ConnectionState) {
        let status = match state {
            ConnectionState::Unitialized => Status::Degraded,
            ConnectionState::Connected => Status::Up,
            ConnectionState::Reconnecting { .. } => Status::Degraded,
        };
        self.imp().status.set(status);
        dbg!(status);
        self.notify_status();
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
    pub fn set_display_name(&self, value: String) -> Promise<(), anyhow::Error> {
        let this = self.clone();
        Promise::from_future(async move {
            this._set_display_name(value);
            this.send_updated_info().await?;
            Ok(())
        })
    }

    async fn send_updated_info(&self) -> anyhow::Result<()> {
        let imp = self.imp();
        imp.client
            .get()
            .unwrap()
            .update_info(
                models::Subscription::builder(self.topic())
                    .display_name((imp.display_name.borrow().to_string()))
                    .muted(imp.muted.get())
                    .build()
                    .map_err(|e| anyhow::anyhow!("invalid subscription data"))?,
            )
            .await?;
        Ok(())
    }
    fn last_message(list: &gio::ListStore) -> Option<models::ReceivedMessage> {
        let n = list.n_items();
        let last = list
            .item(n.checked_sub(1)?)
            .and_downcast::<glib::BoxedAnyObject>()?;
        let last = last.borrow::<models::ReceivedMessage>();
        Some(last.clone())
    }
    fn update_unread_count(&self) {
        let imp = self.imp();
        if Self::last_message(&imp.messages).map(|last| last.time) > Some(imp.read_until.get()) {
            imp.unread_count.set(1);
        } else {
            imp.unread_count.set(0);
        }
        self.notify_unread_count();
    }

    pub fn set_muted(&self, value: bool) -> Promise<(), anyhow::Error> {
        let this = self.clone();
        Promise::from_future(async move {
            this.imp().muted.replace(value);
            this.notify_muted();
            this.send_updated_info().await?;
            Ok(())
        })
    }
    pub async fn flag_all_as_read(&self) -> anyhow::Result<()> {
        let imp = self.imp();
        let Some(value) = Self::last_message(&imp.messages)
            .map(|last| last.time)
            .filter(|time| *time > self.imp().read_until.get())
        else {
            return Ok(());
        };

        let this = self.clone();
        this.imp()
            .client
            .get()
            .unwrap()
            .update_read_until(value)
            .await?;
        this.imp().read_until.set(value);
        this.update_unread_count();

        Ok(())
    }
    pub async fn publish_msg(&self, mut msg: models::OutgoingMessage) -> anyhow::Result<()> {
        let imp = self.imp();
        let json = {
            msg.topic = self.topic();
            serde_json::to_string(&msg)?
        };
        imp.client.get().unwrap().publish(json).await?;
        Ok(())
    }
    #[instrument(skip_all)]
    pub async fn clear_notifications(&self) -> anyhow::Result<()> {
        let imp = self.imp();
        imp.client.get().unwrap().clear_notifications().await?;
        self.imp().messages.remove_all();

        Ok(())
    }

    pub fn nice_status(&self) -> Status {
        Status::try_from(self.imp().status.get() as u16).unwrap()
    }
}
