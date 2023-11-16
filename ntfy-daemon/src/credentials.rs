use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct Credential {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct Credentials {
    keyring: Rc<oo7::Keyring>,
    creds: Rc<RefCell<HashMap<String, Credential>>>,
}

impl Credentials {
    pub async fn new() -> anyhow::Result<Self> {
        let mut this = Self {
            keyring: Rc::new(
                oo7::Keyring::new()
                    .await
                    .expect("Failed to start Secret Service"),
            ),
            creds: Default::default(),
        };
        this.load().await?;
        Ok(this)
    }
    pub async fn load(&mut self) -> anyhow::Result<()> {
        let attrs = HashMap::from([("type", "password")]);
        let values = self
            .keyring
            .search_items(attrs)
            .await
            .map_err(|e| capnp::Error::failed(e.to_string()))?;

        self.creds.borrow_mut().clear();
        for item in values {
            let attrs = item
                .attributes()
                .await
                .map_err(|e| capnp::Error::failed(e.to_string()))?;
            self.creds.borrow_mut().insert(
                attrs["server"].to_string(),
                Credential {
                    username: attrs["username"].to_string(),
                    password: std::str::from_utf8(&item.secret().await?)?.to_string(),
                },
            );
        }
        Ok(())
    }
    pub fn get(&self, server: &str) -> Option<Credential> {
        self.creds.borrow().get(server).cloned()
    }
    pub fn list_all(&self) -> HashMap<String, Credential> {
        self.creds.borrow().clone()
    }
    pub async fn insert(&self, server: &str, username: &str, password: &str) -> anyhow::Result<()> {
        {
            if let Some(cred) = self.creds.borrow().get(server) {
                if cred.username != username {
                    anyhow::bail!("You can add only one account per server");
                }
            }
        }
        let attrs = HashMap::from([
            ("type", "password"),
            ("username", username),
            ("server", server),
        ]);
        self.keyring
            .create_item("Password", attrs, password, true)
            .await
            .map_err(|e| capnp::Error::failed(e.to_string()))?;

        self.creds.borrow_mut().insert(
            server.to_string(),
            Credential {
                username: username.to_string(),
                password: password.to_string(),
            },
        );
        Ok(())
    }
    pub async fn delete(&self, server: &str) -> anyhow::Result<()> {
        let creds = {
            self.creds
                .borrow()
                .get(server)
                .ok_or(anyhow::anyhow!("server creds not found"))?
                .clone()
        };
        let attrs = HashMap::from([
            ("type", "password"),
            ("username", &creds.username),
            ("server", server),
        ]);
        self.keyring
            .delete(attrs)
            .await
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        self.creds
            .borrow_mut()
            .remove(server)
            .ok_or(anyhow::anyhow!("server creds not found"))?;
        Ok(())
    }
}
