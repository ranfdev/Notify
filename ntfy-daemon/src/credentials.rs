use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use async_trait::async_trait;

#[derive(Clone)]
pub struct KeyringItem {
    attributes: HashMap<String, String>,
    // we could zero-out this region of memory
    secret: Vec<u8> 
}

impl KeyringItem {
    async fn attributes(&self) -> HashMap<String, String> {
        self.attributes.clone()
    }
    async fn secret(&self) -> &[u8] {
        &self.secret[..]
    }
}

#[async_trait]
trait LightKeyring {
    async fn search_items(
        &self,
        attributes: HashMap<&str, &str>,
    ) -> anyhow::Result<Vec<KeyringItem>>;
    async fn create_item(
        &self,
        label: &str,
        attributes: HashMap<&str, &str>,
        secret: &str,
        replace: bool,
    ) -> anyhow::Result<()>;
    async fn delete(&self, attributes: HashMap<&str, &str>) -> anyhow::Result<()>;
}

struct RealKeyring {
    keyring: oo7::Keyring,
}

#[async_trait]
impl LightKeyring for RealKeyring {
    async fn search_items(
        &self,
        attributes: HashMap<&str, &str>,
    ) -> anyhow::Result<Vec<KeyringItem>> {
        let items = self.keyring.search_items(attributes).await?;

        let mut out_items = vec![];
        for item in items {
            out_items.push(KeyringItem {
                attributes: item.attributes().await?,
                secret: item.secret().await?.to_vec(),
            });
        }
        Ok(out_items)
    }

    async fn create_item(
        &self,
        label: &str,
        attributes: HashMap<&str, &str>,
        secret: &str,
        replace: bool,
    ) -> anyhow::Result<()> {
        self.keyring
            .create_item(label, attributes, secret, replace)
            .await?;
        Ok(())
    }

    async fn delete(&self, attributes: HashMap<&str, &str>) -> anyhow::Result<()> {
        self.keyring.delete(attributes).await?;
        Ok(())
    }
}

struct NullableKeyring {
    search_response: Vec<KeyringItem>,
}

impl NullableKeyring {
    pub fn new(search_response: Vec<KeyringItem>) -> Self {
        Self { search_response }
    }
}

#[async_trait]
impl LightKeyring for NullableKeyring {
    async fn search_items(
        &self,
        _attributes: HashMap<&str, &str>,
    ) -> anyhow::Result<Vec<KeyringItem>> {
        Ok(self.search_response.clone())
    }

    async fn create_item(
        &self,
        _label: &str,
        _attributes: HashMap<&str, &str>,
        _secret: &str,
        _replace: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn delete(&self, _attributes: HashMap<&str, &str>) -> anyhow::Result<()> {
        Ok(())
    }
}
impl NullableKeyring {
    pub fn with_credentials(credentials: Vec<Credential>) -> Self {
        let mut search_response = vec![];

        for cred in credentials {
            let attributes = HashMap::from([
                ("type".to_string(), "password".to_string()),
                ("username".to_string(), cred.username.clone()),
                ("server".to_string(), cred.password.clone()),
            ]);
            search_response.push(KeyringItem { attributes, secret: cred.password.into_bytes() });
        }

        Self { search_response }
    }
}

#[derive(Debug, Clone)]
pub struct Credential {
    pub username: String,
    pub password: String,
}

#[derive(Clone)]
pub struct Credentials {
    keyring: Rc<dyn LightKeyring>,
    creds: Rc<RefCell<HashMap<String, Credential>>>,
}

impl Credentials {
    pub async fn new() -> anyhow::Result<Self> {
        let mut this = Self {
            keyring: Rc::new(RealKeyring {
                keyring: oo7::Keyring::new()
                    .await
                    .expect("Failed to start Secret Service"),
            }),
            creds: Default::default(),
        };
        this.load().await?;
        Ok(this)
    }
    pub async fn new_nullable(credentials: Vec<Credential>) -> anyhow::Result<Self> {
        let mut this = Self {
            keyring: Rc::new(NullableKeyring::with_credentials(credentials)),
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
                .await;
            self.creds.borrow_mut().insert(
                attrs["server"].to_string(),
                Credential {
                    username: attrs["username"].to_string(),
                    password: std::str::from_utf8(&item.secret().await)?.to_string(),
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
