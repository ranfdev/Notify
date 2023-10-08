use std::{cell::RefCell, rc::Rc};

use rusqlite::{params, Connection, Result};
use tracing::info;

use crate::models;
use crate::Error;

#[derive(Clone, Debug)]
pub struct Db {
    conn: Rc<RefCell<Connection>>,
}

impl Db {
    pub fn connect(path: &str) -> Result<Self> {
        let mut this = Self {
            conn: Rc::new(RefCell::new(Connection::open(path)?)),
        };
        {
            this.conn.borrow().execute_batch(
                "PRAGMA foreign_keys = ON;
        PRAGMA journal_mode = wal;",
            )?;
        }
        this.migrate()?;
        Ok(this)
    }
    fn migrate(&mut self) -> Result<()> {
        {
            self.conn
                .borrow()
                .execute_batch(include_str!("./migrations/00.sql"))?
        };
        Ok(())
    }
    fn get_or_insert_server(&mut self, server: &str) -> Result<i64> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        let mut res = tx.query_row(
            "SELECT id
                FROM server
                WHERE endpoint = ?1",
            params![server,],
            |row| {
                let id: i64 = row.get(0)?;
                Ok(id)
            },
        );
        if let Err(rusqlite::Error::QueryReturnedNoRows) = res {
            tx.execute(
                "INSERT INTO server (id, endpoint) VALUES (NULL, ?1)",
                params![server,],
            )?;
            res = Ok(tx.last_insert_rowid());
        }
        tx.commit()?;
        res
    }
    pub fn insert_message(&mut self, server: &str, json_data: &str) -> Result<(), Error> {
        let server_id = self.get_or_insert_server(server)?;
        let res = self.conn.borrow().execute(
            "INSERT INTO message (server, data) VALUES (?1, ?2)",
            params![server_id, json_data],
        );
        match res {
            Err(rusqlite::Error::SqliteFailure(_, Some(text)))
                if text.starts_with("UNIQUE constraint failed") =>
            {
                Err(Error::DuplicateMessage)
            }
            Err(e) => Err(Error::Db(e)),
            Ok(_) => Ok(()),
        }
    }
    pub fn list_messages(
        &self,
        server: &str,
        topic: &str,
        since: u64,
    ) -> Result<Vec<String>, rusqlite::Error> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "
            SELECT data
            FROM subscription sub
            JOIN server s ON sub.server = s.id
            JOIN message m ON m.server = sub.server AND m.topic = sub.topic
            WHERE s.endpoint = ?1 AND m.topic = ?2 AND m.data ->> 'time' >= ?3
            ORDER BY m.data ->> 'time'
        ",
        )?;
        let msgs: Result<Vec<String>, _> = stmt
            .query_map(params![server, topic, since], |row| Ok(row.get(0)?))?
            .collect();
        Ok(msgs?)
    }
    pub fn insert_subscription(&mut self, sub: models::Subscription) -> Result<(), Error> {
        let server_id = self.get_or_insert_server(&sub.server)?;
        self.conn.borrow().execute(
            "INSERT INTO subscription (server, topic, display_name, reserved, muted, archived) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                server_id,
                sub.topic,
                sub.display_name,
                sub.reserved,
                sub.muted,
                sub.archived
            ],
        )?;
        Ok(())
    }
    pub fn remove_subscription(&mut self, server: &str, topic: &str) -> Result<(), Error> {
        let server_id = self.get_or_insert_server(server)?;
        let res = self.conn.borrow().execute(
            "DELETE FROM subscription
            WHERE server = ?1 AND topic = ?2",
            params![server_id, topic],
        )?;
        if res <= 0 {
            return Err(Error::SubscriptionNotFound("removing subscription".into()));
        }
        Ok(())
    }
    pub fn list_subscriptions(&mut self) -> Result<Vec<models::Subscription>, Error> {
        let conn = self.conn.borrow();
        let mut stmt = conn.prepare(
            "SELECT server.endpoint, sub.topic, sub.display_name, sub.reserved, sub.muted, sub.archived, sub.symbolic_icon, sub.read_until
            FROM subscription sub
            JOIN server ON server.id = sub.server
            ORDER BY server.endpoint, sub.display_name, sub.topic
            ",
        )?;
        let rows = stmt.query_map(params![], |row| {
            Ok(models::Subscription {
                server: row.get(0)?,
                topic: row.get(1)?,
                display_name: row.get(2)?,
                reserved: row.get(3)?,
                muted: row.get(4)?,
                archived: row.get(5)?,
                symbolic_icon: row.get(6)?,
                read_until: row.get(7)?,
            })
        })?;
        let subs: Result<Vec<_>, rusqlite::Error> = rows.collect();
        Ok(subs?)
    }

    pub fn update_subscription(&mut self, sub: models::Subscription) -> Result<(), Error> {
        let server_id = self.get_or_insert_server(&sub.server)?;
        let res = self.conn.borrow().execute(
            "UPDATE subscription
            SET display_name = ?1, reserved = ?2, muted = ?3, archived = ?4, read_until = ?5
            WHERE server = ?6 AND topic = ?7",
            params![
                sub.display_name,
                sub.reserved,
                sub.muted,
                sub.archived,
                sub.read_until,
                server_id,
                sub.topic,
            ],
        )?;
        if res <= 0 {
            return Err(Error::SubscriptionNotFound("updating subscription".into()));
        }
        info!(info = ?sub, "stored subscription info");
        Ok(())
    }

    pub fn update_read_until(
        &mut self,
        server: &str,
        topic: &str,
        value: u64,
    ) -> Result<(), Error> {
        let server_id = self.get_or_insert_server(server).unwrap();
        let conn = self.conn.borrow();
        let res = conn.execute(
            "UPDATE subscription
            SET read_until = ?3
            WHERE topic = ?2 AND server = ?1
            ",
            params![server_id, topic, value],
        )?;
        if res <= 0 {
            return Err(Error::SubscriptionNotFound("updating read_until".into()));
        }
        Ok(())
    }
    pub fn delete_messages(&mut self, server: &str, topic: &str) -> Result<(), Error> {
        let server_id = self.get_or_insert_server(server).unwrap();
        let conn = self.conn.borrow();
        let res = conn.execute(
            "DELETE FROM message
            WHERE topic = ?2 AND server = ?1
            ",
            params![server_id, topic],
        )?;
        if res <= 0 {
            return Err(Error::SubscriptionNotFound("deleting messages".into()));
        }
        Ok(())
    }
}
