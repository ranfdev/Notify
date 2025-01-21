use std::{cell::RefCell, rc::Rc, sync::Arc};

use tokio::sync::RwLock;

#[derive(Clone)]
pub struct OutputTracker<T> {
    store: Rc<RefCell<Option<Vec<T>>>>,
}

impl<T> Default for OutputTracker<T> {
    fn default() -> Self {
        Self {
            store: Default::default(),
        }
    }
}

impl<T: Clone> OutputTracker<T> {
    pub fn enable(&self) {
        let mut inner = self.store.borrow_mut();
        if inner.is_none() {
            *inner = Some(vec![]);
        }
    }
    pub fn push(&self, item: T) {
        if let Some(v) = &mut *self.store.borrow_mut() {
            v.push(item);
        }
    }
    pub fn items(&self) -> Vec<T> {
        if let Some(v) = &*self.store.borrow() {
            v.clone()
        } else {
            vec![]
        }
    }
}

#[derive(Clone)]
pub struct OutputTrackerAsync<T> {
    store: Arc<RwLock<Option<Vec<T>>>>,
}

impl<T> Default for OutputTrackerAsync<T> {
    fn default() -> Self {
        Self {
            store: Default::default(),
        }
    }
}

impl<T: Clone> OutputTrackerAsync<T> {
    pub async fn enable(&self) {
        let mut inner = self.store.write().await;
        if inner.is_none() {
            *inner = Some(vec![]);
        }
    }
    pub async fn push(&self, item: T) {
        if let Some(v) = &mut *self.store.write().await {
            v.push(item);
        }
    }
    pub async fn items(&self) -> Vec<T> {
        if let Some(v) = &*self.store.read().await {
            v.clone()
        } else {
            vec![]
        }
    }
}
